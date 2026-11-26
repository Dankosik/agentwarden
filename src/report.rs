//! One pass of agentwarden and the report it prints.

use std::{fmt, fmt::Write as _, io, io::Write as _};

use compact_str::CompactString;
use serde::Serialize;

use crate::{
    actions::{self, System},
    error::AppError,
    model::{AgentKind, Pressure, SessionMode, Snapshot, Swap},
    owners::{DEFAULT_ANTIGRAVITY_DATA_DIR, Ownership},
    rules::{self, Action},
    state::{PendingRelaunch, RELAUNCH_WINDOW_SECS, State},
    store::{FileStamp, Outcome, Record, StateCache, Store},
};

const RECENT_RECORDS: usize = 10;

/// The documented `--format json` schema of `status`, `reclaim` and `watch`.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The agentwarden version that produced the report.
    pub version: &'static str,
    pub taken_at: u64,
    /// Pressure the rules act on: the kernel's level, raised by swap trends.
    pub pressure: Pressure,
    pub kernel_pressure: Pressure,
    pub swap: Swap,
    pub user_idle_secs: Option<u64>,
    pub claude_sessions: Vec<SessionReport>,
    pub codex: Option<CodexReport>,
    /// All dynamically discovered desktop agent apps (Codex/ChatGPT, Antigravity/clones, OpenCode).
    pub desktop_apps: Vec<DesktopAppReport>,
    /// Standalone CLI, background service, and nested subagent sessions (Codex, OpenCode, Antigravity).
    pub agent_sessions: Vec<AgentSessionReport>,
    pub orphans: Vec<HelperReport>,
    /// Actions the rules choose now.
    pub planned: Vec<Action>,
    /// What this run did; empty for `status` and `--dry-run`.
    pub applied: Vec<Record>,
    /// The newest records of the action log.
    pub recent: Vec<Record>,
}

#[derive(Debug, Serialize)]
pub struct SessionReport {
    pub pid: i32,
    /// `None` until the session was sampled twice.
    pub idle_secs: Option<u64>,
    pub helpers_footprint_bytes: u64,
    pub helpers: Vec<HelperReport>,
}

#[derive(Debug, Serialize)]
pub struct CodexReport {
    pub app_pid: i32,
    pub idle_secs: u64,
    /// Codex runtimes with everything under them; what a restart reclaims.
    pub footprint_bytes: u64,
    pub pools_footprint_bytes: u64,
    pub pools: Vec<HelperReport>,
    /// Running shells and commands; while any runs, Codex is not restarted.
    pub commands: Vec<HelperReport>,
}

#[derive(Debug, Serialize)]
pub struct DesktopAppReport {
    pub agent: AgentKind,
    pub app_name: CompactString,
    pub bundle_path: CompactString,
    pub app_pid: i32,
    /// Idle duration (`None` for generic/unknown tools without an activity probe).
    pub idle_secs: Option<u64>,
    /// What restarting this app reclaims (runtimes and their helpers; for OpenCode, app + service;
    /// for generic/other tools, the MCP helper pools).
    pub footprint_bytes: u64,
    pub pools_footprint_bytes: u64,
    pub pools: Vec<HelperReport>,
    /// Running shells, TTY commands, nested agents, and non-pool children.
    pub commands: Vec<HelperReport>,
}

#[derive(Debug, Serialize)]
pub struct AgentSessionReport {
    pub agent: AgentKind,
    pub mode: SessionMode,
    pub pid: i32,
    /// Process executable name for generic (`other`) CLI tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent: Option<CompactString>,
    /// CPU-based idle duration (tracked for background services; `None` for CLI/subagent runs).
    pub idle_secs: Option<u64>,
    pub footprint_bytes: u64,
    pub helpers_footprint_bytes: u64,
    pub helpers: Vec<HelperReport>,
    pub commands: Vec<HelperReport>,
}

#[derive(Debug, Serialize)]
pub struct HelperReport {
    pub pid: i32,
    pub exe_name: CompactString,
    /// Footprint of the helper and its descendants.
    pub footprint_bytes: u64,
    pub age_secs: u64,
}

impl Report {
    pub fn has_failures(&self) -> bool {
        self.applied
            .iter()
            .any(|record| record.outcome == Outcome::Failed)
    }
}

/// Sample, plan, and with `apply` act, log, and remember.
pub fn pass(system: &dyn System, store: &Store, apply: bool) -> Result<Report, AppError> {
    let (mut state, state_persisted) = store.load_state_with_status();
    let mut snapshot = system.begin_pass(&state.known_helpers, true)?;
    let ownership = Ownership::new(&snapshot, &state.known_helpers);

    if ownership
        .desktop_apps
        .iter()
        .any(|app| app.agent == AgentKind::Codex)
    {
        snapshot.codex_activity_at = system.codex_activity_at(&snapshot, None);
    }
    let mut probed_ag_dirs: smallvec::SmallVec<[CompactString; 2]> = smallvec::SmallVec::new();
    for app in &ownership.desktop_apps {
        if app.agent == AgentKind::Antigravity {
            let dir = app
                .data_dir
                .as_deref()
                .unwrap_or(DEFAULT_ANTIGRAVITY_DATA_DIR);
            if !probed_ag_dirs.iter().any(|existing| existing == dir) {
                probed_ag_dirs.push(CompactString::from(dir));
            }
        }
    }
    for dir in &probed_ag_dirs {
        let activity = system.antigravity_activity_at_dir(&snapshot, dir, None);
        snapshot
            .antigravity_activities
            .insert(dir.clone(), activity);
        if dir == DEFAULT_ANTIGRAVITY_DATA_DIR {
            snapshot.antigravity_activity_at = activity;
        }
    }
    if ownership
        .desktop_apps
        .iter()
        .any(|app| app.agent == AgentKind::Opencode)
        || ownership
            .agent_sessions
            .iter()
            .any(|s| s.agent == AgentKind::Opencode && s.mode == SessionMode::Service)
    {
        snapshot.opencode_activity_at = system.opencode_activity_at(&snapshot);
    }

    let need_footprints = !ownership.orphans.is_empty()
        || ownership
            .claude_sessions
            .iter()
            .any(|session| !session.helpers.is_empty())
        || ownership.desktop_apps.iter().any(|app| {
            app.agent != AgentKind::Codex
                || !app.runtimes.is_empty()
                || !app.pools.is_empty()
                || !app.commands.is_empty()
        })
        || !ownership.agent_sessions.is_empty();
    if need_footprints {
        if apply {
            if let Err(error) = system.resolve_footprints(&mut snapshot) {
                let _ = writeln!(
                    io::stderr(),
                    "footprint probe failed (continuing without footprints): {}",
                    crate::error::diagnostic(&error)
                );
            }
        } else {
            system.resolve_footprints(&mut snapshot)?;
        }
    }
    let planned = rules::plan(&snapshot, &ownership, &state);
    let report_without_actions = build(&snapshot, &ownership, &state, planned);
    if !apply {
        return Ok(Report {
            recent: store.recent(RECENT_RECORDS),
            ..report_without_actions
        });
    }
    let (applied, _) = apply_and_persist(
        system,
        store,
        &snapshot,
        &ownership,
        &mut state,
        state_persisted,
        &report_without_actions.planned,
    )?;
    Ok(Report {
        applied,
        recent: store.recent(RECENT_RECORDS),
        ..report_without_actions
    })
}

/// Watch-loop pass: applies the exact same rules and persistence as `pass(..., true)`,
/// reuses `state_cache` across passes when `state.json` is unchanged on disk,
/// skips building a discarded `Report` and reading `actions.jsonl`, and
/// resolves HID idle, agent activity, and `top` footprints only when a rule
/// actually depends on them.
///
/// A `/usr/bin/top` timeout or failure never aborts `orphan`, `idle-claude-session`,
/// pressure-idle `idle-opencode-service`, or `state.observe()`: rules that require
/// footprint (`idle-*-restart`) conservatively skip when footprints are unavailable.
pub fn watch_pass_cached(
    system: &dyn System,
    store: &Store,
    state_cache: &mut StateCache,
) -> Result<(), AppError> {
    let (state, state_persisted) = state_cache.load_mut(store);
    let mut snapshot = system.begin_pass(&state.known_helpers, false)?;
    let ownership = Ownership::new(&snapshot, &state.known_helpers);

    let needs = rules::idle_probe_needs(&snapshot, &ownership, state);
    let mut opencode_probed = false;
    if needs.opencode_service_pressure_idle {
        snapshot.opencode_activity_at = system.opencode_activity_at(&snapshot);
        opencode_probed = true;
    }

    let need_stop = rules::has_stop_candidates(&snapshot, &ownership, state);
    let mut need_footprint_gated_check = false;
    if needs.needs_user_idle() {
        let user_idle = system.user_idle_secs(&snapshot);
        snapshot.user_idle_secs = user_idle;
        if user_idle.is_some_and(|secs| secs >= rules::USER_IDLE_SECS) {
            let cutoff = snapshot.taken_at.checked_sub(rules::CODEX_IDLE_SECS);
            if let Some(app_age) = needs.codex_app_age {
                let activity_at = system.codex_activity_at(&snapshot, cutoff);
                snapshot.codex_activity_at = activity_at;
                let idle = activity_at.map_or(app_age, |at| snapshot.taken_at.saturating_sub(at));
                if idle >= rules::CODEX_IDLE_SECS {
                    need_footprint_gated_check = true;
                }
            }
            for (data_dir, app_age) in &needs.antigravity_app_ages {
                let activity_at = system.antigravity_activity_at_dir(&snapshot, data_dir, cutoff);
                snapshot
                    .antigravity_activities
                    .insert(data_dir.clone(), activity_at);
                if data_dir == DEFAULT_ANTIGRAVITY_DATA_DIR {
                    snapshot.antigravity_activity_at = activity_at;
                }
                let idle = activity_at.map_or(*app_age, |at| snapshot.taken_at.saturating_sub(at));
                if idle >= rules::CODEX_IDLE_SECS {
                    need_footprint_gated_check = true;
                }
            }
            if let Some(age) = needs.opencode_app_age.or(needs.opencode_service_normal_age) {
                if !opencode_probed {
                    snapshot.opencode_activity_at = system.opencode_activity_at(&snapshot);
                }
                let idle = snapshot
                    .opencode_activity_at
                    .map_or(age, |at| snapshot.taken_at.saturating_sub(at));
                if idle >= rules::CODEX_IDLE_SECS {
                    need_footprint_gated_check = true;
                }
            }
        }
    }

    let result = if need_stop || need_footprint_gated_check {
        if let Err(error) = system.resolve_footprints(&mut snapshot) {
            let _ = writeln!(
                io::stderr(),
                "footprint probe failed (continuing without footprints): {}",
                crate::error::diagnostic(&error)
            );
        }
        let planned = rules::plan(&snapshot, &ownership, state);
        apply_and_persist(
            system,
            store,
            &snapshot,
            &ownership,
            state,
            state_persisted,
            &planned,
        )
    } else {
        apply_and_persist(
            system,
            store,
            &snapshot,
            &ownership,
            state,
            state_persisted,
            &[],
        )
    };
    match result {
        Ok((_, Some(stamp))) => {
            state_cache.mark_saved(stamp);
            Ok(())
        }
        Ok((_, None)) => Ok(()),
        Err(error) => {
            // `apply_and_persist` may have mutated `state_cache.state` in place
            // before `save_state_stamped` failed; mark unpersisted so the next
            // pass retries saving instead of silently diverging from disk.
            state_cache.mark_dirty();
            Err(error)
        }
    }
}

#[cfg(test)]
pub fn watch_pass(system: &dyn System, store: &Store) -> Result<(), AppError> {
    let mut cache = StateCache::default();
    watch_pass_cached(system, store, &mut cache)
}

fn process_pending_relaunch(
    system: &dyn System,
    snapshot: &Snapshot,
    ownership: &Ownership,
    pending: PendingRelaunch,
    applied: &mut Vec<Record>,
    state_changed: &mut bool,
) -> Option<PendingRelaunch> {
    if !ownership.is_bundle_running(&pending.bundle)
        && snapshot.taken_at.saturating_sub(pending.since) <= RELAUNCH_WINDOW_SECS
    {
        let record = actions::relaunch(system, &pending.bundle, &pending.rule);
        let retry = record.outcome != Outcome::Restarted;
        if !retry {
            *state_changed = true;
        }
        applied.push(record);
        retry.then_some(pending)
    } else {
        *state_changed = true;
        None
    }
}

fn apply_and_persist(
    system: &dyn System,
    store: &Store,
    snapshot: &Snapshot,
    ownership: &Ownership,
    state: &mut State,
    state_persisted: bool,
    planned: &[Action],
) -> Result<(Vec<Record>, Option<Option<FileStamp>>), AppError> {
    let mut applied = Vec::new();
    let mut state_changed = false;
    // Any app a restart left quit is opened again once it is gone, until the
    // window passes; a running app needs nothing.
    if let Some(pending) = state.pending_relaunch.take() {
        state.pending_relaunch = process_pending_relaunch(
            system,
            snapshot,
            ownership,
            pending,
            &mut applied,
            &mut state_changed,
        );
    }
    if !state.pending_relaunches.is_empty() {
        let existing = std::mem::take(&mut state.pending_relaunches);
        for pending in existing {
            if let Some(keep) = process_pending_relaunch(
                system,
                snapshot,
                ownership,
                pending,
                &mut applied,
                &mut state_changed,
            ) {
                state.pending_relaunches.push(keep);
            }
        }
    }

    let executed = actions::execute(system, planned, snapshot.user);
    for (action, record) in planned.iter().zip(&executed) {
        if let Some((bundle, rule)) = actions::left_quit(action, record) {
            let next = PendingRelaunch {
                bundle,
                since: snapshot.taken_at,
                rule,
            };
            if matches!(action, Action::RestartCodex { .. }) {
                if state.pending_relaunch.as_ref() != Some(&next) {
                    state.pending_relaunch = Some(next);
                    state_changed = true;
                }
            } else if let Some(slot) = state
                .pending_relaunches
                .iter_mut()
                .find(|existing| existing.bundle == next.bundle)
            {
                if *slot != next {
                    *slot = next;
                    state_changed = true;
                }
            } else {
                state.pending_relaunches.push(next);
                state_changed = true;
            }
        }
        if record.outcome != Outcome::Skipped {
            match action {
                Action::RestartCodex { app_exe, .. } => {
                    let bundle = actions::app_bundle(app_exe)
                        .and_then(|p| p.to_str())
                        .unwrap_or(app_exe);
                    state_changed |=
                        state.record_restart(AgentKind::Codex, bundle, snapshot.taken_at);
                }
                Action::RestartApp { agent, app_exe, .. } => {
                    let bundle = actions::app_bundle(app_exe)
                        .and_then(|p| p.to_str())
                        .unwrap_or(app_exe);
                    state_changed |= state.record_restart(*agent, bundle, snapshot.taken_at);
                }
                _ => {}
            }
        }
    }
    applied.extend(executed);
    state_changed |= state.observe(snapshot, ownership);
    // Both are attempted: state holds the restart gate, the log the record of
    // what was done. A failed log write does not fail the pass.
    let saved = if !state_persisted || state_changed {
        store
            .save_state_stamped(state)
            .map(Some)
            .map_err(AppError::Store)
    } else {
        Ok(None)
    };
    if let Err(error) = store.append(&applied) {
        let _ = writeln!(io::stderr(), "cannot append to the action log: {error}");
    }
    let stamp = saved?;
    Ok((applied, stamp))
}

fn build(
    snapshot: &Snapshot,
    ownership: &Ownership,
    state: &State,
    planned: Vec<Action>,
) -> Report {
    let helper = |pid: &i32| {
        let process = ownership.process(snapshot, *pid)?;
        Some(HelperReport {
            pid: *pid,
            exe_name: CompactString::from(snapshot.exe_name(process)),
            footprint_bytes: ownership.tree_footprint(snapshot, *pid),
            age_secs: process.age_secs,
        })
    };
    let claude_sessions = ownership
        .claude_sessions
        .iter()
        .filter_map(|session| {
            let process = ownership.process(snapshot, session.pid)?;
            let helpers: Vec<HelperReport> = session.helpers.iter().filter_map(helper).collect();
            Some(SessionReport {
                pid: session.pid,
                idle_secs: state.session_idle_secs_for(
                    process.pid,
                    snapshot.started(process),
                    snapshot.taken_at,
                ),
                helpers_footprint_bytes: helpers.iter().map(|h| h.footprint_bytes).sum(),
                helpers,
            })
        })
        .collect();
    let codex = ownership.codex_app.as_ref().and_then(|app| {
        let process = ownership.process(snapshot, app.pid)?;
        let pools: Vec<HelperReport> = app.pools.iter().filter_map(helper).collect();
        let idle_secs = snapshot
            .codex_activity_at
            .map_or(process.age_secs, |at| snapshot.taken_at.saturating_sub(at));
        Some(CodexReport {
            app_pid: app.pid,
            idle_secs,
            footprint_bytes: app
                .runtimes
                .iter()
                .map(|runtime| ownership.tree_footprint(snapshot, *runtime))
                .sum(),
            pools_footprint_bytes: pools.iter().map(|h| h.footprint_bytes).sum(),
            pools,
            commands: app.commands.iter().filter_map(helper).collect(),
        })
    });
    let desktop_apps = ownership
        .desktop_apps
        .iter()
        .filter_map(|app| {
            let process = ownership.process(snapshot, app.pid)?;
            let pools: Vec<HelperReport> = app.pools.iter().filter_map(helper).collect();
            let idle_secs = match app.agent {
                AgentKind::Codex => Some(
                    snapshot
                        .codex_activity_at
                        .map_or(process.age_secs, |at| snapshot.taken_at.saturating_sub(at)),
                ),
                AgentKind::Antigravity => Some(
                    snapshot
                        .antigravity_activity_for(app.data_dir.as_deref())
                        .map_or(process.age_secs, |at| snapshot.taken_at.saturating_sub(at)),
                ),
                AgentKind::Opencode => Some(
                    snapshot
                        .opencode_activity_at
                        .map_or(process.age_secs, |at| snapshot.taken_at.saturating_sub(at)),
                ),
                AgentKind::Claude | AgentKind::Other => None,
            };
            let footprint_bytes = ownership.desktop_app_footprint(snapshot, app);
            Some(DesktopAppReport {
                agent: app.agent,
                app_name: app.app_name.clone(),
                bundle_path: app.bundle_path.clone(),
                app_pid: app.pid,
                idle_secs,
                footprint_bytes,
                pools_footprint_bytes: pools.iter().map(|h| h.footprint_bytes).sum(),
                pools,
                commands: app.commands.iter().filter_map(helper).collect(),
            })
        })
        .collect();
    let agent_sessions = ownership
        .agent_sessions
        .iter()
        .filter_map(|session| {
            let _ = ownership.process(snapshot, session.pid)?;
            let helpers: Vec<HelperReport> = session.helpers.iter().filter_map(helper).collect();
            let commands: Vec<HelperReport> = session.commands.iter().filter_map(helper).collect();
            let idle_secs =
                if session.agent == AgentKind::Opencode && session.mode == SessionMode::Service {
                    rules::opencode_service_idle_secs(
                        snapshot,
                        ownership,
                        session.pid,
                        session.commands.is_empty(),
                    )
                } else {
                    None
                };
            Some(AgentSessionReport {
                agent: session.agent,
                mode: session.mode,
                pid: session.pid,
                name: session.name.clone(),
                parent_agent: session.parent_agent.clone(),
                idle_secs,
                footprint_bytes: ownership.tree_footprint(snapshot, session.pid),
                helpers_footprint_bytes: helpers.iter().map(|h| h.footprint_bytes).sum(),
                helpers,
                commands,
            })
        })
        .collect();
    Report {
        version: env!("CARGO_PKG_VERSION"),
        taken_at: snapshot.taken_at,
        pressure: state.effective_pressure(snapshot),
        kernel_pressure: snapshot.pressure,
        swap: snapshot.swap,
        user_idle_secs: snapshot.user_idle_secs,
        claude_sessions,
        codex,
        desktop_apps,
        agent_sessions,
        orphans: ownership.orphans.iter().filter_map(helper).collect(),
        planned,
        applied: Vec::new(),
        recent: Vec::new(),
    }
}

struct Gb(u64);

impl fmt::Display for Gb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2} GB", self.0 as f64 / (1u64 << 30) as f64)
    }
}

struct IdleDisplay(Option<u64>);

impl fmt::Display for IdleDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(secs) => write!(f, "{} min", minutes(secs)),
            None => f.write_str("unknown"),
        }
    }
}

fn pressure_name(pressure: Pressure) -> &'static str {
    match pressure {
        Pressure::Normal => "normal",
        Pressure::Warning => "warning",
        Pressure::Critical => "critical",
    }
}

fn minutes(secs: u64) -> u64 {
    secs / 60
}

pub fn text(report: &Report) -> String {
    let mut out = String::with_capacity(512);
    let _ = writeln!(
        out,
        "memory: {} pressure (kernel {}), swap {} of {}, user idle {}",
        pressure_name(report.pressure),
        pressure_name(report.kernel_pressure),
        Gb(report.swap.used_bytes),
        Gb(report.swap.total_bytes),
        IdleDisplay(report.user_idle_secs),
    );
    let helpers: u64 = report
        .claude_sessions
        .iter()
        .map(|s| s.helpers_footprint_bytes)
        .sum();
    let idle_hour = report
        .claude_sessions
        .iter()
        .filter(|s| s.idle_secs.is_some_and(|idle| idle >= 3600))
        .count();
    let _ = writeln!(
        out,
        "claude code: {} sessions, helpers {}, idle over 60 min: {}",
        report.claude_sessions.len(),
        Gb(helpers),
        idle_hour
    );
    match &report.codex {
        Some(codex) => {
            let _ = writeln!(
                out,
                "codex: app {}, holds {}, helpers {} in {} processes, running commands {}, idle {} min",
                codex.app_pid,
                Gb(codex.footprint_bytes),
                Gb(codex.pools_footprint_bytes),
                codex.pools.len(),
                codex.commands.len(),
                minutes(codex.idle_secs)
            );
        }
        None => out.push_str("codex: app not running\n"),
    }
    let primary_codex_pid = report.codex.as_ref().map(|c| c.app_pid);
    let extra_apps: Vec<&DesktopAppReport> = report
        .desktop_apps
        .iter()
        .filter(|app| Some(app.app_pid) != primary_codex_pid)
        .collect();
    if !extra_apps.is_empty() {
        out.push_str("desktop apps:\n");
        for app in extra_apps {
            let _ = writeln!(
                out,
                "  {} ({}, pid {}): holds {}, helpers {} in {} processes, running commands {}, idle {}",
                app.agent.as_str(),
                app.app_name,
                app.app_pid,
                Gb(app.footprint_bytes),
                Gb(app.pools_footprint_bytes),
                app.pools.len(),
                app.commands.len(),
                IdleDisplay(app.idle_secs),
            );
        }
    }
    if !report.agent_sessions.is_empty() {
        out.push_str("agent sessions:\n");
        for session in &report.agent_sessions {
            match (&session.name, &session.parent_agent) {
                (Some(name), Some(parent)) => {
                    let _ = writeln!(
                        out,
                        "  {} {} ({}) under {} (pid {}): holds {}, helpers {} in {} processes, running commands {}",
                        session.agent.as_str(),
                        session.mode.as_str(),
                        name,
                        parent,
                        session.pid,
                        Gb(session.footprint_bytes),
                        Gb(session.helpers_footprint_bytes),
                        session.helpers.len(),
                        session.commands.len(),
                    );
                }
                (Some(name), None) => {
                    let _ = writeln!(
                        out,
                        "  {} {} ({}, pid {}): holds {}, helpers {} in {} processes, running commands {}, idle {}",
                        session.agent.as_str(),
                        session.mode.as_str(),
                        name,
                        session.pid,
                        Gb(session.footprint_bytes),
                        Gb(session.helpers_footprint_bytes),
                        session.helpers.len(),
                        session.commands.len(),
                        IdleDisplay(session.idle_secs),
                    );
                }
                (None, Some(parent)) => {
                    let _ = writeln!(
                        out,
                        "  {} {} under {} (pid {}): holds {}, helpers {} in {} processes, running commands {}",
                        session.agent.as_str(),
                        session.mode.as_str(),
                        parent,
                        session.pid,
                        Gb(session.footprint_bytes),
                        Gb(session.helpers_footprint_bytes),
                        session.helpers.len(),
                        session.commands.len(),
                    );
                }
                (None, None) => {
                    let _ = writeln!(
                        out,
                        "  {} {} (pid {}): holds {}, helpers {} in {} processes, running commands {}, idle {}",
                        session.agent.as_str(),
                        session.mode.as_str(),
                        session.pid,
                        Gb(session.footprint_bytes),
                        Gb(session.helpers_footprint_bytes),
                        session.helpers.len(),
                        session.commands.len(),
                        IdleDisplay(session.idle_secs),
                    );
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "orphans: {}, {}",
        report.orphans.len(),
        Gb(report.orphans.iter().map(|o| o.footprint_bytes).sum())
    );
    if report.planned.is_empty() {
        out.push_str("planned: nothing to do\n");
    } else {
        let _ = writeln!(
            out,
            "planned: {} actions, {}",
            report.planned.len(),
            Gb(report.planned.iter().map(Action::footprint_bytes).sum())
        );
    }
    if !report.applied.is_empty() {
        let mut stopped = 0usize;
        let mut restarted = 0usize;
        let mut skipped = 0usize;
        let mut refused = 0usize;
        let mut failed = 0usize;
        let mut freed = 0u64;
        for record in &report.applied {
            match record.outcome {
                Outcome::Stopped => {
                    stopped += 1;
                    freed += record.footprint_bytes;
                }
                Outcome::Restarted => {
                    restarted += 1;
                    freed += record.footprint_bytes;
                }
                Outcome::Skipped => skipped += 1,
                Outcome::Refused => refused += 1,
                Outcome::Failed => failed += 1,
            }
        }
        let _ = writeln!(
            out,
            "applied: {stopped} stopped, {restarted} restarted, {skipped} skipped, {refused} refused, {failed} failed; freed {}",
            Gb(freed)
        );
    }
    if !report.recent.is_empty() {
        out.push_str("recent:\n");
        for record in &report.recent {
            let _ = writeln!(
                out,
                "  {} min ago  {:?}  {}  {}  {}",
                minutes(report.taken_at.saturating_sub(record.at)),
                record.outcome,
                record.rule,
                record.target,
                Gb(record.footprint_bytes)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use compact_str::CompactString;

    use super::{pass, text};
    use crate::{
        actions::{System, tests::FakeSystem},
        error::AppError,
        model::Snapshot,
        owners::tests::machine,
        store::{Outcome, Store},
    };

    /// The fake machine from the ownership tests, with its processes alive.
    struct Machine {
        snapshot: RefCell<Snapshot>,
        system: FakeSystem,
    }

    impl Machine {
        fn new() -> Self {
            let snapshot = machine();
            let system = FakeSystem::default();
            for process in &snapshot.processes {
                system.alive.borrow_mut().insert(
                    process.pid,
                    (CompactString::from(snapshot.started(process)), process.uid),
                );
            }
            Self {
                snapshot: RefCell::new(snapshot),
                system,
            }
        }
    }

    impl System for Machine {
        fn snapshot(&self) -> Result<Snapshot, AppError> {
            let mut snapshot = self.snapshot.borrow().clone();
            let alive = self.system.alive.borrow();
            snapshot.processes.retain(|p| alive.contains_key(&p.pid));
            Ok(snapshot)
        }
        fn identify(&self, pid: i32) -> Option<(CompactString, u32)> {
            self.system.identify(pid)
        }
        fn signal(&self, pid: i32, signal: crate::actions::Signal) -> std::io::Result<()> {
            self.system.signal(pid, signal)
        }
        fn launch_app(&self, bundle: &std::path::Path) -> std::io::Result<()> {
            self.system.launch_app(bundle)
        }
        fn sleep(&self, duration: std::time::Duration) {
            self.system.sleep(duration);
        }
        fn now(&self) -> u64 {
            self.snapshot.borrow().taken_at
        }
    }

    #[test]
    fn status_plans_without_effects_and_reclaim_applies_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let machine = Machine::new();

        let status = pass(&machine, &store, false).unwrap();
        // gopls (402) is not configured and not yet known, so one orphan.
        assert_eq!(status.orphans.len(), 1);
        assert_eq!(status.planned.len(), 1);
        assert!(status.applied.is_empty());
        assert!(machine.system.signals.borrow().is_empty());
        assert!(!dir.path().join("state.json").exists());

        let reclaim = pass(&machine, &store, true).unwrap();
        assert_eq!(reclaim.applied.len(), 1);
        assert_eq!(reclaim.applied[0].outcome, Outcome::Stopped);
        assert_eq!(reclaim.applied[0].pids.as_slice(), [401, 400]);
        assert!(!reclaim.has_failures());
        assert_eq!(store.recent(10).len(), 1);

        let after = pass(&machine, &store, false).unwrap();
        assert!(after.orphans.is_empty());
        assert_eq!(after.claude_sessions.len(), 2);
        assert_eq!(after.claude_sessions[0].idle_secs, Some(0));
        let rendered = text(&after);
        assert!(rendered.contains("claude code: 2 sessions"), "{rendered}");
        assert!(rendered.contains("planned: nothing to do"), "{rendered}");
        assert!(rendered.contains("Stopped  orphan  node"), "{rendered}");
    }

    #[test]
    fn an_app_that_quits_after_the_wait_is_opened_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut machine = Machine::new();
        {
            let mut snapshot = machine.snapshot.borrow_mut();
            snapshot.processes.retain(|p| p.pid != 213);
            snapshot.codex_activity_at = Some(snapshot.taken_at - 40 * 60);
            snapshot.user_idle_secs = Some(45 * 60);
            snapshot.pressure = crate::model::Pressure::Warning;
        }
        // The app ignores the quit for longer than the wait.
        machine.system.immortal.insert(200);
        let first = pass(&machine, &store, true).unwrap();
        let restart = first
            .applied
            .iter()
            .find(|record| record.rule == "idle-codex-restart")
            .unwrap();
        assert_eq!(restart.outcome, Outcome::Refused);
        assert!(machine.system.launched.borrow().is_empty());

        // It quits a moment later; the next pass opens it again, once.
        for pid in [200, 210, 211, 212, 215] {
            machine.system.alive.borrow_mut().remove(&pid);
        }
        machine.snapshot.borrow_mut().taken_at += 60;
        let second = pass(&machine, &store, true).unwrap();
        assert_eq!(second.applied[0].outcome, Outcome::Restarted);
        assert_eq!(
            *machine.system.launched.borrow(),
            [std::path::PathBuf::from("/Applications/ChatGPT.app")]
        );
        machine.snapshot.borrow_mut().taken_at += 60;
        pass(&machine, &store, true).unwrap();
        assert_eq!(machine.system.launched.borrow().len(), 1);
    }

    struct LazyMachine {
        inner: Machine,
        user_idle_calls: std::cell::Cell<usize>,
        codex_activity_calls: std::cell::Cell<usize>,
        footprint_calls: std::cell::Cell<usize>,
        user_idle_value: Option<u64>,
        codex_activity_value: Option<u64>,
    }

    impl LazyMachine {
        fn new(snapshot: Snapshot) -> Self {
            let user_idle_value = snapshot.user_idle_secs;
            let codex_activity_value = snapshot.codex_activity_at;
            let system = FakeSystem::default();
            for process in &snapshot.processes {
                system.alive.borrow_mut().insert(
                    process.pid,
                    (CompactString::from(snapshot.started(process)), process.uid),
                );
            }
            Self {
                inner: Machine {
                    snapshot: RefCell::new(snapshot),
                    system,
                },
                user_idle_calls: std::cell::Cell::new(0),
                codex_activity_calls: std::cell::Cell::new(0),
                footprint_calls: std::cell::Cell::new(0),
                user_idle_value,
                codex_activity_value,
            }
        }
    }

    impl System for LazyMachine {
        fn snapshot(&self) -> Result<Snapshot, AppError> {
            self.inner.snapshot()
        }
        fn begin_pass(
            &self,
            _known_helpers: &std::collections::BTreeSet<crate::model::Identity>,
            include_user_idle: bool,
        ) -> Result<Snapshot, AppError> {
            let mut snap = self.inner.snapshot()?;
            snap.user_idle_secs = if include_user_idle {
                self.user_idle_secs(&snap)
            } else {
                None
            };
            snap.codex_activity_at = None;
            for process in &mut snap.processes {
                process.footprint_bytes = None;
            }
            Ok(snap)
        }
        fn user_idle_secs(&self, _snapshot: &Snapshot) -> Option<u64> {
            self.user_idle_calls.set(self.user_idle_calls.get() + 1);
            self.user_idle_value
        }
        fn codex_activity_at(
            &self,
            _snapshot: &Snapshot,
            _early_exit_after: Option<u64>,
        ) -> Option<u64> {
            self.codex_activity_calls
                .set(self.codex_activity_calls.get() + 1);
            self.codex_activity_value
        }
        fn resolve_footprints(&self, snapshot: &mut Snapshot) -> Result<(), AppError> {
            self.footprint_calls.set(self.footprint_calls.get() + 1);
            let full = self.inner.snapshot()?;
            for process in &mut snapshot.processes {
                if let Some(orig) = full.processes.iter().find(|p| p.pid == process.pid) {
                    process.footprint_bytes = orig.footprint_bytes;
                }
            }
            Ok(())
        }
        fn identify(&self, pid: i32) -> Option<(CompactString, u32)> {
            self.inner.identify(pid)
        }
        fn signal(&self, pid: i32, signal: crate::actions::Signal) -> std::io::Result<()> {
            self.inner.signal(pid, signal)
        }
        fn launch_app(&self, bundle: &std::path::Path) -> std::io::Result<()> {
            self.inner.launch_app(bundle)
        }
        fn sleep(&self, duration: std::time::Duration) {
            self.inner.sleep(duration);
        }
        fn now(&self) -> u64 {
            self.inner.now()
        }
    }

    #[test]
    fn watch_pass_skips_expensive_probes_when_no_rule_can_fire() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut snap = machine();
        // Remove orphan 400 so no orphan rule fires; Codex still has command 213 running.
        snap.processes.retain(|p| p.pid != 400 && p.pid != 401);
        let lazy = LazyMachine::new(snap);

        super::watch_pass(&lazy, &store).unwrap();
        assert_eq!(lazy.user_idle_calls.get(), 0, "codex has running command");
        assert_eq!(lazy.codex_activity_calls.get(), 0);
        assert_eq!(lazy.footprint_calls.get(), 0, "no rule needs footprints");
        assert!(lazy.inner.system.signals.borrow().is_empty());
        assert_eq!(store.load_state().sessions.len(), 2);
    }

    #[test]
    fn watch_pass_resolves_footprints_for_orphan_and_skips_protected_orphan() {
        // Protected orphan (child runs a shell): no footprints needed.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut snap = machine();
        crate::owners::tests::add_proc(&mut snap, 405, 401, "/bin/zsh", "zsh", 60, 4);
        let protected = LazyMachine::new(snap);
        super::watch_pass(&protected, &store).unwrap();
        assert_eq!(protected.footprint_calls.get(), 0);
        assert!(protected.inner.system.signals.borrow().is_empty());

        // Unprotected orphan: resolves footprints, stops 401/400, records footprint.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = Store::new(dir2.path().to_owned());
        let active = LazyMachine::new(machine());
        super::watch_pass(&active, &store2).unwrap();
        assert_eq!(active.footprint_calls.get(), 1);
        let recent = store2.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].outcome, Outcome::Stopped);
        assert_eq!(recent[0].footprint_bytes, 94 << 20);
    }

    #[test]
    fn watch_pass_restarts_codex_lazily_and_short_circuits_on_active_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut snap = machine();
        snap.processes.retain(|p| !matches!(p.pid, 213 | 400 | 401));
        snap.codex_activity_at = Some(snap.taken_at - 40 * 60);
        snap.user_idle_secs = Some(5 * 60);
        snap.pressure = crate::model::Pressure::Warning;

        // Active user: checks user_idle, skips codex_activity and footprints.
        let active_user = LazyMachine::new(snap.clone());
        super::watch_pass(&active_user, &store).unwrap();
        assert_eq!(active_user.user_idle_calls.get(), 1);
        assert_eq!(active_user.codex_activity_calls.get(), 0);
        assert_eq!(active_user.footprint_calls.get(), 0);

        // Idle user + idle Codex: checks all three and restarts the app.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = Store::new(dir2.path().to_owned());
        snap.user_idle_secs = Some(45 * 60);
        let idle = LazyMachine::new(snap);
        super::watch_pass(&idle, &store2).unwrap();
        assert_eq!(idle.user_idle_calls.get(), 1);
        assert_eq!(idle.codex_activity_calls.get(), 1);
        assert_eq!(idle.footprint_calls.get(), 1);
        assert_eq!(
            *idle.inner.system.launched.borrow(),
            [std::path::PathBuf::from("/Applications/ChatGPT.app")]
        );
    }

    #[test]
    fn pass_skips_footprints_for_helperless_claude_and_watch_skips_runtimeless_codex() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        // Helperless Claude session + ChatGPT app with zero Codex runtimes.
        let mut snap = Snapshot {
            taken_at: 1_791_290_000,
            user: 501,
            pressure: crate::model::Pressure::Warning,
            user_idle_secs: Some(45 * 60),
            codex_activity_at: Some(1_791_290_000 - 40 * 60),
            launchd_jobs: Some(Vec::new()),
            ..Snapshot::default()
        };
        crate::owners::tests::add_proc(
            &mut snap,
            110,
            1,
            "/Users/u/.local/bin/claude",
            "claude",
            9000,
            300,
        );
        crate::owners::tests::add_proc(
            &mut snap,
            200,
            1,
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            "ChatGPT",
            9000,
            400,
        );
        let lazy_status = LazyMachine::new(snap.clone());
        let report = pass(&lazy_status, &store, false).unwrap();
        assert_eq!(report.claude_sessions.len(), 1);
        assert_eq!(report.claude_sessions[0].helpers_footprint_bytes, 0);
        assert!(report.codex.is_some());
        assert_eq!(
            lazy_status.footprint_calls.get(),
            0,
            "no helpers, runtimes, pools, commands, or orphans need footprints"
        );

        let lazy_watch = LazyMachine::new(snap.clone());
        super::watch_pass(&lazy_watch, &store).unwrap();
        assert_eq!(
            lazy_watch.user_idle_calls.get(),
            0,
            "runtimeless Codex app cannot be worth restarting"
        );
        assert_eq!(lazy_watch.codex_activity_calls.get(), 0);
        assert_eq!(lazy_watch.footprint_calls.get(), 0);

        // With no sessions and unchanged swap, a second watch pass skips rewriting state.json.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = Store::new(dir2.path().to_owned());
        snap.processes.clear();
        let idle_watch = LazyMachine::new(snap);
        super::watch_pass(&idle_watch, &store2).unwrap();
        let state_path = dir2.path().join("state.json");
        assert!(state_path.exists());
        // Replace state.json contents with pretty-printed equivalent JSON so any
        // rewrite (which writes compact JSON) is detectable portably across OSes.
        let compact = std::fs::read_to_string(&state_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&compact).unwrap();
        let pretty = serde_json::to_string_pretty(&parsed).unwrap();
        assert_ne!(compact, pretty);
        std::fs::write(&state_path, &pretty).unwrap();

        super::watch_pass(&idle_watch, &store2).unwrap();
        assert_eq!(
            std::fs::read_to_string(&state_path).unwrap(),
            pretty,
            "unchanged state must not rewrite state.json"
        );

        // Changing swap triggers a real write.
        idle_watch.inner.snapshot.borrow_mut().swap.used_bytes = 4096;
        super::watch_pass(&idle_watch, &store2).unwrap();
        assert_eq!(store2.load_state().last_swap_used_bytes, Some(4096));
        assert_ne!(std::fs::read_to_string(&state_path).unwrap(), pretty);
    }

    #[test]
    fn watch_pass_cached_reuses_state_and_observes_external_reclaim_updates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut snap = machine();
        snap.processes.retain(|p| !matches!(p.pid, 213 | 400 | 401));
        let lazy = LazyMachine::new(snap);
        let mut cache = crate::store::StateCache::default();

        super::watch_pass_cached(&lazy, &store, &mut cache).unwrap();
        assert_eq!(store.load_state().sessions.len(), 2);
        assert_eq!(
            lazy.user_idle_calls.get(),
            1,
            "pass 1 checks HID idle when Codex has no commands or cooldown"
        );

        // External `reclaim`-style write updates `last_codex_restart_at`.
        let mut external = store.load_state();
        external.last_codex_restart_at = Some(1_791_290_000);
        store.save_state(&external).unwrap();

        // Next cached watch pass detects the stamp change, reloads `last_codex_restart_at`,
        // and therefore skips Codex idle checks because of the 6-hour cooldown.
        lazy.inner.snapshot.borrow_mut().taken_at += 60;
        super::watch_pass_cached(&lazy, &store, &mut cache).unwrap();
        assert_eq!(
            lazy.user_idle_calls.get(),
            1,
            "reloaded cooldown skips HID idle on pass 2"
        );
        assert_eq!(
            store.load_state().last_codex_restart_at,
            Some(1_791_290_000)
        );
    }

    #[cfg(unix)]
    #[test]
    fn watch_pass_cached_retries_persist_after_transient_save_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut snap = Snapshot {
            taken_at: 1_791_290_000,
            user: 501,
            launchd_jobs: Some(Vec::new()),
            ..Snapshot::default()
        };
        snap.swap.used_bytes = 1024;
        let lazy = LazyMachine::new(snap);
        let mut cache = crate::store::StateCache::default();

        // Pass 1 persists initial state.
        super::watch_pass_cached(&lazy, &store, &mut cache).unwrap();
        assert_eq!(store.load_state().last_swap_used_bytes, Some(1024));

        // Simulate transient disk error (read-only store directory): Pass 2
        // mutates in-memory cached state, then fails to save.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        lazy.inner.snapshot.borrow_mut().swap.used_bytes = 8192;
        lazy.inner.snapshot.borrow_mut().taken_at += 60;
        let err = super::watch_pass_cached(&lazy, &store, &mut cache).unwrap_err();
        assert!(matches!(err, AppError::Store(_)), "{err:?}");
        assert_eq!(store.load_state().last_swap_used_bytes, Some(1024));

        // Once the directory is writable again, Pass 3 with the same swap must
        // still flush the dirty cached state to disk instead of skipping write.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        lazy.inner.snapshot.borrow_mut().taken_at += 60;
        super::watch_pass_cached(&lazy, &store, &mut cache).unwrap();
        assert_eq!(store.load_state().last_swap_used_bytes, Some(8192));
    }

    #[test]
    fn watch_pass_still_stops_orphans_and_preserves_state_when_top_fails() {
        struct FailingFootprintSystem {
            inner: LazyMachine,
        }

        impl System for FailingFootprintSystem {
            fn snapshot(&self) -> Result<Snapshot, AppError> {
                self.inner.snapshot()
            }
            fn begin_pass(
                &self,
                known: &std::collections::BTreeSet<crate::model::Identity>,
                include_user_idle: bool,
            ) -> Result<Snapshot, AppError> {
                self.inner.begin_pass(known, include_user_idle)
            }
            fn user_idle_secs(&self, snapshot: &Snapshot) -> Option<u64> {
                self.inner.user_idle_secs(snapshot)
            }
            fn codex_activity_at(&self, snapshot: &Snapshot, cutoff: Option<u64>) -> Option<u64> {
                self.inner.codex_activity_at(snapshot, cutoff)
            }
            fn resolve_footprints(&self, _snapshot: &mut Snapshot) -> Result<(), AppError> {
                Err(AppError::NoHome)
            }
            fn identify(&self, pid: i32) -> Option<(CompactString, u32)> {
                self.inner.identify(pid)
            }
            fn signal(&self, pid: i32, signal: crate::actions::Signal) -> std::io::Result<()> {
                self.inner.signal(pid, signal)
            }
            fn launch_app(&self, bundle: &std::path::Path) -> std::io::Result<()> {
                self.inner.launch_app(bundle)
            }
            fn sleep(&self, duration: std::time::Duration) {
                self.inner.sleep(duration);
            }
            fn now(&self) -> u64 {
                self.inner.now()
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut snap = machine();
        // Make Codex otherwise eligible to restart AND leave orphan 400 present.
        snap.processes.retain(|p| p.pid != 213);
        snap.codex_activity_at = Some(snap.taken_at - 40 * 60);
        snap.user_idle_secs = Some(45 * 60);
        snap.pressure = crate::model::Pressure::Warning;

        let failing = FailingFootprintSystem {
            inner: LazyMachine::new(snap),
        };
        // Must NOT fail the pass when footprint probe errors!
        super::watch_pass(&failing, &store).unwrap();

        let recent = store.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].rule, "orphan");
        assert_eq!(recent[0].outcome, Outcome::Stopped);
        assert_eq!(recent[0].pids.as_slice(), [401, 400]);
        // Codex must NOT be restarted without footprint evidence.
        assert!(failing.inner.inner.system.launched.borrow().is_empty());
        // Session state was STILL observed and persisted (no idle timer reset).
        assert_eq!(store.load_state().sessions.len(), 2);
    }

    #[test]
    fn antigravity_clone_late_quit_reopens_exact_bundle_and_renders_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let mut snap = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            pressure: crate::model::Pressure::Warning,
            user_idle_secs: Some(45 * 60),
            antigravity_activity_at: Some(1_795_725_000 - 40 * 60),
            launchd_jobs: Some(Vec::new()),
            ..Snapshot::default()
        };
        crate::owners::tests::add_proc(
            &mut snap,
            44215,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real",
            90_000,
            300,
        );
        crate::owners::tests::add_proc(
            &mut snap,
            44926,
            44215,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity",
            90_000,
            4096,
        );
        let system = FakeSystem::default();
        for p in &snap.processes {
            system
                .alive
                .borrow_mut()
                .insert(p.pid, (CompactString::from(snap.started(p)), p.uid));
        }
        let mut machine = Machine {
            snapshot: RefCell::new(snap),
            system,
        };
        let status = pass(&machine, &store, false).unwrap();
        let rendered = text(&status);
        assert!(rendered.contains("Antigravity US"), "{rendered}");
        assert!(rendered.contains("desktop apps:"), "{rendered}");

        // App refuses SIGTERM for 60s -> recorded Refused and saved in pending_relaunches.
        machine.system.immortal.insert(44215);
        let first = pass(&machine, &store, true).unwrap();
        assert_eq!(first.applied[0].rule, "idle-antigravity-restart");
        assert_eq!(first.applied[0].outcome, Outcome::Refused);
        assert!(machine.system.launched.borrow().is_empty());

        // App finishes quitting late; next pass relaunches the exact Antigravity US bundle.
        machine.system.alive.borrow_mut().remove(&44215);
        machine.system.alive.borrow_mut().remove(&44926);
        machine.snapshot.borrow_mut().taken_at += 60;
        let second = pass(&machine, &store, true).unwrap();
        assert_eq!(second.applied[0].rule, "idle-antigravity-restart");
        assert_eq!(second.applied[0].outcome, Outcome::Restarted);
        assert_eq!(
            *machine.system.launched.borrow(),
            [std::path::PathBuf::from(
                "/Users/u/Applications/Antigravity US.app"
            )]
        );
    }

    #[test]
    fn report_renders_generic_tools_and_probes_per_antigravity_data_dir() {
        use std::sync::Arc;

        use smallvec::smallvec;

        struct MultiDirSystem {
            snap: Snapshot,
            probed_dirs: RefCell<Vec<String>>,
            system: FakeSystem,
        }

        impl System for MultiDirSystem {
            fn snapshot(&self) -> Result<Snapshot, AppError> {
                Ok(self.snap.clone())
            }
            fn antigravity_activity_at_dir(
                &self,
                _snapshot: &Snapshot,
                data_dir: &str,
                _early_exit_after: Option<u64>,
            ) -> Option<u64> {
                self.probed_dirs.borrow_mut().push(data_dir.to_owned());
                match data_dir {
                    "antigravity" => Some(self.snap.taken_at - 120),
                    "antigravity-ide" => Some(self.snap.taken_at - 3600),
                    _ => None,
                }
            }
            fn identify(&self, pid: i32) -> Option<(CompactString, u32)> {
                self.system.identify(pid)
            }
            fn signal(&self, pid: i32, signal: crate::actions::Signal) -> std::io::Result<()> {
                self.system.signal(pid, signal)
            }
            fn launch_app(&self, bundle: &std::path::Path) -> std::io::Result<()> {
                self.system.launch_app(bundle)
            }
            fn sleep(&self, d: std::time::Duration) {
                self.system.sleep(d);
            }
            fn now(&self) -> u64 {
                self.snap.taken_at
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned());
        let now = 1_795_725_000;
        let mut snap = Snapshot {
            taken_at: now,
            user: 501,
            pressure: crate::model::Pressure::Warning,
            user_idle_secs: Some(45 * 60),
            launchd_jobs: Some(vec![100, 200, 300]),
            mcp_servers: Arc::from([crate::model::ServerSignature {
                command: "npx".into(),
                args: smallvec!["-y".into(), "@playwright/mcp@latest".into()],
            }]),
            ..Snapshot::default()
        };
        crate::owners::tests::add_proc(
            &mut snap,
            100,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real",
            90_000,
            300,
        );
        crate::owners::tests::add_proc(
            &mut snap,
            101,
            100,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity --app_data_dir antigravity",
            90_000,
            2048,
        );
        crate::owners::tests::add_proc(
            &mut snap,
            200,
            1,
            "/Applications/Antigravity IDE.app/Contents/MacOS/Antigravity",
            "Antigravity",
            90_000,
            300,
        );
        crate::owners::tests::add_proc(
            &mut snap,
            201,
            200,
            "/Applications/Antigravity IDE.app/Contents/Resources/bin/language_server",
            "language_server --override_ide_name antigravity --app_data_dir antigravity-ide",
            90_000,
            2048,
        );
        crate::owners::tests::add_proc(
            &mut snap,
            300,
            1,
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
            "Cursor",
            90_000,
            400,
        );
        crate::owners::tests::add_proc(
            &mut snap,
            301,
            300,
            "/opt/homebrew/bin/node",
            "/opt/homebrew/bin/node /Users/u/.npm/_npx/1/node_modules/@playwright/mcp/cli.js",
            90_000,
            128,
        );
        snap.push_process_with_tty(
            400,
            1,
            501,
            "start-400",
            9000,
            0x8001,
            0,
            Some(50 << 20),
            "/opt/homebrew/bin/gemini",
            "gemini",
        );
        snap.push_process_with_tty(
            401,
            400,
            501,
            "start-401",
            9000,
            0x8001,
            0,
            Some(64 << 20),
            "/opt/homebrew/bin/node",
            "/opt/homebrew/bin/node /Users/u/.npm/_npx/1/node_modules/@playwright/mcp/cli.js",
        );

        let sys = MultiDirSystem {
            snap,
            probed_dirs: RefCell::new(Vec::new()),
            system: FakeSystem::default(),
        };
        let report = pass(&sys, &store, false).unwrap();
        assert_eq!(
            *sys.probed_dirs.borrow(),
            ["antigravity", "antigravity-ide"]
        );
        let ag_us = report
            .desktop_apps
            .iter()
            .find(|a| a.app_pid == 100)
            .unwrap();
        assert_eq!(ag_us.idle_secs, Some(120));
        let ag_ide = report
            .desktop_apps
            .iter()
            .find(|a| a.app_pid == 200)
            .unwrap();
        assert_eq!(ag_ide.idle_secs, Some(3600));
        let cursor = report
            .desktop_apps
            .iter()
            .find(|a| a.app_pid == 300)
            .unwrap();
        assert_eq!(cursor.agent, crate::model::AgentKind::Other);
        assert_eq!(cursor.idle_secs, None);
        assert_eq!(report.planned.len(), 1);
        let rendered = text(&report);
        assert!(rendered.contains("other (Cursor, pid 300)"), "{rendered}");
        assert!(
            rendered.contains("other cli (gemini, pid 400)"),
            "{rendered}"
        );
    }
}

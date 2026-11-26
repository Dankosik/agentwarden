//! One pass of agentwarden and the report it prints.

use std::{fmt, fmt::Write as _, io, io::Write as _};

use compact_str::CompactString;
use serde::Serialize;

use crate::{
    actions::{self, System},
    error::AppError,
    model::{Pressure, Snapshot, Swap},
    owners::{self, Ownership},
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
    let has_codex = snapshot
        .processes
        .iter()
        .any(|process| process.uid == snapshot.user && owners::is_codex_app(&snapshot, process));
    if has_codex {
        snapshot.codex_activity_at = system.codex_activity_at(&snapshot, None);
    }
    let ownership = Ownership::new(&snapshot, &state.known_helpers);
    let need_footprints = !ownership.orphans.is_empty()
        || ownership
            .claude_sessions
            .iter()
            .any(|session| !session.helpers.is_empty())
        || ownership.codex_app.as_ref().is_some_and(|app| {
            !app.runtimes.is_empty() || !app.pools.is_empty() || !app.commands.is_empty()
        });
    if need_footprints {
        system.resolve_footprints(&mut snapshot)?;
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
/// resolves HID idle, Codex rollout activity, and `top` footprints only when a
/// rule actually depends on them.
pub fn watch_pass_cached(
    system: &dyn System,
    store: &Store,
    state_cache: &mut StateCache,
) -> Result<(), AppError> {
    let (state, state_persisted) = state_cache.load_mut(store);
    let mut snapshot = system.begin_pass(&state.known_helpers, false)?;
    let ownership = Ownership::new(&snapshot, &state.known_helpers);

    let need_stop = rules::has_stop_candidates(&snapshot, &ownership, state);
    let mut need_codex_restart_check = false;
    if let Some(app_age) = rules::codex_eligible_for_idle_check(&snapshot, &ownership, state) {
        let user_idle = system.user_idle_secs(&snapshot);
        snapshot.user_idle_secs = user_idle;
        if user_idle.is_some_and(|secs| secs >= rules::USER_IDLE_SECS) {
            let cutoff = snapshot.taken_at.checked_sub(rules::CODEX_IDLE_SECS);
            let activity_at = system.codex_activity_at(&snapshot, cutoff);
            snapshot.codex_activity_at = activity_at;
            let codex_idle = activity_at.map_or(app_age, |at| snapshot.taken_at.saturating_sub(at));
            if codex_idle >= rules::CODEX_IDLE_SECS {
                need_codex_restart_check = true;
            }
        }
    }

    let result = if need_stop || need_codex_restart_check {
        system.resolve_footprints(&mut snapshot)?;
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
    // An app a restart left quit is opened again once it is gone, until the
    // window passes; a running app needs nothing.
    if let Some(pending) = state.pending_relaunch.take() {
        if ownership.codex_app.is_none()
            && snapshot.taken_at.saturating_sub(pending.since) <= RELAUNCH_WINDOW_SECS
        {
            let record = actions::relaunch(system, &pending.bundle);
            if record.outcome != Outcome::Restarted {
                state.pending_relaunch = Some(pending);
            } else {
                state_changed = true;
            }
            applied.push(record);
        } else {
            state_changed = true;
        }
    }
    let executed = actions::execute(system, planned, snapshot.user);
    for (action, record) in planned.iter().zip(&executed) {
        if let Some(bundle) = actions::left_quit(action, record) {
            let next = Some(PendingRelaunch {
                bundle,
                since: snapshot.taken_at,
            });
            if state.pending_relaunch != next {
                state.pending_relaunch = next;
                state_changed = true;
            }
        }
    }
    if executed
        .iter()
        .any(|record| record.rule == "idle-codex-restart" && record.outcome != Outcome::Skipped)
    {
        let next = Some(snapshot.taken_at);
        if state.last_codex_restart_at != next {
            state.last_codex_restart_at = next;
            state_changed = true;
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
    Report {
        version: env!("CARGO_PKG_VERSION"),
        taken_at: snapshot.taken_at,
        pressure: state.effective_pressure(snapshot),
        kernel_pressure: snapshot.pressure,
        swap: snapshot.swap,
        user_idle_secs: snapshot.user_idle_secs,
        claude_sessions,
        codex,
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
}

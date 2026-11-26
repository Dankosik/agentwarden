//! The rules: which processes to stop and when to restart Codex. Pure functions
//! of a snapshot, its ownership and the remembered state.

use compact_str::CompactString;
use serde::Serialize;
use smallvec::SmallVec;

use crate::{
    model::{AgentKind, Identity, Pressure, SessionMode, Snapshot},
    owners::{
        DEFAULT_ANTIGRAVITY_DATA_DIR, DesktopApp, Ownership, is_agent_host, is_antigravity_cli,
        is_codex, is_opencode_runtime, is_shell,
    },
    state::State,
};

/// Idle time after which a Claude Code session's helpers (or a headless OpenCode
/// service) are stopped, by pressure.
pub fn claude_idle_window_secs(pressure: Pressure) -> Option<u64> {
    match pressure {
        Pressure::Normal => None,
        Pressure::Warning => Some(60 * 60),
        Pressure::Critical => Some(15 * 60),
    }
}

pub const CODEX_IDLE_SECS: u64 = 30 * 60;
pub const USER_IDLE_SECS: u64 = 30 * 60;
/// What the desktop agent runtimes must hold for a restart to be worth it; under
/// memory pressure half of it is enough.
pub const CODEX_WORTH_RESTART_BYTES: u64 = 1 << 30;
pub const CODEX_RESTART_INTERVAL_SECS: u64 = 6 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    Orphan,
    IdleClaudeSession,
    IdleCodexRestart,
    IdleAntigravityRestart,
    IdleOpencodeRestart,
    IdleOpencodeService,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Orphan => "orphan",
            Self::IdleClaudeSession => "idle-claude-session",
            Self::IdleCodexRestart => "idle-codex-restart",
            Self::IdleAntigravityRestart => "idle-antigravity-restart",
            Self::IdleOpencodeRestart => "idle-opencode-restart",
            Self::IdleOpencodeService => "idle-opencode-service",
        }
    }

    pub fn for_app_restart(agent: AgentKind) -> Self {
        match agent {
            AgentKind::Codex => Self::IdleCodexRestart,
            AgentKind::Antigravity => Self::IdleAntigravityRestart,
            AgentKind::Opencode => Self::IdleOpencodeRestart,
            AgentKind::Claude | AgentKind::Other => Self::IdleCodexRestart,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    pub identity: Identity,
    pub exe_name: CompactString,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Action {
    /// Stop a helper and its descendants, listed deepest first.
    Stop {
        rule: Rule,
        root: Target,
        tree: Vec<Target>,
        footprint_bytes: u64,
        reason: String,
    },
    /// Gracefully stop a headless background service via SIGTERM only (never
    /// SIGKILL, preserving on-disk databases such as OpenCode's SQLite WAL).
    StopGraceful {
        rule: Rule,
        target: Target,
        footprint_bytes: u64,
        reason: String,
    },
    /// Quit the app that hosts Codex and start it again in the background.
    RestartCodex {
        rule: Rule,
        app: Target,
        app_exe: String,
        footprint_bytes: u64,
        reason: String,
    },
    /// Quit an idle desktop agent app (Antigravity / Antigravity US / OpenCode)
    /// and start its exact bundle again in the background.
    RestartApp {
        rule: Rule,
        agent: AgentKind,
        app: Target,
        app_exe: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<Target>,
        footprint_bytes: u64,
        reason: String,
    },
}

impl Action {
    pub fn footprint_bytes(&self) -> u64 {
        match self {
            Self::Stop {
                footprint_bytes, ..
            }
            | Self::StopGraceful {
                footprint_bytes, ..
            }
            | Self::RestartCodex {
                footprint_bytes, ..
            }
            | Self::RestartApp {
                footprint_bytes, ..
            } => *footprint_bytes,
        }
    }
}

fn idle_session_secs(
    snapshot: &Snapshot,
    ownership: &Ownership,
    state: &State,
    session_pid: i32,
    now: u64,
    window: u64,
) -> Option<u64> {
    let process = ownership.process(snapshot, session_pid)?;
    let idle = state.session_idle_secs_for(process.pid, snapshot.started(process), now)?;
    if idle < window {
        return None;
    }
    let session_tty = process.has_tty.then_some(process.tty_id);
    let session_helpers = ownership
        .claude_sessions
        .iter()
        .find(|s| s.pid == session_pid)
        .map(|s| s.helpers.as_slice())
        .unwrap_or(&[]);
    // A running shell, nested external agent (`codex`, `opencode`, `agy`),
    // direct agent child, non-helper terminal process, a freshly opened PTY
    // (`tty_id != session_tty`), or anything started within the window means
    // the session is working. (Any helper whose subtree contains a nested
    // `claude` or other protected process is also left alone per-tree in
    // `stop()`.)
    let is_busy_node = |child: &crate::model::Process, allow_inherited_tty: bool| -> bool {
        is_shell(snapshot, child)
            || is_codex(snapshot, child)
            || is_opencode_runtime(snapshot, child)
            || is_antigravity_cli(snapshot, child)
            || (child.ppid == session_pid && is_agent_host(snapshot, child))
            || child.age_secs < window
            || (child.has_tty && !(allow_inherited_tty && session_tty == Some(child.tty_id)))
    };
    for direct_pid in ownership.direct_child_pids(session_pid) {
        let Some(direct) = ownership.process(snapshot, direct_pid) else {
            continue;
        };
        let in_helper = session_helpers.contains(&direct_pid);
        if is_busy_node(direct, in_helper)
            || ownership.any_descendant(snapshot, direct_pid, |desc| is_busy_node(desc, in_helper))
        {
            return None;
        }
    }
    Some(idle)
}

/// Effective idle duration of a headless OpenCode background service:
/// minimum of real SQLite session idle (`opencode_activity_at`), service process
/// age, and newest descendant age. Returns `None` if the service is missing or
/// has active commands/shells/TTY/nested agents.
pub fn opencode_service_idle_secs(
    snapshot: &Snapshot,
    ownership: &Ownership,
    service_pid: i32,
    commands_empty: bool,
) -> Option<u64> {
    if !commands_empty {
        return None;
    }
    let process = ownership.process(snapshot, service_pid)?;
    let mut min_age = process.age_secs;
    let busy = ownership.any_descendant(snapshot, service_pid, |child| {
        if is_shell(snapshot, child) || child.has_tty || is_agent_host(snapshot, child) {
            return true;
        }
        min_age = min_age.min(child.age_secs);
        false
    });
    if busy {
        return None;
    }
    let disk_idle = snapshot
        .opencode_activity_at
        .map_or(process.age_secs, |at| snapshot.taken_at.saturating_sub(at));
    Some(disk_idle.min(min_age))
}

/// True when a headless OpenCode service has no commands/shells/TTY/agents and
/// both the service and all of its descendants are at least `window` seconds old
/// (so `watch` knows whether probing `opencode_activity_at` could fire).
fn opencode_service_structurally_idle(
    snapshot: &Snapshot,
    ownership: &Ownership,
    service_pid: i32,
    commands_empty: bool,
    window: u64,
) -> bool {
    if !commands_empty {
        return false;
    }
    let Some(process) = ownership.process(snapshot, service_pid) else {
        return false;
    };
    if process.age_secs < window {
        return false;
    }
    !ownership.any_descendant(snapshot, service_pid, |child| {
        is_shell(snapshot, child)
            || child.has_tty
            || is_agent_host(snapshot, child)
            || child.age_secs < window
    })
}

/// True when an orphan, idle-Claude, or pressure-idle headless OpenCode service
/// rule can fire without requiring footprints to decide.
pub fn has_stop_candidates(snapshot: &Snapshot, ownership: &Ownership, state: &State) -> bool {
    if ownership
        .orphans
        .iter()
        .any(|&root| !ownership.is_tree_protected(snapshot, root))
    {
        return true;
    }
    let pressure = state.effective_pressure(snapshot);
    if let Some(window) = claude_idle_window_secs(pressure) {
        let now = snapshot.taken_at;
        for session in &ownership.claude_sessions {
            if session.helpers.is_empty() {
                continue;
            }
            let inherited_tty = ownership
                .process(snapshot, session.pid)
                .filter(|p| p.has_tty)
                .map(|p| p.tty_id);
            if idle_session_secs(snapshot, ownership, state, session.pid, now, window).is_some()
                && session.helpers.iter().any(|&helper| {
                    !ownership.is_tree_protected_with_inherited_tty(snapshot, helper, inherited_tty)
                })
            {
                return true;
            }
        }
        if !ownership.has_opencode_clients() {
            for session in &ownership.agent_sessions {
                if session.agent == AgentKind::Opencode
                    && session.mode == SessionMode::Service
                    && opencode_service_idle_secs(
                        snapshot,
                        ownership,
                        session.pid,
                        session.commands.is_empty(),
                    )
                    .is_some_and(|idle| idle >= window)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Summary of which optional probes (`ioreg` HID idle, per-agent disk activity)
/// `watch` actually needs on this pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdleProbeNeeds {
    pub codex_app_age: Option<u64>,
    /// Per-`--app_data_dir` max Antigravity desktop app age `(dir_name, age_secs)`.
    pub antigravity_app_ages: SmallVec<[(CompactString, u64); 2]>,
    pub opencode_app_age: Option<u64>,
    pub opencode_service_pressure_idle: bool,
    pub opencode_service_normal_age: Option<u64>,
}

impl IdleProbeNeeds {
    pub fn needs_user_idle(&self) -> bool {
        self.codex_app_age.is_some()
            || !self.antigravity_app_ages.is_empty()
            || self.opencode_app_age.is_some()
            || self.opencode_service_normal_age.is_some()
    }
}

/// Determine which desktop apps and headless OpenCode services are structurally
/// eligible for idle reclamation (no running commands, outside cooldown) so
/// `watch` only runs `ioreg`, disk activity scans, and `top` when needed.
pub fn idle_probe_needs(
    snapshot: &Snapshot,
    ownership: &Ownership,
    state: &State,
) -> IdleProbeNeeds {
    let mut needs = IdleProbeNeeds::default();
    let now = snapshot.taken_at;

    for app in &ownership.desktop_apps {
        if app.runtimes.is_empty() || !app.commands.is_empty() {
            continue;
        }
        let Some(process) = ownership.process(snapshot, app.pid) else {
            continue;
        };
        let recent_restart = state
            .last_restart_at(app.agent, &app.bundle_path)
            .is_some_and(|at| now.saturating_sub(at) < CODEX_RESTART_INTERVAL_SECS);
        if recent_restart {
            continue;
        }
        match app.agent {
            AgentKind::Codex => {
                needs.codex_app_age = Some(
                    needs
                        .codex_app_age
                        .map_or(process.age_secs, |prev| prev.max(process.age_secs)),
                );
            }
            AgentKind::Antigravity => {
                let dir = app
                    .data_dir
                    .as_deref()
                    .unwrap_or(DEFAULT_ANTIGRAVITY_DATA_DIR);
                if let Some((_, age)) = needs
                    .antigravity_app_ages
                    .iter_mut()
                    .find(|(d, _)| d.as_str() == dir)
                {
                    *age = (*age).max(process.age_secs);
                } else {
                    needs
                        .antigravity_app_ages
                        .push((CompactString::from(dir), process.age_secs));
                }
            }
            AgentKind::Opencode => {
                needs.opencode_app_age = Some(
                    needs
                        .opencode_app_age
                        .map_or(process.age_secs, |prev| prev.max(process.age_secs)),
                );
            }
            AgentKind::Claude | AgentKind::Other => continue,
        }
    }

    if !ownership.has_opencode_clients() {
        let pressure = state.effective_pressure(snapshot);
        for session in &ownership.agent_sessions {
            if session.agent != AgentKind::Opencode || session.mode != SessionMode::Service {
                continue;
            }
            let Some(process) = ownership.process(snapshot, session.pid) else {
                continue;
            };
            if let Some(window) = claude_idle_window_secs(pressure) {
                if opencode_service_structurally_idle(
                    snapshot,
                    ownership,
                    session.pid,
                    session.commands.is_empty(),
                    window,
                ) {
                    needs.opencode_service_pressure_idle = true;
                }
            } else if opencode_service_structurally_idle(
                snapshot,
                ownership,
                session.pid,
                session.commands.is_empty(),
                CODEX_IDLE_SECS,
            ) {
                needs.opencode_service_normal_age = Some(
                    needs
                        .opencode_service_normal_age
                        .map_or(process.age_secs, |prev| prev.max(process.age_secs)),
                );
            }
        }
    }

    needs
}

pub fn plan(snapshot: &Snapshot, ownership: &Ownership, state: &State) -> Vec<Action> {
    let now = snapshot.taken_at;
    let pressure = state.effective_pressure(snapshot);
    let mut actions = Vec::new();

    for root in &ownership.orphans {
        actions.extend(stop(
            snapshot,
            ownership,
            *root,
            Rule::Orphan,
            None,
            "its agent session has exited".into(),
        ));
    }

    if let Some(window) = claude_idle_window_secs(pressure) {
        for session in &ownership.claude_sessions {
            if session.helpers.is_empty() {
                continue;
            }
            let Some(idle) =
                idle_session_secs(snapshot, ownership, state, session.pid, now, window)
            else {
                continue;
            };
            let inherited_tty = ownership
                .process(snapshot, session.pid)
                .filter(|p| p.has_tty)
                .map(|p| p.tty_id);
            for helper in &session.helpers {
                actions.extend(stop(
                    snapshot,
                    ownership,
                    *helper,
                    Rule::IdleClaudeSession,
                    inherited_tty,
                    format!(
                        "session {} idle for {} min under {:?} pressure",
                        session.pid,
                        idle / 60,
                        pressure
                    ),
                ));
            }
        }
    }

    for app in &ownership.desktop_apps {
        if let Some(action) = desktop_app_restart(snapshot, ownership, state, app, pressure) {
            actions.push(action);
        }
    }

    if !ownership.has_opencode_clients() {
        for session in &ownership.agent_sessions {
            if session.agent == AgentKind::Opencode
                && session.mode == SessionMode::Service
                && let Some(action) = opencode_service_stop(snapshot, ownership, session, pressure)
            {
                actions.push(action);
            }
        }
    }

    actions
}

fn desktop_app_restart(
    snapshot: &Snapshot,
    ownership: &Ownership,
    state: &State,
    app: &DesktopApp,
    pressure: Pressure,
) -> Option<Action> {
    let now = snapshot.taken_at;
    let process = ownership.process(snapshot, app.pid)?;
    // A running command/shell/subagent is work in progress however quiet it is;
    // without a runtime there is no runtime footprint to reclaim.
    if app.runtimes.is_empty() || !app.commands.is_empty() {
        return None;
    }
    let recent_restart = state
        .last_restart_at(app.agent, &app.bundle_path)
        .is_some_and(|at| now.saturating_sub(at) < CODEX_RESTART_INTERVAL_SECS);
    if recent_restart {
        return None;
    }
    let user_idle = snapshot.user_idle_secs?;
    if user_idle < USER_IDLE_SECS {
        return None;
    }
    let activity_at = match app.agent {
        AgentKind::Codex => snapshot.codex_activity_at,
        AgentKind::Antigravity => snapshot.antigravity_activity_for(app.data_dir.as_deref()),
        AgentKind::Opencode => snapshot.opencode_activity_at,
        AgentKind::Claude | AgentKind::Other => return None,
    };
    let agent_idle = activity_at.map_or(process.age_secs, |at| now.saturating_sub(at));
    if agent_idle < CODEX_IDLE_SECS {
        return None;
    }
    let footprint = ownership.desktop_app_footprint(snapshot, app);
    let needed = if pressure >= Pressure::Warning {
        CODEX_WORTH_RESTART_BYTES / 2
    } else {
        CODEX_WORTH_RESTART_BYTES
    };
    if footprint < needed {
        return None;
    }

    match app.agent {
        AgentKind::Codex => Some(Action::RestartCodex {
            rule: Rule::IdleCodexRestart,
            app: target(snapshot, process),
            app_exe: snapshot.exe(process).to_owned(),
            footprint_bytes: footprint,
            reason: format!(
                "Codex idle {} min, user idle {} min, Codex holds {} MB, {:?} pressure",
                agent_idle / 60,
                user_idle / 60,
                footprint >> 20,
                pressure
            ),
        }),
        AgentKind::Antigravity | AgentKind::Opencode => {
            let service = match app.agent {
                AgentKind::Opencode => {
                    // Only stop the detached OpenCode service when no external CLI
                    // client is sharing it.
                    let has_cli = ownership
                        .agent_sessions
                        .iter()
                        .any(|s| s.agent == AgentKind::Opencode && s.mode != SessionMode::Service);
                    if has_cli {
                        None
                    } else {
                        app.attached_service
                            .and_then(|pid| ownership.process(snapshot, pid))
                            .map(|svc| target(snapshot, svc))
                    }
                }
                // After Antigravity's main process exits, gracefully SIGTERM
                // its `language_server` if it did not exit with the main app.
                AgentKind::Antigravity => app
                    .runtimes
                    .first()
                    .and_then(|&pid| ownership.process(snapshot, pid))
                    .map(|rt| target(snapshot, rt)),
                AgentKind::Codex | AgentKind::Claude | AgentKind::Other => None,
            };
            Some(Action::RestartApp {
                rule: Rule::for_app_restart(app.agent),
                agent: app.agent,
                app: target(snapshot, process),
                app_exe: snapshot.exe(process).to_owned(),
                service,
                footprint_bytes: footprint,
                reason: format!(
                    "{} idle {} min, user idle {} min, holds {} MB, {:?} pressure",
                    app.app_name,
                    agent_idle / 60,
                    user_idle / 60,
                    footprint >> 20,
                    pressure
                ),
            })
        }
        AgentKind::Claude | AgentKind::Other => None,
    }
}

fn opencode_service_stop(
    snapshot: &Snapshot,
    ownership: &Ownership,
    session: &crate::owners::AgentSession,
    pressure: Pressure,
) -> Option<Action> {
    let process = ownership.process(snapshot, session.pid)?;
    let idle = opencode_service_idle_secs(
        snapshot,
        ownership,
        session.pid,
        session.commands.is_empty(),
    )?;
    let footprint = ownership.tree_footprint(snapshot, session.pid);
    if let Some(window) = claude_idle_window_secs(pressure) {
        if idle < window {
            return None;
        }
        return Some(Action::StopGraceful {
            rule: Rule::IdleOpencodeService,
            target: target(snapshot, process),
            footprint_bytes: footprint,
            reason: format!(
                "OpenCode service idle {} min with no open client under {:?} pressure",
                idle / 60,
                pressure
            ),
        });
    }
    // Normal pressure: only stop a headless OpenCode service when user is away,
    // service is idle >= 30 min, and it holds >= 1 GB.
    let user_idle = snapshot.user_idle_secs?;
    if user_idle < USER_IDLE_SECS || footprint < CODEX_WORTH_RESTART_BYTES || idle < CODEX_IDLE_SECS
    {
        return None;
    }
    Some(Action::StopGraceful {
        rule: Rule::IdleOpencodeService,
        target: target(snapshot, process),
        footprint_bytes: footprint,
        reason: format!(
            "OpenCode service idle {} min, user idle {} min, holds {} MB with no open client",
            idle / 60,
            user_idle / 60,
            footprint >> 20
        ),
    })
}

fn stop(
    snapshot: &Snapshot,
    ownership: &Ownership,
    root: i32,
    rule: Rule,
    inherited_tty: Option<u16>,
    reason: String,
) -> Option<Action> {
    let process = ownership.process(snapshot, root)?;
    let tree = ownership.tree(snapshot, root);
    // A helper that started an agent, a shell or a terminal program is doing
    // someone's work; it is left alone with everything under it.
    if tree
        .iter()
        .any(|member| ownership.is_protected_with_inherited_tty(snapshot, member, inherited_tty))
    {
        return None;
    }
    let footprint_bytes = tree
        .iter()
        .filter_map(|member| member.footprint_bytes)
        .sum();
    Some(Action::Stop {
        rule,
        root: target(snapshot, process),
        tree: tree.into_iter().map(|p| target(snapshot, p)).collect(),
        footprint_bytes,
        reason,
    })
}

fn target(snapshot: &Snapshot, process: &crate::model::Process) -> Target {
    Target {
        identity: snapshot.identity(process),
        exe_name: CompactString::from(snapshot.exe_name(process)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Action, Rule, plan};
    use crate::{
        model::{Pressure, Snapshot},
        owners::{
            Ownership,
            tests::{known_gopls, machine},
        },
        state::State,
    };

    fn rules(actions: &[Action]) -> Vec<(Rule, i32)> {
        actions
            .iter()
            .map(|action| match action {
                Action::Stop { rule, root, .. } => (*rule, root.identity.pid),
                Action::StopGraceful { rule, target, .. } => (*rule, target.identity.pid),
                Action::RestartCodex { rule, app, .. } | Action::RestartApp { rule, app, .. } => {
                    (*rule, app.identity.pid)
                }
            })
            .collect()
    }

    /// Observe the machine every minute for `idle_secs` while no session spends
    /// CPU, as the watch loop would.
    fn aged(snapshot: &mut Snapshot, idle_secs: u64) -> State {
        let mut state = State::default();
        let end = snapshot.taken_at + idle_secs;
        loop {
            let ownership = Ownership::new(snapshot, &state.known_helpers);
            state.observe(snapshot, &ownership);
            if snapshot.taken_at >= end {
                break;
            }
            snapshot.taken_at = (snapshot.taken_at + 60).min(end);
        }
        // gopls (402) was seen serving a session before it was orphaned.
        state.known_helpers.extend(known_gopls());
        state
    }

    #[test]
    fn orphans_are_stopped_at_every_pressure() {
        let mut snapshot = machine();
        let state = aged(&mut snapshot, 0);
        let ownership = Ownership::new(&snapshot, &state.known_helpers);
        let actions = plan(&snapshot, &ownership, &state);
        assert_eq!(rules(&actions), [(Rule::Orphan, 400), (Rule::Orphan, 402)]);
        let Action::Stop {
            tree,
            footprint_bytes,
            ..
        } = &actions[0]
        else {
            panic!("expected a stop");
        };
        assert_eq!(
            tree.iter().map(|t| t.identity.pid).collect::<Vec<_>>(),
            [401, 400]
        );
        assert_eq!(*footprint_bytes, 94 << 20);
    }

    #[test]
    fn idle_session_window_follows_pressure() {
        for (pressure, idle, expect_stops) in [
            (Pressure::Normal, 10 * 3600, false),
            (Pressure::Warning, 59 * 60, false),
            (Pressure::Warning, 61 * 60, true),
            (Pressure::Critical, 16 * 60, true),
        ] {
            let mut snapshot = machine();
            snapshot.pressure = pressure;
            let state = aged(&mut snapshot, idle);
            let ownership = Ownership::new(&snapshot, &state.known_helpers);
            let stopped: Vec<i32> = rules(&plan(&snapshot, &ownership, &state))
                .into_iter()
                .filter(|(rule, _)| *rule == Rule::IdleClaudeSession)
                .map(|(_, pid)| pid)
                .collect();
            // Session 110 runs a command in a shell, so only 120 is idle.
            let expected: &[i32] = if expect_stops { &[121, 122] } else { &[] };
            assert_eq!(stopped, expected, "{pressure:?} after {idle} s");
        }
    }

    fn codex_ready() -> (Snapshot, State) {
        let mut snapshot = machine();
        // No shell under the app, no rollout for 40 minutes, user away 45 minutes.
        snapshot.processes.retain(|p| p.pid != 213);
        snapshot.codex_activity_at = Some(snapshot.taken_at - 40 * 60);
        snapshot.user_idle_secs = Some(45 * 60);
        snapshot.pressure = Pressure::Warning;
        let state = State::default();
        (snapshot, state)
    }

    fn restarts(snapshot: &Snapshot, state: &State) -> bool {
        let ownership = Ownership::new(snapshot, &BTreeSet::new());
        plan(snapshot, &ownership, state)
            .iter()
            .any(|action| matches!(action, Action::RestartCodex { .. }))
    }

    #[test]
    fn codex_restarts_only_when_every_condition_holds() {
        let (snapshot, state) = codex_ready();
        assert!(restarts(&snapshot, &state));

        let (mut busy, state) = codex_ready();
        busy.codex_activity_at = Some(busy.taken_at - 10 * 60);
        assert!(!restarts(&busy, &state), "recent rollout write");

        let (mut shell, state) = codex_ready();
        crate::owners::tests::add_proc(
            &mut shell,
            214,
            210,
            "/bin/zsh",
            "/bin/zsh -lc make",
            300,
            3,
        );
        assert!(!restarts(&shell, &state), "a command started 5 minutes ago");

        // `zsh -lc 'cargo test'` execs cargo; it has run, silent, for 2 hours.
        let (mut long, state) = codex_ready();
        crate::owners::tests::add_proc(
            &mut long,
            214,
            210,
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
            7200,
            200,
        );
        assert!(!restarts(&long, &state), "a long silent command");

        let (mut present, state) = codex_ready();
        present.user_idle_secs = Some(5 * 60);
        assert!(!restarts(&present, &state), "user at the machine");

        let (mut unknown, state) = codex_ready();
        unknown.user_idle_secs = None;
        assert!(!restarts(&unknown, &state), "user idle unknown");

        let (mut cheap, state) = codex_ready();
        cheap.pressure = Pressure::Normal;
        assert!(
            !restarts(&cheap, &state),
            "Codex under 1 GB, normal pressure"
        );

        // Under pressure, Codex still has to hold half a gigabyte.
        let (mut small, state) = codex_ready();
        for process in &mut small.processes {
            if [210, 211, 212].contains(&process.pid) {
                process.footprint_bytes = Some(10 << 20);
            }
        }
        assert!(!restarts(&small, &state), "30 MB is not worth a restart");

        let (snapshot, mut recent) = codex_ready();
        recent.last_codex_restart_at = Some(snapshot.taken_at - 3600);
        assert!(!restarts(&snapshot, &recent), "restarted an hour ago");
    }

    #[test]
    fn helper_trees_with_an_agent_shell_or_terminal_are_left_alone() {
        let mut snapshot = machine();
        // Orphan 400's worker runs a shell; a Claude helper has started a CLI agent.
        crate::owners::tests::add_proc(
            &mut snapshot,
            405,
            401,
            "/bin/zsh",
            "zsh -c npm run dev",
            60,
            4,
        );
        crate::owners::tests::add_proc(
            &mut snapshot,
            123,
            122,
            "/Users/u/.local/bin/claude",
            "claude -p review",
            5000,
            200,
        );
        snapshot.pressure = Pressure::Critical;
        let state = aged(&mut snapshot, 20 * 60);
        let ownership = Ownership::new(&snapshot, &state.known_helpers);
        assert_eq!(
            rules(&plan(&snapshot, &ownership, &state)),
            [(Rule::Orphan, 402), (Rule::IdleClaudeSession, 121),]
        );
    }

    #[test]
    fn a_session_whose_child_started_within_the_window_is_working() {
        let mut snapshot = machine();
        snapshot.pressure = Pressure::Warning;
        // A command the session started 20 minutes ago, exec'd without a shell.
        crate::owners::tests::add_proc(
            &mut snapshot,
            124,
            120,
            "/Users/u/.cargo/bin/cargo",
            "cargo build",
            20 * 60,
            300,
        );
        let mut state = aged(&mut snapshot, 61 * 60);
        state.known_helpers.clear();
        let ownership = Ownership::new(&snapshot, &state.known_helpers);
        assert!(
            !rules(&plan(&snapshot, &ownership, &state))
                .iter()
                .any(|(rule, _)| *rule == Rule::IdleClaudeSession)
        );
    }

    #[test]
    fn antigravity_clone_restarts_only_when_idle_and_blocks_on_nested_codex_or_tty() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            pressure: Pressure::Warning,
            user_idle_secs: Some(45 * 60),
            antigravity_activity_at: Some(1_795_725_000 - 40 * 60),
            launchd_jobs: Some(vec![]),
            ..Snapshot::default()
        };
        crate::owners::tests::add_proc(
            &mut snapshot,
            44215,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real",
            90_000,
            300,
        );
        crate::owners::tests::add_proc(
            &mut snapshot,
            44926,
            44215,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity",
            90_000,
            4096,
        );
        crate::owners::tests::add_proc(
            &mut snapshot,
            44943,
            44926,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real --import=file:///Users/u/.gemini/antigravity/lib/node_modules/sidecar_sdk/register_loader.js main.mjs",
            90_000,
            37,
        );
        let state = State::default();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        let actions = plan(&snapshot, &ownership, &state);
        assert_eq!(rules(&actions), [(Rule::IdleAntigravityRestart, 44215)]);
        // Antigravity restart includes `language_server` as a graceful companion target.
        let Action::RestartApp { service, .. } = &actions[0] else {
            panic!("expected RestartApp");
        };
        assert_eq!(service.as_ref().map(|t| t.identity.pid), Some(44926));

        // Nested codex CLI under language_server blocks restart!
        let mut with_codex = snapshot.clone();
        crate::owners::tests::add_proc(
            &mut with_codex,
            50000,
            44926,
            "/Users/u/.nvm/versions/node/v26/bin/codex",
            "codex exec review",
            500,
            100,
        );
        let ownership = Ownership::new(&with_codex, &BTreeSet::new());
        assert!(plan(&with_codex, &ownership, &state).is_empty());

        // Active TTY command under language_server blocks restart!
        let mut with_tty = snapshot.clone();
        with_tty.push_process(
            50001,
            44926,
            501,
            "start-50001",
            10,
            true,
            0,
            Some(20 << 20),
            "/opt/homebrew/bin/python3",
            "python3 script.py",
        );
        let ownership = Ownership::new(&with_tty, &BTreeSet::new());
        assert!(plan(&with_tty, &ownership, &state).is_empty());

        // Recent conversation write blocks restart!
        let mut active_conv = snapshot.clone();
        active_conv.antigravity_activity_at = Some(active_conv.taken_at - 60);
        let ownership = Ownership::new(&active_conv, &BTreeSet::new());
        assert!(plan(&active_conv, &ownership, &state).is_empty());

        // Per-bundle cooldown blocks repeat restart of Antigravity US only.
        let mut cooled = State::default();
        cooled.record_restart(
            crate::model::AgentKind::Antigravity,
            "/Users/u/Applications/Antigravity US.app",
            snapshot.taken_at - 3600,
        );
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(plan(&snapshot, &ownership, &cooled).is_empty());
    }

    #[test]
    fn headless_opencode_service_stops_gracefully_only_when_no_client_is_open() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            pressure: Pressure::Warning,
            opencode_activity_at: Some(1_795_725_000 - 90 * 60),
            launchd_jobs: Some(vec![]),
            ..Snapshot::default()
        };
        crate::owners::tests::add_proc(
            &mut snapshot,
            37642,
            1,
            "/Users/u/Library/Application Support/ai.opencode.desktop/cli/2.4.1/opencode-cli",
            "opencode-cli serve --service",
            90_000,
            325,
        );
        // Fires directly from real SQLite session idle + process age without
        // depending on Claude's 50 centis/min CPU threshold (which OpenCode's
        // 5-minute background catalog tick exceeds while completely idle).
        let state = State::default();
        let ownership = Ownership::new(&snapshot, &state.known_helpers);
        let actions = plan(&snapshot, &ownership, &state);
        assert_eq!(rules(&actions), [(Rule::IdleOpencodeService, 37642)]);
        assert!(matches!(actions[0], Action::StopGraceful { .. }));

        // Recent child helper (< 60m under Warning) delays stop.
        let mut young_child = snapshot.clone();
        crate::owners::tests::add_proc(
            &mut young_child,
            37643,
            37642,
            "/Users/u/.codegraph/versions/v1.6.4/node",
            "node codegraph.js serve --mcp",
            30 * 60,
            50,
        );
        let ownership = Ownership::new(&young_child, &state.known_helpers);
        assert!(
            !rules(&plan(&young_child, &ownership, &state))
                .iter()
                .any(|(rule, _)| *rule == Rule::IdleOpencodeService)
        );

        // If an OpenCode CLI client is running, service is NOT stopped!
        let mut with_cli = snapshot.clone();
        crate::owners::tests::add_proc(
            &mut with_cli,
            38000,
            1,
            "/opt/homebrew/bin/opencode",
            "opencode",
            100,
            50,
        );
        let ownership = Ownership::new(&with_cli, &state.known_helpers);
        assert!(
            !rules(&plan(&with_cli, &ownership, &state))
                .iter()
                .any(|(rule, _)| *rule == Rule::IdleOpencodeService)
        );
    }

    #[test]
    fn nested_opencode_blocks_idle_claude_and_orphaned_language_server_is_reaped() {
        // Claude session 120 running a nested `opencode` (not just `codex exec`) is busy.
        let mut snapshot = machine();
        snapshot.pressure = Pressure::Critical;
        crate::owners::tests::add_proc(
            &mut snapshot,
            126,
            120,
            "/opt/homebrew/bin/opencode",
            "opencode run fix",
            5000,
            150,
        );
        let state = aged(&mut snapshot, 20 * 60);
        let ownership = Ownership::new(&snapshot, &state.known_helpers);
        assert!(
            !rules(&plan(&snapshot, &ownership, &state))
                .iter()
                .any(|(rule, _)| *rule == Rule::IdleClaudeSession)
        );

        // Orphaned desktop Antigravity `language_server` emits Orphan stop action.
        let mut orphan_snap = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
            ..Snapshot::default()
        };
        crate::owners::tests::add_proc(
            &mut orphan_snap,
            44926,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity --host_bridge_url=http://127.0.0.1:51982",
            90_000,
            18_000,
        );
        let ownership = Ownership::new(&orphan_snap, &BTreeSet::new());
        let actions = plan(&orphan_snap, &ownership, &State::default());
        assert_eq!(rules(&actions), [(Rule::Orphan, 44926)]);
    }

    #[test]
    fn terminal_claude_cli_reclaims_inherited_tty_mcp_helpers_safely() {
        use std::sync::Arc;

        use smallvec::smallvec;

        let make_snap = || {
            let mut snapshot = Snapshot {
                taken_at: 1_795_725_000,
                user: 501,
                pressure: Pressure::Critical,
                launchd_jobs: Some(vec![]),
                mcp_servers: Arc::from([crate::model::ServerSignature {
                    command: "npx".into(),
                    args: smallvec!["-y".into(), "@playwright/mcp@latest".into()],
                }]),
                ..Snapshot::default()
            };
            // Terminal `claude` CLI on `ttys001` (`0x8001`).
            snapshot.push_process_with_tty(
                150,
                1,
                501,
                "start-150",
                9000,
                0x8001,
                0,
                Some(200 << 20),
                "/Users/u/.local/share/claude/versions/2.1.322",
                "claude",
            );
            // Stdio MCP helper inheriting `ttys001`, plus its child `node` worker on `ttys001`.
            snapshot.push_process_with_tty(
                151,
                150,
                501,
                "start-151",
                9000,
                0x8001,
                0,
                Some(32 << 20),
                "npm exec @playwright/mcp@latest",
                "npm exec @playwright/mcp@latest",
            );
            snapshot.push_process_with_tty(
                152,
                151,
                501,
                "start-152",
                9000,
                0x8001,
                0,
                Some(96 << 20),
                "/opt/homebrew/bin/node",
                "/opt/homebrew/bin/node /Users/u/.npm/_npx/1/node_modules/@playwright/mcp/cli.js",
            );
            snapshot
        };

        // 1. Under Critical pressure after 20 min idle -> stops [152, 151] deepest-first!
        let mut snap = make_snap();
        let state = aged(&mut snap, 20 * 60);
        let ownership = Ownership::new(&snap, &state.known_helpers);
        assert!(super::has_stop_candidates(&snap, &ownership, &state));
        let actions = plan(&snap, &ownership, &state);
        assert_eq!(rules(&actions), [(Rule::IdleClaudeSession, 151)]);
        let Action::Stop {
            tree,
            footprint_bytes,
            ..
        } = &actions[0]
        else {
            panic!("expected Stop");
        };
        assert_eq!(
            tree.iter().map(|t| t.identity.pid).collect::<Vec<_>>(),
            [152, 151]
        );
        assert_eq!(*footprint_bytes, 128 << 20);

        // 2. Under Normal pressure -> never stopped.
        let mut normal = make_snap();
        normal.pressure = Pressure::Normal;
        let state_n = aged(&mut normal, 120 * 60);
        let own_n = Ownership::new(&normal, &state_n.known_helpers);
        assert!(plan(&normal, &own_n, &state_n).is_empty());

        // 3. Direct non-helper interactive command (`cargo test`) on same TTY blocks reclamation.
        let mut with_cmd = make_snap();
        with_cmd.push_process_with_tty(
            153,
            150,
            501,
            "start-153",
            9000,
            0x8001,
            0,
            Some(100 << 20),
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
        );
        let state_c = aged(&mut with_cmd, 20 * 60);
        let own_c = Ownership::new(&with_cmd, &state_c.known_helpers);
        assert!(plan(&with_cmd, &own_c, &state_c).is_empty());

        // 4. Helper child allocating a DIFFERENT PTY (`0x8002 != 0x8001`) blocks and is protected!
        let mut diff_tty = make_snap();
        diff_tty.push_process_with_tty(
            154,
            152,
            501,
            "start-154",
            9000,
            0x8002,
            0,
            Some(10 << 20),
            "/usr/bin/vim",
            "vim",
        );
        let state_d = aged(&mut diff_tty, 20 * 60);
        let own_d = Ownership::new(&diff_tty, &state_d.known_helpers);
        assert!(plan(&diff_tty, &own_d, &state_d).is_empty());

        // 4b. Helper descendant running an interactive editor (`fzf`/`vim`) on the SAME
        //     inherited TTY (`0x8001`) OR a VCS/system utility (`git`/`ssh`) is protected!
        let mut same_tty_editor = make_snap();
        same_tty_editor.push_process_with_tty(
            155,
            152,
            501,
            "start-155",
            9000,
            0x8001,
            0,
            Some(10 << 20),
            "/opt/homebrew/bin/fzf",
            "fzf",
        );
        let state_e = aged(&mut same_tty_editor, 20 * 60);
        let own_e = Ownership::new(&same_tty_editor, &state_e.known_helpers);
        assert!(plan(&same_tty_editor, &own_e, &state_e).is_empty());

        let mut helper_with_git = make_snap();
        helper_with_git.push_process_with_tty(
            156,
            152,
            501,
            "start-156",
            9000,
            0x8001,
            0,
            Some(10 << 20),
            "/usr/bin/git",
            "git fsmonitor--daemon",
        );
        let state_g = aged(&mut helper_with_git, 20 * 60);
        let own_g = Ownership::new(&helper_with_git, &state_g.known_helpers);
        assert!(plan(&helper_with_git, &own_g, &state_g).is_empty());

        // 5. Orphan rule NEVER stops a `ppid == 1` process that still has a TTY,
        //    and NEVER stops an orphan tree containing a VCS/system utility (`git`/`ssh`)!
        let mut orphan_tty = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
            mcp_servers: Arc::from([crate::model::ServerSignature {
                command: "railway".into(),
                args: smallvec!["mcp".into()],
            }]),
            ..Snapshot::default()
        };
        orphan_tty.push_process_with_tty(
            500,
            1,
            501,
            "start-500",
            9000,
            0x8001,
            0,
            Some(50 << 20),
            "/opt/homebrew/bin/railway",
            "railway mcp",
        );
        let own_o = Ownership::new(&orphan_tty, &BTreeSet::new());
        assert!(plan(&orphan_tty, &own_o, &State::default()).is_empty());

        let mut orphan_with_ssh = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
            mcp_servers: Arc::from([crate::model::ServerSignature {
                command: "railway".into(),
                args: smallvec!["mcp".into()],
            }]),
            ..Snapshot::default()
        };
        crate::owners::tests::add_proc(
            &mut orphan_with_ssh,
            501,
            1,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            9000,
            50,
        );
        crate::owners::tests::add_proc(
            &mut orphan_with_ssh,
            502,
            501,
            "/usr/bin/ssh",
            "ssh tunnel",
            9000,
            5,
        );
        let own_s = Ownership::new(&orphan_with_ssh, &BTreeSet::new());
        assert!(plan(&orphan_with_ssh, &own_s, &State::default()).is_empty());
    }

    #[test]
    fn multiple_antigravity_instances_track_app_data_dir_independently_and_other_tools_never_stop()
    {
        use std::sync::Arc;

        use smallvec::smallvec;

        let now = 1_795_725_000;
        let mut snapshot = Snapshot {
            taken_at: now,
            user: 501,
            pressure: Pressure::Critical,
            user_idle_secs: Some(45 * 60),
            launchd_jobs: Some(vec![100, 200, 300]),
            mcp_servers: Arc::from([crate::model::ServerSignature {
                command: "npx".into(),
                args: smallvec!["-y".into(), "@playwright/mcp@latest".into()],
            }]),
            ..Snapshot::default()
        };
        snapshot
            .antigravity_activities
            .insert("antigravity".into(), Some(now - 60)); // Active 1 min ago!
        snapshot
            .antigravity_activities
            .insert("antigravity-ide".into(), Some(now - 50 * 60)); // Idle 50 min!

        // Antigravity US (`--app_data_dir antigravity`) — ACTIVE.
        crate::owners::tests::add_proc(
            &mut snapshot,
            100,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real",
            90_000,
            300,
        );
        crate::owners::tests::add_proc(
            &mut snapshot,
            101,
            100,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity --app_data_dir antigravity",
            90_000,
            2048,
        );
        // Antigravity IDE (`--app_data_dir antigravity-ide`) — IDLE.
        crate::owners::tests::add_proc(
            &mut snapshot,
            200,
            1,
            "/Applications/Antigravity IDE.app/Contents/MacOS/Antigravity",
            "Antigravity",
            90_000,
            300,
        );
        crate::owners::tests::add_proc(
            &mut snapshot,
            201,
            200,
            "/Applications/Antigravity IDE.app/Contents/Resources/bin/language_server",
            "language_server --override_ide_name antigravity --app_data_dir antigravity-ide",
            90_000,
            2048,
        );
        // Generic `Cursor.app` holding > 2 GB of MCP helpers under Critical pressure -> NEVER stopped/restarted!
        crate::owners::tests::add_proc(
            &mut snapshot,
            300,
            1,
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
            "Cursor",
            90_000,
            600,
        );
        crate::owners::tests::add_proc(
            &mut snapshot,
            301,
            300,
            "/opt/homebrew/bin/node",
            "/opt/homebrew/bin/node /Users/u/.npm/_npx/1/node_modules/@playwright/mcp/cli.js",
            90_000,
            2500,
        );

        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        let needs = super::idle_probe_needs(&snapshot, &ownership, &State::default());
        assert_eq!(needs.antigravity_app_ages.len(), 2);

        let actions = plan(&snapshot, &ownership, &State::default());
        assert_eq!(rules(&actions), [(Rule::IdleAntigravityRestart, 200)]);
    }
}

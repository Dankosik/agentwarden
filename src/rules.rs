//! The rules: which processes to stop and when to restart Codex. Pure functions
//! of a snapshot, its ownership and the remembered state.

use serde::Serialize;

use crate::{
    model::{Identity, Pressure, Snapshot},
    owners::{Ownership, is_shell},
    state::State,
};

/// Idle time after which a Claude Code session's helpers are stopped, by pressure.
pub fn claude_idle_window_secs(pressure: Pressure) -> Option<u64> {
    match pressure {
        Pressure::Normal => None,
        Pressure::Warning => Some(60 * 60),
        Pressure::Critical => Some(15 * 60),
    }
}

pub const CODEX_IDLE_SECS: u64 = 30 * 60;
pub const USER_IDLE_SECS: u64 = 30 * 60;
/// What the Codex runtimes must hold for a restart to be worth it; under
/// memory pressure half of it is enough.
pub const CODEX_WORTH_RESTART_BYTES: u64 = 1 << 30;
pub const CODEX_RESTART_INTERVAL_SECS: u64 = 6 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    Orphan,
    IdleClaudeSession,
    IdleCodexRestart,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    pub identity: Identity,
    pub exe_name: String,
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
    /// Quit the app that hosts Codex and start it again in the background.
    RestartCodex {
        rule: Rule,
        app: Target,
        app_exe: String,
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
            | Self::RestartCodex {
                footprint_bytes, ..
            } => *footprint_bytes,
        }
    }
}

pub fn plan(snapshot: &Snapshot, ownership: &Ownership<'_>, state: &State) -> Vec<Action> {
    let now = snapshot.taken_at;
    let pressure = state.effective_pressure(snapshot);
    let mut actions = Vec::new();

    for root in &ownership.orphans {
        actions.extend(stop(
            ownership,
            *root,
            Rule::Orphan,
            "its agent session has exited".into(),
        ));
    }

    if let Some(window) = claude_idle_window_secs(pressure) {
        for session in &ownership.claude_sessions {
            let Some(process) = ownership.process(session.pid) else {
                continue;
            };
            let Some(idle) = state.session_idle_secs(&process.identity(), now) else {
                continue;
            };
            // A running command, however old, means the session is working.
            let busy_shell = ownership
                .descendants(session.pid)
                .into_iter()
                .filter_map(|pid| ownership.process(pid))
                .any(is_shell);
            if idle < window || busy_shell {
                continue;
            }
            for helper in &session.helpers {
                actions.extend(stop(
                    ownership,
                    *helper,
                    Rule::IdleClaudeSession,
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

    if let Some(action) = codex_restart(snapshot, ownership, state, pressure) {
        actions.push(action);
    }
    actions
}

fn codex_restart(
    snapshot: &Snapshot,
    ownership: &Ownership<'_>,
    state: &State,
    pressure: Pressure,
) -> Option<Action> {
    let now = snapshot.taken_at;
    let app = ownership.codex_app.as_ref()?;
    let process = ownership.process(app.pid)?;
    // A running command is work in progress however quiet it is.
    if !app.commands.is_empty() {
        return None;
    }
    let codex_idle = snapshot
        .codex_activity_at
        .map_or(process.age_secs, |at| now.saturating_sub(at));
    let user_idle = snapshot.user_idle_secs?;
    let footprint: u64 = app
        .runtimes
        .iter()
        .map(|runtime| ownership.tree_footprint(*runtime))
        .sum();
    let recent_restart = state
        .last_codex_restart_at
        .is_some_and(|at| now.saturating_sub(at) < CODEX_RESTART_INTERVAL_SECS);
    let needed = if pressure >= Pressure::Warning {
        CODEX_WORTH_RESTART_BYTES / 2
    } else {
        CODEX_WORTH_RESTART_BYTES
    };
    if codex_idle < CODEX_IDLE_SECS
        || user_idle < USER_IDLE_SECS
        || footprint < needed
        || recent_restart
    {
        return None;
    }
    Some(Action::RestartCodex {
        rule: Rule::IdleCodexRestart,
        app: target(process),
        app_exe: process.exe.clone(),
        footprint_bytes: footprint,
        reason: format!(
            "Codex idle {} min, user idle {} min, Codex holds {} MB, {:?} pressure",
            codex_idle / 60,
            user_idle / 60,
            footprint >> 20,
            pressure
        ),
    })
}

fn stop(ownership: &Ownership<'_>, root: i32, rule: Rule, reason: String) -> Option<Action> {
    let process = ownership.process(root)?;
    // A helper that started an agent, a shell or a terminal program is doing
    // someone's work; it is left alone with everything under it.
    if std::iter::once(root)
        .chain(ownership.descendants(root))
        .any(|pid| ownership.is_protected(pid))
    {
        return None;
    }
    Some(Action::Stop {
        rule,
        root: target(process),
        tree: ownership.tree(root).into_iter().map(target).collect(),
        footprint_bytes: ownership.tree_footprint(root),
        reason,
    })
}

fn target(process: &crate::model::Process) -> Target {
    Target {
        identity: process.identity(),
        exe_name: process.exe_name().to_owned(),
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
                Action::RestartCodex { rule, app, .. } => (*rule, app.identity.pid),
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
        shell.processes.push(crate::owners::tests::proc(
            214,
            210,
            "/bin/zsh",
            "/bin/zsh -lc make",
            300,
            3,
        ));
        assert!(!restarts(&shell, &state), "a command started 5 minutes ago");

        // `zsh -lc 'cargo test'` execs cargo; it has run, silent, for 2 hours.
        let (mut long, state) = codex_ready();
        long.processes.push(crate::owners::tests::proc(
            214,
            210,
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
            7200,
            200,
        ));
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
        snapshot.processes.push(crate::owners::tests::proc(
            405,
            401,
            "/bin/zsh",
            "zsh -c npm run dev",
            60,
            4,
        ));
        snapshot.processes.push(crate::owners::tests::proc(
            123,
            122,
            "/Users/u/.local/bin/claude",
            "claude -p review",
            5000,
            200,
        ));
        snapshot.pressure = Pressure::Critical;
        let state = aged(&mut snapshot, 20 * 60);
        let ownership = Ownership::new(&snapshot, &state.known_helpers);
        assert_eq!(
            rules(&plan(&snapshot, &ownership, &state)),
            [(Rule::Orphan, 402), (Rule::IdleClaudeSession, 121),]
        );
    }
}

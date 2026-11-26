//! The rules: which processes to stop and when to restart Codex. Pure functions
//! of a snapshot, its ownership and the remembered state.

use compact_str::CompactString;
use serde::Serialize;

use crate::{
    model::{Identity, Pressure, Snapshot},
    owners::{Ownership, is_protected_process, is_shell},
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

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Orphan => "orphan",
            Self::IdleClaudeSession => "idle-claude-session",
            Self::IdleCodexRestart => "idle-codex-restart",
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
    // A running shell, however old, or anything started within the
    // window means the session is working. A shell that runs a single
    // command execs it, so a long command can be a direct child.
    let busy = ownership.any_descendant(snapshot, session_pid, |child| {
        is_shell(snapshot, child) || child.age_secs < window
    });
    (!busy).then_some(idle)
}

/// True when the orphan or idle-Claude rules would emit at least one stop
/// action, so `watch` knows whether process footprints need to be sampled.
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
            if idle_session_secs(snapshot, ownership, state, session.pid, now, window).is_some()
                && session
                    .helpers
                    .iter()
                    .any(|&helper| !ownership.is_tree_protected(snapshot, helper))
            {
                return true;
            }
        }
    }
    false
}

/// When the Codex app is running with no commands and outside its cooldown,
/// returns the app's age in seconds so `watch` knows whether to query HID idle
/// and Codex rollout activity.
pub fn codex_eligible_for_idle_check(
    snapshot: &Snapshot,
    ownership: &Ownership,
    state: &State,
) -> Option<u64> {
    let app = ownership.codex_app.as_ref()?;
    let process = ownership.process(snapshot, app.pid)?;
    // Without any Codex runtime, runtime footprint is guaranteed 0 (< 512 MiB).
    if app.runtimes.is_empty() || !app.commands.is_empty() {
        return None;
    }
    let recent_restart = state
        .last_codex_restart_at
        .is_some_and(|at| snapshot.taken_at.saturating_sub(at) < CODEX_RESTART_INTERVAL_SECS);
    (!recent_restart).then_some(process.age_secs)
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
            for helper in &session.helpers {
                actions.extend(stop(
                    snapshot,
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
    ownership: &Ownership,
    state: &State,
    pressure: Pressure,
) -> Option<Action> {
    let now = snapshot.taken_at;
    let app = ownership.codex_app.as_ref()?;
    let process = ownership.process(snapshot, app.pid)?;
    // A running command is work in progress however quiet it is; without a
    // Codex runtime there is no Codex footprint to reclaim.
    if app.runtimes.is_empty() || !app.commands.is_empty() {
        return None;
    }
    let recent_restart = state
        .last_codex_restart_at
        .is_some_and(|at| now.saturating_sub(at) < CODEX_RESTART_INTERVAL_SECS);
    if recent_restart {
        return None;
    }
    let user_idle = snapshot.user_idle_secs?;
    if user_idle < USER_IDLE_SECS {
        return None;
    }
    let codex_idle = snapshot
        .codex_activity_at
        .map_or(process.age_secs, |at| now.saturating_sub(at));
    if codex_idle < CODEX_IDLE_SECS {
        return None;
    }
    let footprint: u64 = app
        .runtimes
        .iter()
        .map(|runtime| ownership.tree_footprint(snapshot, *runtime))
        .sum();
    let needed = if pressure >= Pressure::Warning {
        CODEX_WORTH_RESTART_BYTES / 2
    } else {
        CODEX_WORTH_RESTART_BYTES
    };
    if footprint < needed {
        return None;
    }
    Some(Action::RestartCodex {
        rule: Rule::IdleCodexRestart,
        app: target(snapshot, process),
        app_exe: snapshot.exe(process).to_owned(),
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

fn stop(
    snapshot: &Snapshot,
    ownership: &Ownership,
    root: i32,
    rule: Rule,
    reason: String,
) -> Option<Action> {
    let process = ownership.process(snapshot, root)?;
    let tree = ownership.tree(snapshot, root);
    // A helper that started an agent, a shell or a terminal program is doing
    // someone's work; it is left alone with everything under it.
    if tree
        .iter()
        .any(|member| is_protected_process(snapshot, member))
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
}

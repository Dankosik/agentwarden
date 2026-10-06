//! What agentwarden remembers between passes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    model::{Identity, Pressure, Snapshot},
    owners::Ownership,
};

/// A waiting Claude Code session spends 0.2–0.45 s of CPU per minute on its own
/// timers (measured 2026-10-06); a working one spends more.
pub const IDLE_CPU_CENTIS_PER_MINUTE: u64 = 50;

/// Swap growth below this between passes is noise, not a trend.
const SWAP_GROWTH_SLACK_BYTES: u64 = 64 << 20;

/// Passes run about a minute apart; a longer gap means the Mac slept or
/// agentwarden was stopped, and says nothing about whether a session worked.
pub const SAMPLE_GAP_SECS: u64 = 3 * 60;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Processes seen serving a live agent; they stay known after orphaning.
    pub known_helpers: BTreeSet<Identity>,
    pub sessions: BTreeMap<String, SessionActivity>,
    pub last_swap_used_bytes: Option<u64>,
    pub last_codex_restart_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    pub cpu_centis: u64,
    pub sampled_at: u64,
    /// Last time the session spent more than idle CPU.
    pub active_at: u64,
}

/// Session keys are text so the state file stays plain JSON.
pub fn session_key(identity: &Identity) -> String {
    format!("{}@{}", identity.pid, identity.started)
}

impl State {
    /// Pressure rules act on: the kernel's level, raised to Warning while swap
    /// grows. How full swap is says nothing: macOS adds swap files as needed,
    /// so the space it has allocated is nearly always nearly full.
    pub fn effective_pressure(&self, snapshot: &Snapshot) -> Pressure {
        let growing = self
            .last_swap_used_bytes
            .is_some_and(|last| snapshot.swap.used_bytes > last + SWAP_GROWTH_SLACK_BYTES);
        if growing {
            snapshot.pressure.max(Pressure::Warning)
        } else {
            snapshot.pressure
        }
    }

    /// Seconds the session has been idle, or `None` before it was first sampled
    /// and on the first pass after a gap, which proves nothing either way.
    pub fn session_idle_secs(&self, identity: &Identity, now: u64) -> Option<u64> {
        self.sessions
            .get(&session_key(identity))
            .filter(|activity| now.saturating_sub(activity.sampled_at) <= SAMPLE_GAP_SECS)
            .map(|activity| now.saturating_sub(activity.active_at))
    }

    /// Record this pass. Call after rules were evaluated against the old state.
    pub fn observe(&mut self, snapshot: &Snapshot, ownership: &Ownership<'_>) {
        let now = snapshot.taken_at;
        let alive: BTreeSet<Identity> = snapshot.processes.iter().map(|p| p.identity()).collect();
        self.known_helpers
            .retain(|identity| alive.contains(identity));
        self.known_helpers.extend(ownership.helper_identities());

        let mut sessions = BTreeMap::new();
        for session in &ownership.claude_sessions {
            let Some(process) = ownership.process(session.pid) else {
                continue;
            };
            let key = session_key(&process.identity());
            let activity = match self.sessions.get(&key) {
                Some(previous) => {
                    let elapsed = now.saturating_sub(previous.sampled_at).max(1);
                    let spent = process.cpu_centis.saturating_sub(previous.cpu_centis);
                    // A process started since the last pass is a tool call, even
                    // when it cost the session itself little CPU.
                    let started_something = ownership
                        .descendants(session.pid)
                        .into_iter()
                        .filter_map(|pid| ownership.process(pid))
                        .any(|child| child.age_secs <= elapsed);
                    let busy = elapsed > SAMPLE_GAP_SECS
                        || started_something
                        || spent * 60 > IDLE_CPU_CENTIS_PER_MINUTE * elapsed;
                    SessionActivity {
                        cpu_centis: process.cpu_centis,
                        sampled_at: now,
                        active_at: if busy { now } else { previous.active_at },
                    }
                }
                None => SessionActivity {
                    cpu_centis: process.cpu_centis,
                    sampled_at: now,
                    active_at: now,
                },
            };
            sessions.insert(key, activity);
        }
        self.sessions = sessions;
        self.last_swap_used_bytes = Some(snapshot.swap.used_bytes);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::State;
    use crate::{
        model::{Pressure, Swap},
        owners::{Ownership, tests::machine},
    };

    #[test]
    fn swap_trend_raises_kernel_pressure() {
        let mut snapshot = machine();
        let mut state = State::default();
        snapshot.swap = Swap {
            used_bytes: 4 << 30,
            total_bytes: 6 << 30,
        };
        assert_eq!(state.effective_pressure(&snapshot), Pressure::Normal);
        state.last_swap_used_bytes = Some(3 << 30);
        assert_eq!(state.effective_pressure(&snapshot), Pressure::Warning);
        // Allocated swap 92% used but not growing: macOS just has not added a file.
        snapshot.swap.used_bytes = (5.5 * (1u64 << 30) as f64) as u64;
        state.last_swap_used_bytes = Some(snapshot.swap.used_bytes);
        assert_eq!(state.effective_pressure(&snapshot), Pressure::Normal);
        snapshot.swap = Swap::default();
        snapshot.pressure = Pressure::Warning;
        assert_eq!(state.effective_pressure(&snapshot), Pressure::Warning);
    }

    #[test]
    fn session_idles_until_it_spends_more_than_timer_cpu() {
        let mut snapshot = machine();
        let mut state = State::default();
        let observe = |state: &mut State, snapshot: &crate::model::Snapshot| {
            let ownership = Ownership::new(snapshot, &BTreeSet::new());
            state.observe(snapshot, &ownership);
        };
        let session = snapshot
            .processes
            .iter()
            .find(|p| p.pid == 110)
            .unwrap()
            .identity();
        // Its shell and command started long ago; ages stay fixed in this fixture.
        for process in &mut snapshot.processes {
            if [113, 114].contains(&process.pid) {
                process.age_secs = 10_000;
            }
        }
        let start = snapshot.taken_at;
        observe(&mut state, &snapshot);
        assert_eq!(state.session_idle_secs(&session, start), Some(0));

        // Ten minutes at 0.3 s per minute: still idle.
        for _ in 0..10 {
            snapshot.taken_at += 60;
            snapshot
                .processes
                .iter_mut()
                .find(|p| p.pid == 110)
                .unwrap()
                .cpu_centis += 30;
            observe(&mut state, &snapshot);
        }
        assert_eq!(
            state.session_idle_secs(&session, snapshot.taken_at),
            Some(600)
        );

        // One busy minute resets the clock.
        snapshot.taken_at += 60;
        snapshot
            .processes
            .iter_mut()
            .find(|p| p.pid == 110)
            .unwrap()
            .cpu_centis += 200;
        observe(&mut state, &snapshot);
        assert_eq!(
            state.session_idle_secs(&session, snapshot.taken_at),
            Some(0)
        );

        // History keeps only live helpers and live sessions.
        assert!(
            state
                .known_helpers
                .iter()
                .any(|identity| identity.pid == 111)
        );
        snapshot.processes.retain(|p| p.pid != 120 && p.pid != 121);
        observe(&mut state, &snapshot);
        assert_eq!(state.sessions.len(), 1);
        assert!(
            !state
                .known_helpers
                .iter()
                .any(|identity| identity.pid == 121)
        );
    }

    #[test]
    fn sleep_gaps_and_new_tool_processes_are_not_idleness() {
        let mut snapshot = machine();
        let mut state = State::default();
        let observe = |state: &mut State, snapshot: &crate::model::Snapshot| {
            let ownership = Ownership::new(snapshot, &BTreeSet::new());
            state.observe(snapshot, &ownership);
        };
        let session = snapshot
            .processes
            .iter()
            .find(|p| p.pid == 120)
            .unwrap()
            .identity();
        observe(&mut state, &snapshot);
        for _ in 0..10 {
            snapshot.taken_at += 66;
            observe(&mut state, &snapshot);
        }
        assert_eq!(
            state.session_idle_secs(&session, snapshot.taken_at),
            Some(660)
        );

        // The lid was closed for 8 hours: the first pass after it decides nothing,
        // and the session's clock starts again.
        snapshot.taken_at += 8 * 3600;
        assert_eq!(state.session_idle_secs(&session, snapshot.taken_at), None);
        observe(&mut state, &snapshot);
        assert_eq!(
            state.session_idle_secs(&session, snapshot.taken_at),
            Some(0)
        );

        // A respawned MCP server means a tool call happened.
        snapshot.taken_at += 600;
        for _ in 0..5 {
            snapshot.taken_at += 60;
            observe(&mut state, &snapshot);
        }
        snapshot.taken_at += 60;
        snapshot.processes.push(crate::owners::tests::proc(
            125,
            120,
            "/opt/homebrew/bin/gopls",
            "gopls mcp",
            20,
            50,
        ));
        observe(&mut state, &snapshot);
        assert_eq!(
            state.session_idle_secs(&session, snapshot.taken_at),
            Some(0)
        );
    }

    #[test]
    fn idle_tolerance_follows_the_real_pass_interval() {
        let mut snapshot = machine();
        let mut state = State::default();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        state.observe(&snapshot, &ownership);
        let session = snapshot
            .processes
            .iter()
            .find(|p| p.pid == 120)
            .unwrap()
            .identity();
        // 0.6 s in 66 s is above 0.5 s per minute: working.
        snapshot.taken_at += 66;
        snapshot
            .processes
            .iter_mut()
            .find(|p| p.pid == 120)
            .unwrap()
            .cpu_centis += 60;
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        state.observe(&snapshot, &ownership);
        snapshot.taken_at += 60;
        assert_eq!(
            state.session_idle_secs(&session, snapshot.taken_at),
            Some(60)
        );
    }
}

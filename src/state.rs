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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Pressure rules act on: the kernel's level, raised by swap trends.
    pub fn effective_pressure(&self, snapshot: &Snapshot) -> Pressure {
        let swap = snapshot.swap;
        let from_swap = if swap.total_bytes > 0 && swap.used_bytes * 10 >= swap.total_bytes * 9 {
            Pressure::Critical
        } else if self
            .last_swap_used_bytes
            .is_some_and(|last| swap.used_bytes > last + SWAP_GROWTH_SLACK_BYTES)
        {
            Pressure::Warning
        } else {
            Pressure::Normal
        };
        snapshot.pressure.max(from_swap)
    }

    /// Seconds the session has been idle, or `None` before it was first sampled.
    pub fn session_idle_secs(&self, identity: &Identity, now: u64) -> Option<u64> {
        self.sessions
            .get(&session_key(identity))
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
                    let minutes = now.saturating_sub(previous.sampled_at).max(1).div_ceil(60);
                    let spent = process.cpu_centis.saturating_sub(previous.cpu_centis);
                    let busy = spent > IDLE_CPU_CENTIS_PER_MINUTE * minutes;
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
        snapshot.swap.used_bytes = (5.5 * (1u64 << 30) as f64) as u64;
        assert_eq!(state.effective_pressure(&snapshot), Pressure::Critical);
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
}

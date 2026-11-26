//! What agentwarden remembers between passes.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write as _},
};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use crate::{
    model::{AgentKind, Identity, Pressure, Snapshot},
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
    /// Per-app-bundle restart timestamps (e.g. `"/Users/.../Antigravity US.app"` -> unix secs).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_app_restarts: BTreeMap<String, u64>,
    /// Legacy single pending relaunch (kept for backwards compatibility with existing `state.json`).
    pub pending_relaunch: Option<PendingRelaunch>,
    /// Additional pending relaunches when more than one app is waiting to reopen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_relaunches: Vec<PendingRelaunch>,
}

fn default_relaunch_rule() -> CompactString {
    CompactString::const_new("idle-codex-restart")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRelaunch {
    pub bundle: std::path::PathBuf,
    pub since: u64,
    #[serde(default = "default_relaunch_rule")]
    pub rule: CompactString,
}

/// After this, a still closed app is the user's choice, not agentwarden's.
pub const RELAUNCH_WINDOW_SECS: u64 = 10 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    pub cpu_centis: u64,
    pub sampled_at: u64,
    /// Last time the session spent more than idle CPU.
    pub active_at: u64,
}

fn with_session_key<R>(pid: i32, started: &str, f: impl FnOnce(&str) -> R) -> R {
    let mut stack = [0u8; 64];
    let mut cursor = io::Cursor::new(&mut stack[..]);
    if write!(cursor, "{pid}@{started}").is_ok() {
        let len = cursor.position() as usize;
        if let Ok(key) = std::str::from_utf8(&stack[..len]) {
            return f(key);
        }
    }
    f(&format!("{pid}@{started}"))
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
    #[cfg(test)]
    pub fn session_idle_secs(&self, identity: &Identity, now: u64) -> Option<u64> {
        self.session_idle_secs_for(identity.pid, &identity.started, now)
    }

    /// Zero-allocation lookup by `(pid, started)`.
    pub fn session_idle_secs_for(&self, pid: i32, started: &str, now: u64) -> Option<u64> {
        with_session_key(pid, started, |key| {
            self.sessions
                .get(key)
                .filter(|activity| now.saturating_sub(activity.sampled_at) <= SAMPLE_GAP_SECS)
                .map(|activity| now.saturating_sub(activity.active_at))
        })
    }

    /// Last restart timestamp for `bundle` (combining legacy `last_codex_restart_at`
    /// for Codex desktop apps with per-bundle `last_app_restarts`).
    pub fn last_restart_at(&self, agent: AgentKind, bundle: &str) -> Option<u64> {
        let per_bundle = self.last_app_restarts.get(bundle).copied();
        if agent == AgentKind::Codex {
            match (self.last_codex_restart_at, per_bundle) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            }
        } else {
            per_bundle
        }
    }

    /// Record a restart attempt for `bundle`. Returns whether persisted state changed.
    pub fn record_restart(&mut self, agent: AgentKind, bundle: &str, at: u64) -> bool {
        let mut changed = false;
        if agent == AgentKind::Codex && self.last_codex_restart_at != Some(at) {
            self.last_codex_restart_at = Some(at);
            changed = true;
        }
        if self.last_app_restarts.get(bundle).copied() != Some(at) {
            self.last_app_restarts.insert(bundle.to_owned(), at);
            changed = true;
        }
        changed
    }

    /// Record this pass. Call after rules were evaluated against the old state.
    /// Returns whether any persisted field changed.
    pub fn observe(&mut self, snapshot: &Snapshot, ownership: &Ownership) -> bool {
        let now = snapshot.taken_at;
        let helpers_before = self.known_helpers.len();
        self.known_helpers.retain(|identity| {
            ownership
                .process(snapshot, identity.pid)
                .is_some_and(|process| snapshot.started(process) == identity.started)
        });
        let helpers_after_retain = self.known_helpers.len();
        ownership.extend_helper_identities(snapshot, &mut self.known_helpers);
        let mut changed = helpers_after_retain != helpers_before
            || self.known_helpers.len() != helpers_after_retain;

        let tracked_pids: smallvec::SmallVec<[i32; 8]> = ownership
            .claude_sessions
            .iter()
            .map(|session| session.pid)
            .collect();

        if !self.sessions.is_empty() || !tracked_pids.is_empty() {
            if tracked_pids.is_empty() {
                if !self.sessions.is_empty() {
                    self.sessions.clear();
                    changed = true;
                }
            } else {
                // Update existing sessions in place so steady-state passes with
                // live sessions allocate zero `BTreeMap` nodes.
                let prev_len = self.sessions.len();
                let mut existing_matched = 0usize;
                for &session_pid in &tracked_pids {
                    let Some(process) = ownership.process(snapshot, session_pid) else {
                        continue;
                    };
                    let mut inserted = None;
                    with_session_key(process.pid, snapshot.started(process), |key_str| {
                        if let Some(previous) = self.sessions.get_mut(key_str) {
                            existing_matched += 1;
                            let elapsed = now.saturating_sub(previous.sampled_at).max(1);
                            let spent = process.cpu_centis.saturating_sub(previous.cpu_centis);
                            // A process started since the last pass is a tool call, even
                            // when it cost the session itself little CPU.
                            let busy = elapsed > SAMPLE_GAP_SECS
                                || spent * 60 > IDLE_CPU_CENTIS_PER_MINUTE * elapsed
                                || ownership.any_descendant(snapshot, session_pid, |child| {
                                    child.age_secs <= elapsed
                                });
                            let next = SessionActivity {
                                cpu_centis: process.cpu_centis,
                                sampled_at: now,
                                active_at: if busy { now } else { previous.active_at },
                            };
                            if next != *previous {
                                *previous = next;
                                changed = true;
                            }
                        } else {
                            changed = true;
                            inserted = Some((
                                key_str.to_owned(),
                                SessionActivity {
                                    cpu_centis: process.cpu_centis,
                                    sampled_at: now,
                                    active_at: now,
                                },
                            ));
                        }
                    });
                    if let Some((key, activity)) = inserted {
                        self.sessions.insert(key, activity);
                    }
                }
                if existing_matched < prev_len {
                    // At least one previously tracked session exited; drop only
                    // keys that are no longer among live tracked sessions.
                    self.sessions.retain(|key, _| {
                        tracked_pids.iter().any(|&session_pid| {
                            ownership.process(snapshot, session_pid).is_some_and(|p| {
                                with_session_key(p.pid, snapshot.started(p), |k| k == key)
                            })
                        })
                    });
                    changed = true;
                }
            }
        }
        let next_swap = Some(snapshot.swap.used_bytes);
        if self.last_swap_used_bytes != next_swap {
            self.last_swap_used_bytes = next_swap;
            changed = true;
        }
        changed
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
        let session = snapshot.identity(snapshot.processes.iter().find(|p| p.pid == 110).unwrap());
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
        let session = snapshot.identity(snapshot.processes.iter().find(|p| p.pid == 120).unwrap());
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
        crate::owners::tests::add_proc(
            &mut snapshot,
            125,
            120,
            "/opt/homebrew/bin/gopls",
            "gopls mcp",
            20,
            50,
        );
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
        let session = snapshot.identity(snapshot.processes.iter().find(|p| p.pid == 120).unwrap());
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

    #[test]
    fn observe_reports_changes_accurately_and_handles_long_session_keys() {
        let mut snapshot = crate::model::Snapshot {
            taken_at: 1000,
            user: 501,
            swap: Swap {
                used_bytes: 1 << 30,
                total_bytes: 2 << 30,
            },
            ..Default::default()
        };
        let mut state = State::default();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        // First observe sets last_swap_used_bytes.
        assert!(state.observe(&snapshot, &ownership));
        // Identical pass reports no change.
        snapshot.taken_at += 60;
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(!state.observe(&snapshot, &ownership));

        // Swap change reports changed.
        snapshot.swap.used_bytes += 4096;
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(state.observe(&snapshot, &ownership));
        assert!(!state.observe(&snapshot, &ownership));

        // Long `started` string (> 64 bytes) exercises the heap fallback in `with_session_key`.
        let long_started = "x".repeat(80);
        snapshot.push_process(
            500,
            1,
            501,
            &long_started,
            100,
            false,
            0,
            Some(10 << 20),
            "/bin/claude",
            "claude",
        );
        let id = snapshot.identity(snapshot.processes.last().unwrap());
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(state.observe(&snapshot, &ownership));
        assert_eq!(state.session_idle_secs(&id, snapshot.taken_at), Some(0));

        // Simultaneous exit of session 500 and arrival of session 501 in the same pass
        // (same timestamp!) must drop 500 and keep 501.
        snapshot.processes.clear();
        crate::owners::tests::add_proc(&mut snapshot, 501, 1, "/bin/claude", "claude", 10, 10);
        let id_new = snapshot.identity(snapshot.processes.last().unwrap());
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(state.observe(&snapshot, &ownership));
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.session_idle_secs(&id, snapshot.taken_at), None);
        assert_eq!(state.session_idle_secs(&id_new, snapshot.taken_at), Some(0));

        // Removing the session reports changed.
        snapshot.processes.clear();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(state.observe(&snapshot, &ownership));
        assert!(state.sessions.is_empty());

        // Duplicate PIDs in a synthetic snapshot must not prevent pruning an exited session.
        crate::owners::tests::add_proc(&mut snapshot, 600, 1, "/bin/claude", "claude", 10, 10);
        crate::owners::tests::add_proc(&mut snapshot, 601, 1, "/bin/claude", "claude", 10, 10);
        let id_600 = snapshot.identity(&snapshot.processes[0]);
        let id_601 = snapshot.identity(&snapshot.processes[1]);
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(state.observe(&snapshot, &ownership));
        assert_eq!(state.sessions.len(), 2);

        snapshot.processes.clear();
        // Session 601 exited; session 600 appears twice in an unsorted fixture.
        crate::owners::tests::add_proc(&mut snapshot, 600, 1, "/bin/claude", "claude", 70, 10);
        crate::owners::tests::add_proc(&mut snapshot, 600, 1, "/bin/claude", "claude", 70, 10);
        snapshot.taken_at += 60;
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert_eq!(ownership.claude_sessions.len(), 1);
        assert!(state.observe(&snapshot, &ownership));
        assert_eq!(state.sessions.len(), 1);
        assert!(
            state
                .session_idle_secs(&id_600, snapshot.taken_at)
                .is_some()
        );
        assert_eq!(state.session_idle_secs(&id_601, snapshot.taken_at), None);
    }
}

//! One pass of agentwarden and the report it prints.

use std::{fmt::Write as _, io, io::Write as _};

use serde::Serialize;

use crate::{
    actions::{self, System},
    error::AppError,
    model::{Pressure, Snapshot, Swap},
    owners::Ownership,
    rules::{self, Action},
    state::{PendingRelaunch, RELAUNCH_WINDOW_SECS, State},
    store::{Outcome, Record, Store},
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
    pub exe_name: String,
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
    let snapshot = system.snapshot()?;
    let mut state = store.load_state();
    let ownership = Ownership::new(&snapshot, &state.known_helpers);
    let planned = rules::plan(&snapshot, &ownership, &state);
    let report_without_actions = build(&snapshot, &ownership, &state, planned);
    if !apply {
        return Ok(Report {
            recent: store.recent(RECENT_RECORDS),
            ..report_without_actions
        });
    }
    let mut applied = Vec::new();
    // An app a restart left quit is opened again once it is gone, until the
    // window passes; a running app needs nothing.
    if let Some(pending) = state.pending_relaunch.take()
        && ownership.codex_app.is_none()
        && snapshot.taken_at.saturating_sub(pending.since) <= RELAUNCH_WINDOW_SECS
    {
        let record = actions::relaunch(system, &pending.bundle);
        if record.outcome != Outcome::Restarted {
            state.pending_relaunch = Some(pending);
        }
        applied.push(record);
    }
    let executed = actions::execute(system, &report_without_actions.planned, snapshot.user);
    for (action, record) in report_without_actions.planned.iter().zip(&executed) {
        if let Some(bundle) = actions::left_quit(action, record) {
            state.pending_relaunch = Some(PendingRelaunch {
                bundle,
                since: snapshot.taken_at,
            });
        }
    }
    if executed
        .iter()
        .any(|record| record.rule == "idle-codex-restart" && record.outcome != Outcome::Skipped)
    {
        state.last_codex_restart_at = Some(snapshot.taken_at);
    }
    applied.extend(executed);
    state.observe(&snapshot, &ownership);
    // Both are attempted: state holds the restart gate, the log the record of
    // what was done. A failed log write does not fail the pass.
    let saved = store.save_state(&state).map_err(AppError::Store);
    if let Err(error) = store.append(&applied) {
        let _ = writeln!(io::stderr(), "cannot append to the action log: {error}");
    }
    saved?;
    Ok(Report {
        applied,
        recent: store.recent(RECENT_RECORDS),
        ..report_without_actions
    })
}

fn build(
    snapshot: &Snapshot,
    ownership: &Ownership<'_>,
    state: &State,
    planned: Vec<Action>,
) -> Report {
    let helper = |pid: &i32| {
        let process = ownership.process(*pid)?;
        Some(HelperReport {
            pid: *pid,
            exe_name: process.exe_name().to_owned(),
            footprint_bytes: ownership.tree_footprint(*pid),
            age_secs: process.age_secs,
        })
    };
    let claude_sessions = ownership
        .claude_sessions
        .iter()
        .filter_map(|session| {
            let process = ownership.process(session.pid)?;
            let helpers: Vec<HelperReport> = session.helpers.iter().filter_map(helper).collect();
            Some(SessionReport {
                pid: session.pid,
                idle_secs: state.session_idle_secs(&process.identity(), snapshot.taken_at),
                helpers_footprint_bytes: helpers.iter().map(|h| h.footprint_bytes).sum(),
                helpers,
            })
        })
        .collect();
    let codex = ownership.codex_app.as_ref().and_then(|app| {
        let process = ownership.process(app.pid)?;
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
                .map(|runtime| ownership.tree_footprint(*runtime))
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

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / (1u64 << 30) as f64)
}

fn minutes(secs: u64) -> u64 {
    secs / 60
}

pub fn text(report: &Report) -> String {
    let mut out = String::new();
    let level = |pressure: Pressure| format!("{pressure:?}").to_lowercase();
    let _ = writeln!(
        out,
        "memory: {} pressure (kernel {}), swap {} of {}, user idle {}",
        level(report.pressure),
        level(report.kernel_pressure),
        gb(report.swap.used_bytes),
        gb(report.swap.total_bytes),
        report
            .user_idle_secs
            .map_or("unknown".to_owned(), |secs| format!(
                "{} min",
                minutes(secs)
            )),
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
        gb(helpers),
        idle_hour
    );
    match &report.codex {
        Some(codex) => {
            let _ = writeln!(
                out,
                "codex: app {}, holds {}, helpers {} in {} processes, running commands {}, idle {} min",
                codex.app_pid,
                gb(codex.footprint_bytes),
                gb(codex.pools_footprint_bytes),
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
        gb(report.orphans.iter().map(|o| o.footprint_bytes).sum())
    );
    if report.planned.is_empty() {
        out.push_str("planned: nothing to do\n");
    } else {
        let _ = writeln!(
            out,
            "planned: {} actions, {}",
            report.planned.len(),
            gb(report.planned.iter().map(Action::footprint_bytes).sum())
        );
    }
    if !report.applied.is_empty() {
        let count = |outcome| {
            report
                .applied
                .iter()
                .filter(|r| r.outcome == outcome)
                .count()
        };
        let freed: u64 = report
            .applied
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Stopped | Outcome::Restarted))
            .map(|r| r.footprint_bytes)
            .sum();
        let _ = writeln!(
            out,
            "applied: {} stopped, {} restarted, {} skipped, {} refused, {} failed; freed {}",
            count(Outcome::Stopped),
            count(Outcome::Restarted),
            count(Outcome::Skipped),
            count(Outcome::Refused),
            count(Outcome::Failed),
            gb(freed)
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
                gb(record.footprint_bytes)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

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
                system
                    .alive
                    .borrow_mut()
                    .insert(process.pid, (process.started.clone(), process.uid));
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
        fn identify(&self, pid: i32) -> Option<(String, u32)> {
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
        assert_eq!(reclaim.applied[0].pids, [401, 400]);
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
}

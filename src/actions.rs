//! Carry out planned actions safely and record what happened.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    error::AppError,
    model::{Identity, Snapshot},
    rules::{Action, Rule, Target},
    store::{Outcome, Record},
};

const STOP_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);
pub const APP_QUIT_TIMEOUT: Duration = Duration::from_secs(60);
const QUIT_TIMED_OUT: &str = "app did not quit within 60 s; not forced";
const RELAUNCH_FAILED: &str = "quit, but relaunch failed";
const POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// Everything agentwarden needs from the operating system.
pub trait System {
    fn snapshot(&self) -> Result<Snapshot, AppError>;
    /// Start time and owner of a live process, or `None` when it is gone.
    fn identify(&self, pid: i32) -> Option<(String, u32)>;
    fn signal(&self, pid: i32, signal: Signal) -> io::Result<()>;
    /// Start an application bundle in the background without taking focus.
    fn launch_app(&self, bundle: &Path) -> io::Result<()>;
    fn sleep(&self, duration: Duration);
    fn now(&self) -> u64;
}

pub fn execute(system: &dyn System, actions: &[Action], user: u32) -> Vec<Record> {
    actions
        .iter()
        .map(|action| match action {
            Action::Stop {
                rule,
                root,
                tree,
                footprint_bytes,
                reason,
            } => stop(system, *rule, root, tree, user, *footprint_bytes, reason),
            Action::RestartCodex {
                rule,
                app,
                app_exe,
                footprint_bytes,
                reason,
            } => restart(system, *rule, app, app_exe, user, *footprint_bytes, reason),
        })
        .collect()
}

fn is_same(system: &dyn System, identity: &Identity, user: u32) -> bool {
    system
        .identify(identity.pid)
        .is_some_and(|(started, owner)| started == identity.started && owner == user)
}

fn wait_gone(system: &dyn System, targets: &[&Target], user: u32, limit: Duration) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        if targets.iter().all(|t| !is_same(system, &t.identity, user)) {
            return true;
        }
        if waited >= limit {
            return false;
        }
        system.sleep(POLL);
        waited += POLL;
    }
}

fn stop(
    system: &dyn System,
    rule: Rule,
    root: &Target,
    tree: &[Target],
    user: u32,
    footprint_bytes: u64,
    reason: &str,
) -> Record {
    let mut record = record(system, rule, root, tree, footprint_bytes, reason);
    // The kernel may have reused the PID since the snapshot.
    if !is_same(system, &root.identity, user) {
        record.outcome = Outcome::Skipped;
        record.detail = Some("process changed or exited before the stop".into());
        return record;
    }
    let live: Vec<&Target> = tree
        .iter()
        .filter(|t| is_same(system, &t.identity, user))
        .collect();
    for target in &live {
        let _ = system.signal(target.identity.pid, Signal::Term);
    }
    if wait_gone(system, &live, user, STOP_GRACE) {
        record.outcome = Outcome::Stopped;
        return record;
    }
    let stubborn: Vec<&Target> = live
        .into_iter()
        .filter(|t| is_same(system, &t.identity, user))
        .collect();
    for target in &stubborn {
        let _ = system.signal(target.identity.pid, Signal::Kill);
    }
    if wait_gone(system, &stubborn, user, KILL_GRACE) {
        record.outcome = Outcome::Stopped;
        record.detail = Some("needed SIGKILL".into());
    } else {
        record.outcome = Outcome::Failed;
        record.detail = Some("still running after SIGKILL".into());
    }
    record
}

fn restart(
    system: &dyn System,
    rule: Rule,
    app: &Target,
    app_exe: &str,
    user: u32,
    footprint_bytes: u64,
    reason: &str,
) -> Record {
    let mut record = record(system, rule, app, &[], footprint_bytes, reason);
    let Some(bundle) = app_bundle(app_exe) else {
        record.outcome = Outcome::Refused;
        record.detail = Some("executable is not inside an app bundle".into());
        return record;
    };
    if !is_same(system, &app.identity, user) {
        record.outcome = Outcome::Skipped;
        record.detail = Some("app changed or exited before the restart".into());
        return record;
    }
    if let Err(error) = system.signal(app.identity.pid, Signal::Term) {
        record.outcome = Outcome::Failed;
        record.detail = Some(format!("cannot ask the app to quit: {error}"));
        return record;
    }
    // Never force the app: an app that does not quit is left as it is.
    if !wait_gone(system, &[app], user, APP_QUIT_TIMEOUT) {
        record.outcome = Outcome::Refused;
        record.detail = Some(QUIT_TIMED_OUT.into());
        return record;
    }
    match system.launch_app(bundle) {
        Ok(()) => record.outcome = Outcome::Restarted,
        Err(error) => {
            record.outcome = Outcome::Failed;
            record.detail = Some(format!("{RELAUNCH_FAILED}: {error}"));
        }
    }
    record
}

/// The app bundle to open later when a restart asked the app to quit but did
/// not see it start again: it quit after the wait, or `open` failed.
pub fn left_quit(action: &Action, record: &Record) -> Option<PathBuf> {
    let Action::RestartCodex { app_exe, .. } = action else {
        return None;
    };
    let detail = record.detail.as_deref().unwrap_or_default();
    let quit_requested = (record.outcome == Outcome::Refused && detail == QUIT_TIMED_OUT)
        || (record.outcome == Outcome::Failed && detail.starts_with(RELAUNCH_FAILED));
    quit_requested
        .then(|| app_bundle(app_exe).map(Path::to_owned))
        .flatten()
}

/// Open an app that a restart left quit.
pub fn relaunch(system: &dyn System, bundle: &Path) -> Record {
    let name = bundle
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let mut record = Record {
        at: system.now(),
        rule: "idle-codex-restart".into(),
        outcome: Outcome::Restarted,
        target: name,
        pids: Vec::new(),
        footprint_bytes: 0,
        reason: "relaunch after the app quit late".into(),
        detail: None,
    };
    if let Err(error) = system.launch_app(bundle) {
        record.outcome = Outcome::Failed;
        record.detail = Some(format!("relaunch failed: {error}"));
    }
    record
}

/// `/Applications/ChatGPT.app/Contents/MacOS/ChatGPT` → `/Applications/ChatGPT.app`.
fn app_bundle(exe: &str) -> Option<&Path> {
    let (bundle, _) = exe.split_once(".app/Contents/MacOS/")?;
    Some(Path::new(&exe[..bundle.len() + ".app".len()]))
}

fn record(
    system: &dyn System,
    rule: Rule,
    root: &Target,
    tree: &[Target],
    footprint_bytes: u64,
    reason: &str,
) -> Record {
    let rule = serde_json::to_value(rule)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut pids: Vec<i32> = tree.iter().map(|t| t.identity.pid).collect();
    if pids.is_empty() {
        pids.push(root.identity.pid);
    }
    Record {
        at: system.now(),
        rule,
        outcome: Outcome::Failed,
        target: root.exe_name.clone(),
        pids,
        footprint_bytes,
        reason: reason.to_owned(),
        detail: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        cell::RefCell,
        collections::{BTreeMap, BTreeSet},
        io,
        path::{Path, PathBuf},
        time::Duration,
    };

    use super::{Signal, System, app_bundle, execute};
    use crate::{
        error::AppError,
        model::{Identity, Snapshot},
        rules::{Action, Rule, Target},
        store::Outcome,
    };

    /// Processes are `pid -> (started, uid)`; `stubborn` ignore SIGTERM, and
    /// `immortal` ignore everything.
    #[derive(Default)]
    pub(crate) struct FakeSystem {
        pub alive: RefCell<BTreeMap<i32, (String, u32)>>,
        pub stubborn: BTreeSet<i32>,
        pub immortal: BTreeSet<i32>,
        pub signals: RefCell<Vec<(i32, Signal)>>,
        pub launched: RefCell<Vec<PathBuf>>,
        pub slept: RefCell<Duration>,
    }

    impl System for FakeSystem {
        fn snapshot(&self) -> Result<Snapshot, AppError> {
            Ok(Snapshot::default())
        }
        fn identify(&self, pid: i32) -> Option<(String, u32)> {
            self.alive.borrow().get(&pid).cloned()
        }
        fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
            self.signals.borrow_mut().push((pid, signal));
            let dies = !self.immortal.contains(&pid)
                && (signal == Signal::Kill || !self.stubborn.contains(&pid));
            if dies {
                self.alive.borrow_mut().remove(&pid);
            }
            Ok(())
        }
        fn launch_app(&self, bundle: &Path) -> io::Result<()> {
            self.launched.borrow_mut().push(bundle.to_owned());
            Ok(())
        }
        fn sleep(&self, duration: Duration) {
            *self.slept.borrow_mut() += duration;
        }
        fn now(&self) -> u64 {
            1000
        }
    }

    fn target(pid: i32) -> Target {
        Target {
            identity: Identity {
                pid,
                started: format!("start-{pid}"),
            },
            exe_name: "node".into(),
        }
    }

    fn system(pids: &[i32]) -> FakeSystem {
        let system = FakeSystem::default();
        for pid in pids {
            system
                .alive
                .borrow_mut()
                .insert(*pid, (format!("start-{pid}"), 501));
        }
        system
    }

    fn stop_tree(pids: &[i32]) -> Action {
        Action::Stop {
            rule: Rule::Orphan,
            root: target(*pids.last().unwrap()),
            tree: pids.iter().map(|pid| target(*pid)).collect(),
            footprint_bytes: 1,
            reason: "test".into(),
        }
    }

    #[test]
    fn stop_terminates_children_first() {
        let system = system(&[401, 400]);
        let records = execute(&system, &[stop_tree(&[401, 400])], 501);
        assert_eq!(records[0].outcome, Outcome::Stopped);
        assert_eq!(records[0].pids, [401, 400]);
        assert_eq!(
            *system.signals.borrow(),
            [(401, Signal::Term), (400, Signal::Term)]
        );
    }

    #[test]
    fn reused_pid_or_other_user_is_skipped() {
        let system = system(&[]);
        system
            .alive
            .borrow_mut()
            .insert(400, ("a different start".into(), 501));
        let records = execute(&system, &[stop_tree(&[400])], 501);
        assert_eq!(records[0].outcome, Outcome::Skipped);
        assert!(system.signals.borrow().is_empty());

        let system = system_with_uid(400, 0);
        assert_eq!(
            execute(&system, &[stop_tree(&[400])], 501)[0].outcome,
            Outcome::Skipped
        );
    }

    fn system_with_uid(pid: i32, uid: u32) -> FakeSystem {
        let system = FakeSystem::default();
        system
            .alive
            .borrow_mut()
            .insert(pid, (format!("start-{pid}"), uid));
        system
    }

    #[test]
    fn stubborn_process_gets_sigkill_and_immortal_one_is_reported() {
        let mut stubborn = system(&[400]);
        stubborn.stubborn.insert(400);
        let record = &execute(&stubborn, &[stop_tree(&[400])], 501)[0];
        assert_eq!(record.outcome, Outcome::Stopped);
        assert_eq!(record.detail.as_deref(), Some("needed SIGKILL"));

        let mut immortal = system(&[400]);
        immortal.immortal.insert(400);
        assert_eq!(
            execute(&immortal, &[stop_tree(&[400])], 501)[0].outcome,
            Outcome::Failed
        );
    }

    fn restart_action() -> Action {
        Action::RestartCodex {
            rule: Rule::IdleCodexRestart,
            app: target(200),
            app_exe: "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT".into(),
            footprint_bytes: 1 << 30,
            reason: "test".into(),
        }
    }

    #[test]
    fn codex_app_is_relaunched_after_a_graceful_quit() {
        let system = system(&[200]);
        let record = &execute(&system, &[restart_action()], 501)[0];
        assert_eq!(record.outcome, Outcome::Restarted);
        assert_eq!(
            *system.launched.borrow(),
            [PathBuf::from("/Applications/ChatGPT.app")]
        );
    }

    #[test]
    fn codex_app_that_does_not_quit_is_never_forced() {
        let mut system = system(&[200]);
        system.immortal.insert(200);
        let record = &execute(&system, &[restart_action()], 501)[0];
        assert_eq!(record.outcome, Outcome::Refused);
        assert_eq!(*system.signals.borrow(), [(200, Signal::Term)]);
        assert!(system.launched.borrow().is_empty());
        assert!(*system.slept.borrow() >= super::APP_QUIT_TIMEOUT);
    }

    #[test]
    fn an_app_that_quits_late_is_left_for_a_later_relaunch() {
        let mut slow = system(&[200]);
        slow.immortal.insert(200);
        let action = restart_action();
        let record = &execute(&slow, std::slice::from_ref(&action), 501)[0];
        assert_eq!(
            super::left_quit(&action, record),
            Some(PathBuf::from("/Applications/ChatGPT.app"))
        );

        let quick = system(&[200]);
        let record = &execute(&quick, std::slice::from_ref(&action), 501)[0];
        assert_eq!(super::left_quit(&action, record), None);

        let relaunched = super::relaunch(&quick, Path::new("/Applications/ChatGPT.app"));
        assert_eq!(relaunched.outcome, Outcome::Restarted);
        assert_eq!(relaunched.target, "ChatGPT");
    }

    #[test]
    fn app_bundle_is_derived_from_the_main_executable() {
        assert_eq!(
            app_bundle("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"),
            Some(Path::new("/Applications/ChatGPT.app"))
        );
        assert_eq!(app_bundle("/usr/bin/codex"), None);
    }
}

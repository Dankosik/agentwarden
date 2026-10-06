//! Who owns each process: agent hosts, their helper servers, and orphans.

use std::collections::{BTreeSet, HashMap};

use crate::model::{Identity, Process, Snapshot};

/// A helper must have lived this long before it can be judged an orphan, so a
/// process caught between fork and adoption is never taken for one.
pub const ORPHAN_MIN_AGE_SECS: u64 = 120;

const SHELLS: [&str; 10] = [
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "login",
];

pub fn is_shell(process: &Process) -> bool {
    SHELLS.contains(&process.exe_name())
}

/// A Claude Code session: the desktop app's per-session binary or the CLI.
pub fn is_claude_session(process: &Process) -> bool {
    process.exe_name() == "claude"
}

/// Any Codex runtime: the app server, `codex exec`, the CLI, the daemon.
pub fn is_codex(process: &Process) -> bool {
    process.exe_name().eq_ignore_ascii_case("codex")
}

/// The desktop app that hosts Codex.
pub fn is_codex_app(process: &Process) -> bool {
    process.exe.ends_with("/ChatGPT.app/Contents/MacOS/ChatGPT")
}

/// Agent processes themselves are never signalled by a stop rule.
pub fn is_agent_host(process: &Process) -> bool {
    is_claude_session(process)
        || is_codex(process)
        || is_codex_app(process)
        || process.exe.ends_with("/Claude.app/Contents/MacOS/Claude")
}

#[derive(Debug)]
pub struct Session {
    pub pid: i32,
    /// Non-shell direct children: MCP and similar helper servers.
    pub helpers: Vec<i32>,
}

#[derive(Debug)]
pub struct CodexApp {
    pub pid: i32,
    /// Helper servers of the Codex runtimes inside the app.
    pub pools: Vec<i32>,
    /// Age of the youngest shell or `codex exec` started inside the app, if any.
    pub youngest_activity_secs: Option<u64>,
}

pub struct Ownership<'a> {
    snapshot: &'a Snapshot,
    index: HashMap<i32, usize>,
    children: HashMap<i32, Vec<i32>>,
    pub claude_sessions: Vec<Session>,
    /// Helpers of Codex runtimes outside the desktop app (CLI, daemon).
    pub codex_runtime_helpers: Vec<i32>,
    pub codex_app: Option<CodexApp>,
    pub orphans: Vec<i32>,
}

impl<'a> Ownership<'a> {
    /// `known_helpers` are identities seen earlier as helpers of a live agent.
    pub fn new(snapshot: &'a Snapshot, known_helpers: &BTreeSet<Identity>) -> Self {
        let index: HashMap<i32, usize> = snapshot
            .processes
            .iter()
            .enumerate()
            .map(|(position, process)| (process.pid, position))
            .collect();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for process in &snapshot.processes {
            if process.pid != process.ppid {
                children.entry(process.ppid).or_default().push(process.pid);
            }
        }
        for list in children.values_mut() {
            list.sort_unstable();
        }
        let mut ownership = Self {
            snapshot,
            index,
            children,
            claude_sessions: Vec::new(),
            codex_runtime_helpers: Vec::new(),
            codex_app: None,
            orphans: Vec::new(),
        };
        ownership.classify(known_helpers);
        ownership
    }

    fn classify(&mut self, known_helpers: &BTreeSet<Identity>) {
        let snapshot = self.snapshot;
        let app = snapshot
            .processes
            .iter()
            .find(|process| process.uid == snapshot.user && is_codex_app(process));
        let in_app: BTreeSet<i32> = app
            .map(|app| self.descendants(app.pid).into_iter().collect())
            .unwrap_or_default();

        for process in &snapshot.processes {
            if process.uid != snapshot.user {
                continue;
            }
            if is_claude_session(process) {
                let helpers = self.helper_children(process.pid);
                self.claude_sessions.push(Session {
                    pid: process.pid,
                    helpers,
                });
            } else if is_codex(process) && !in_app.contains(&process.pid) {
                let helpers = self.helper_children(process.pid);
                self.codex_runtime_helpers.extend(helpers);
            }
        }

        if let Some(app) = app {
            let runtimes: Vec<i32> = in_app
                .iter()
                .copied()
                .filter(|pid| self.process(*pid).is_some_and(is_codex))
                .collect();
            let pools = runtimes
                .iter()
                .flat_map(|pid| self.helper_children(*pid))
                .collect();
            let youngest_activity_secs = in_app
                .iter()
                .filter_map(|pid| self.process(*pid))
                .filter(|process| {
                    is_shell(process) || (is_codex(process) && process.args.contains(" exec"))
                })
                .map(|process| process.age_secs)
                .min();
            self.codex_app = Some(CodexApp {
                pid: app.pid,
                pools,
                youngest_activity_secs,
            });
        }

        self.orphans = snapshot
            .processes
            .iter()
            .filter(|process| {
                process.ppid == 1
                    && process.uid == snapshot.user
                    && !process.has_tty
                    && process.age_secs >= ORPHAN_MIN_AGE_SECS
                    && !is_agent_host(process)
                    && !is_shell(process)
                    && !snapshot.launchd_jobs.contains(&process.pid)
                    && (known_helpers.contains(&process.identity())
                        || snapshot
                            .mcp_servers
                            .iter()
                            .any(|server| server.matches(process)))
            })
            .map(|process| process.pid)
            .collect();
    }

    fn helper_children(&self, pid: i32) -> Vec<i32> {
        self.children
            .get(&pid)
            .into_iter()
            .flatten()
            .copied()
            .filter(|child| {
                self.process(*child)
                    .is_some_and(|process| !is_shell(process) && !is_agent_host(process))
            })
            .collect()
    }

    pub fn process(&self, pid: i32) -> Option<&'a Process> {
        self.index
            .get(&pid)
            .map(|position| &self.snapshot.processes[*position])
    }

    /// All descendants of `pid`, excluding `pid` itself.
    pub fn descendants(&self, pid: i32) -> Vec<i32> {
        let mut found = Vec::new();
        let mut stack = vec![pid];
        while let Some(current) = stack.pop() {
            for child in self.children.get(&current).into_iter().flatten() {
                if !found.contains(child) {
                    found.push(*child);
                    stack.push(*child);
                }
            }
        }
        found
    }

    /// `root` and its descendants, deepest first, so children stop before parents.
    pub fn tree(&self, root: i32) -> Vec<&'a Process> {
        let mut pids = self.descendants(root);
        pids.reverse();
        pids.push(root);
        pids.into_iter()
            .filter_map(|pid| self.process(pid))
            .collect()
    }

    pub fn tree_footprint(&self, root: i32) -> u64 {
        self.tree(root)
            .iter()
            .filter_map(|process| process.footprint_bytes)
            .sum()
    }

    /// Every process currently serving an agent, for the history of known helpers.
    pub fn helper_identities(&self) -> BTreeSet<Identity> {
        let roots = self
            .claude_sessions
            .iter()
            .flat_map(|session| session.helpers.iter())
            .chain(&self.codex_runtime_helpers)
            .chain(self.codex_app.iter().flat_map(|app| app.pools.iter()));
        let mut identities: BTreeSet<Identity> = roots
            .flat_map(|root| self.tree(*root))
            .map(Process::identity)
            .collect();
        // The app's own native helpers outlive a quit; remembering them lets the
        // orphan rule reclaim them after a restart. Commands run by shells and
        // the Codex runtimes themselves are not helpers.
        if let Some(app) = &self.codex_app {
            let mut stack = vec![app.pid];
            while let Some(pid) = stack.pop() {
                for child in self.children.get(&pid).into_iter().flatten() {
                    if let Some(process) = self.process(*child)
                        && !is_shell(process)
                        && !is_agent_host(process)
                    {
                        identities.insert(process.identity());
                        stack.push(*child);
                    }
                }
            }
        }
        identities
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeSet;

    use super::Ownership;
    use crate::model::{Process, ServerSignature, Snapshot};

    pub(crate) fn proc(pid: i32, ppid: i32, exe: &str, args: &str, age: u64, mb: u64) -> Process {
        Process {
            pid,
            ppid,
            uid: 501,
            started: format!("start-{pid}"),
            age_secs: age,
            has_tty: false,
            cpu_centis: 0,
            footprint_bytes: Some(mb << 20),
            exe: exe.into(),
            args: args.into(),
        }
    }

    const CLAUDE: &str = "/Users/u/Library/Application Support/Claude/claude-code/2.1.286/x/claude.app/Contents/MacOS/claude";
    const NODE: &str = "/Users/u/.codegraph/versions/v1.6.2/node";
    const CODEGRAPH: &str =
        "/Users/u/.codegraph/versions/v1.6.2/node lib/bin/codegraph.js serve --mcp";

    /// The machine on 2026-10-06, reduced: two Claude sessions, the Codex app with
    /// one pool, a user launchd service, and orphans left by an app restart.
    pub(crate) fn machine() -> Snapshot {
        let processes = vec![
            proc(1, 0, "/sbin/launchd", "/sbin/launchd", 400_000, 20),
            // Claude desktop app and two sessions.
            proc(
                100,
                1,
                "/Applications/Claude.app/Contents/MacOS/Claude",
                "Claude",
                90_000,
                500,
            ),
            proc(110, 100, CLAUDE, "claude", 9000, 300),
            proc(111, 110, NODE, CODEGRAPH, 9000, 64),
            proc(
                112,
                110,
                "/opt/homebrew/bin/railway",
                "railway mcp",
                9000,
                8,
            ),
            proc(
                113,
                110,
                "/bin/zsh",
                "/bin/zsh -c source snapshot.sh && cargo test",
                30,
                4,
            ),
            proc(114, 113, "/Users/u/.cargo/bin/cargo", "cargo test", 30, 200),
            proc(120, 100, CLAUDE, "claude", 9000, 280),
            proc(121, 120, NODE, CODEGRAPH, 9000, 64),
            proc(122, 120, "/opt/homebrew/bin/gopls", "gopls mcp", 9000, 220),
            // Codex inside the ChatGPT app, with one pool and a running command.
            proc(
                200,
                1,
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
                "ChatGPT",
                2000,
                400,
            ),
            proc(
                210,
                200,
                "/Applications/ChatGPT.app/Contents/Resources/codex-cli/Codex",
                "codex app-server",
                2000,
                450,
            ),
            proc(211, 210, NODE, CODEGRAPH, 1900, 64),
            proc(
                212,
                210,
                "/opt/homebrew/bin/railway",
                "railway mcp",
                1900,
                8,
            ),
            proc(213, 210, "/bin/zsh", "/bin/zsh -lc rg foo", 40, 3),
            proc(
                215,
                200,
                "/Applications/ChatGPT.app/Contents/Resources/native/bare-modifier-monitor",
                "bare-modifier-monitor",
                2000,
                5,
            ),
            // A user service run by launchd, and an interactive shell.
            proc(
                300,
                1,
                "/Applications/OrbStack.app/Contents/MacOS/OrbStack Helper",
                "OrbStack Helper",
                90_000,
                600,
            ),
            proc(
                301,
                1,
                "/opt/homebrew/bin/railway",
                "railway mcp",
                90_000,
                8,
            ),
            // Orphans: codegraph with its child, gopls known from history, and a
            // fresh orphan still inside its grace period.
            proc(400, 1, NODE, CODEGRAPH, 5000, 64),
            proc(401, 400, NODE, "node worker", 5000, 30),
            proc(402, 1, "/opt/homebrew/bin/gopls", "gopls mcp", 5000, 220),
            proc(403, 1, "/opt/homebrew/bin/railway", "railway mcp", 30, 8),
        ];
        Snapshot {
            taken_at: 1_791_290_000,
            user: 501,
            launchd_jobs: vec![300, 301],
            mcp_servers: vec![
                ServerSignature {
                    command: "/Users/u/.local/bin/codegraph".into(),
                    args: vec!["serve".into(), "--mcp".into()],
                },
                ServerSignature {
                    command: "railway".into(),
                    args: vec!["mcp".into()],
                },
            ],
            processes,
            ..Snapshot::default()
        }
    }

    pub(crate) fn known_gopls() -> BTreeSet<crate::model::Identity> {
        let snapshot = machine();
        snapshot
            .processes
            .iter()
            .filter(|p| p.pid == 402)
            .map(Process::identity)
            .collect()
    }

    #[test]
    fn sessions_pools_and_orphans_are_attributed() {
        let snapshot = machine();
        let ownership = Ownership::new(&snapshot, &known_gopls());
        let sessions: Vec<_> = ownership
            .claude_sessions
            .iter()
            .map(|s| (s.pid, s.helpers.clone()))
            .collect();
        assert_eq!(sessions, [(110, vec![111, 112]), (120, vec![121, 122])]);

        let app = ownership.codex_app.as_ref().unwrap();
        assert_eq!(app.pid, 200);
        assert_eq!(app.pools, [211, 212]);
        assert_eq!(app.youngest_activity_secs, Some(40));

        // 300/301 are launchd jobs, 403 is inside its grace period.
        assert_eq!(ownership.orphans, [400, 402]);
        assert_eq!(
            ownership
                .tree(400)
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
            [401, 400]
        );
        assert_eq!(ownership.tree_footprint(400), 94 << 20);
    }

    #[test]
    fn unknown_ppid_one_processes_are_not_orphans() {
        let snapshot = machine();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        // gopls is neither configured nor known from history.
        assert_eq!(ownership.orphans, [400]);
    }

    #[test]
    fn helper_history_covers_whole_helper_trees() {
        let snapshot = machine();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        let pids: Vec<i32> = ownership
            .helper_identities()
            .iter()
            .map(|identity| identity.pid)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(pids, [111, 112, 121, 122, 211, 212, 215]);
    }

    #[test]
    fn app_helpers_left_by_a_restart_become_orphans() {
        let before = machine();
        let known = Ownership::new(&before, &BTreeSet::new()).helper_identities();
        let mut after = machine();
        // The app quit: its native helper is reparented to launchd.
        after
            .processes
            .retain(|p| ![200, 210, 211, 212, 213].contains(&p.pid));
        after
            .processes
            .iter_mut()
            .find(|p| p.pid == 215)
            .unwrap()
            .ppid = 1;
        let ownership = Ownership::new(&after, &known);
        assert!(ownership.orphans.contains(&215));
        assert!(ownership.codex_app.is_none());
    }
}

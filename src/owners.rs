//! Who owns each process: agent hosts, their helper servers, and orphans.

use std::collections::BTreeSet;

use smallvec::SmallVec;

use crate::model::{Identity, Process, ServerSignature, Snapshot};

/// A helper must have lived this long before it can be judged an orphan, so a
/// process caught between fork and adoption is never taken for one.
pub const ORPHAN_MIN_AGE_SECS: u64 = 120;

const SHELLS: [&str; 10] = [
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "login",
];

fn exe_name(exe: &str) -> &str {
    exe.rsplit('/').next().unwrap_or(exe)
}

pub fn is_shell_exe(exe: &str) -> bool {
    SHELLS.contains(&exe_name(exe))
}

pub fn is_shell(snapshot: &Snapshot, process: &Process) -> bool {
    is_shell_exe(snapshot.exe(process))
}

/// A Claude Code session: the desktop app's per-session binary or the CLI.
pub fn is_claude_session_exe(exe: &str) -> bool {
    exe_name(exe) == "claude"
}

pub fn is_claude_session(snapshot: &Snapshot, process: &Process) -> bool {
    is_claude_session_exe(snapshot.exe(process))
}

/// Any Codex runtime: the app server, `codex exec`, the CLI, the daemon.
pub fn is_codex_exe(exe: &str) -> bool {
    exe_name(exe).eq_ignore_ascii_case("codex")
}

pub fn is_codex(snapshot: &Snapshot, process: &Process) -> bool {
    is_codex_exe(snapshot.exe(process))
}

/// The desktop app that hosts Codex.
pub fn is_codex_app_exe(exe: &str) -> bool {
    exe.ends_with("/ChatGPT.app/Contents/MacOS/ChatGPT")
}

pub fn is_codex_app(snapshot: &Snapshot, process: &Process) -> bool {
    is_codex_app_exe(snapshot.exe(process))
}

/// Agent processes themselves are never signalled by a stop rule.
pub fn is_agent_host_exe(exe: &str) -> bool {
    is_claude_session_exe(exe)
        || is_codex_exe(exe)
        || is_codex_app_exe(exe)
        || exe.ends_with("/Claude.app/Contents/MacOS/Claude")
}

pub fn is_agent_host(snapshot: &Snapshot, process: &Process) -> bool {
    is_agent_host_exe(snapshot.exe(process))
}

#[derive(Debug)]
pub struct Session {
    pub pid: i32,
    /// Non-shell direct children: MCP and similar helper servers.
    pub helpers: SmallVec<[i32; 8]>,
}

#[derive(Debug)]
pub struct CodexApp {
    pub pid: i32,
    /// Codex runtimes inside the app, such as the app server.
    pub runtimes: SmallVec<[i32; 4]>,
    /// Helper servers of those runtimes: configured MCP servers and helpers
    /// bundled with the app.
    pub pools: Vec<i32>,
    /// Work in progress inside the app: shells, `codex exec`, and any other
    /// child of a runtime. A shell that runs one command execs it, so a running
    /// `cargo test` is a runtime's direct child, not a shell's.
    pub commands: SmallVec<[i32; 4]>,
}

/// A process no rule may signal: an agent, a shell, or one with a terminal.
pub fn is_protected_process(snapshot: &Snapshot, process: &Process) -> bool {
    is_agent_host(snapshot, process) || is_shell(snapshot, process) || process.has_tty
}

/// Returns whether raw process fields qualify as an orphan base candidate
/// before checking `launchd_jobs` and `known_helpers`/`mcp_servers`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn is_orphan_base_candidate_fields(
    ppid: i32,
    uid: u32,
    has_tty: bool,
    age_secs: u64,
    exe: &str,
    user: u32,
) -> bool {
    ppid == 1
        && uid == user
        && !has_tty
        && age_secs >= ORPHAN_MIN_AGE_SECS
        && !is_agent_host_exe(exe)
        && !is_shell_exe(exe)
}

/// Returns whether a process is a candidate for the orphan rule before
/// checking `launchd_jobs` and `known_helpers`/`mcp_servers`.
pub fn is_orphan_base_candidate(snapshot: &Snapshot, process: &Process, user: u32) -> bool {
    is_orphan_base_candidate_fields(
        process.ppid,
        process.uid,
        process.has_tty,
        process.age_secs,
        snapshot.exe(process),
        user,
    )
}

enum PidIndex {
    /// `snapshot.processes` is already strictly sorted by `pid`.
    Direct,
    /// Fallback sorted `(pid, index)` map for unsorted or duplicate-PID fixtures.
    Sorted(Vec<(i32, usize)>),
}

enum Visited {
    Inline([u64; 16]),
    Heap(Vec<u64>),
}

impl Visited {
    fn new(count: usize) -> Self {
        let words = count.div_ceil(64);
        if words <= 16 {
            Self::Inline([0; 16])
        } else {
            Self::Heap(vec![0; words])
        }
    }

    fn insert(&mut self, index: usize) -> bool {
        let word = index / 64;
        let bit = 1u64 << (index % 64);
        let slot = match self {
            Self::Inline(arr) => &mut arr[word],
            Self::Heap(vec) => &mut vec[word],
        };
        let fresh = (*slot & bit) == 0;
        *slot |= bit;
        fresh
    }
}

pub struct Ownership {
    index: PidIndex,
    /// Flat `(ppid, pid)` pairs sorted by `(ppid, pid)`.
    children: Vec<(i32, i32)>,
    pub claude_sessions: Vec<Session>,
    /// Helpers of Codex runtimes outside the desktop app (CLI, daemon).
    pub codex_runtime_helpers: Vec<i32>,
    pub codex_app: Option<CodexApp>,
    pub orphans: SmallVec<[i32; 4]>,
}

impl Ownership {
    /// `known_helpers` are identities seen earlier as helpers of a live agent.
    ///
    /// Detached from any borrow of `snapshot` so callers may populate process
    /// footprints in place without rebuilding ownership classification.
    pub fn new(snapshot: &Snapshot, known_helpers: &BTreeSet<Identity>) -> Self {
        let index = if snapshot
            .processes
            .windows(2)
            .all(|pair| pair[0].pid < pair[1].pid)
        {
            PidIndex::Direct
        } else {
            let mut entries: Vec<(i32, usize)> = snapshot
                .processes
                .iter()
                .enumerate()
                .map(|(position, process)| (process.pid, position))
                .collect();
            // Stable sort then keep last duplicate, matching `HashMap::collect`.
            entries.sort_by_key(|&(pid, _)| pid);
            let mut deduped: Vec<(i32, usize)> = Vec::with_capacity(entries.len());
            for entry in entries {
                if let Some(last) = deduped.last_mut()
                    && last.0 == entry.0
                {
                    last.1 = entry.1;
                } else {
                    deduped.push(entry);
                }
            }
            PidIndex::Sorted(deduped)
        };
        let mut children = Vec::with_capacity(snapshot.processes.len());
        for process in &snapshot.processes {
            if process.pid != process.ppid {
                children.push((process.ppid, process.pid));
            }
        }
        children.sort_unstable();
        let mut ownership = Self {
            index,
            children,
            claude_sessions: Vec::new(),
            codex_runtime_helpers: Vec::new(),
            codex_app: None,
            orphans: SmallVec::new(),
        };
        ownership.classify(snapshot, known_helpers);
        ownership
    }

    fn direct_children(&self, ppid: i32) -> &[(i32, i32)] {
        let start = self.children.partition_point(|&(parent, _)| parent < ppid);
        let rest = &self.children[start..];
        let len = rest.partition_point(|&(parent, _)| parent == ppid);
        &rest[..len]
    }

    fn process_index(&self, snapshot: &Snapshot, pid: i32) -> Option<usize> {
        match &self.index {
            PidIndex::Direct => snapshot
                .processes
                .binary_search_by_key(&pid, |process| process.pid)
                .ok(),
            PidIndex::Sorted(entries) => entries
                .binary_search_by_key(&pid, |&(entry_pid, _)| entry_pid)
                .ok()
                .map(|pos| entries[pos].1),
        }
    }

    fn is_canonical_process(&self, snapshot: &Snapshot, idx: usize, pid: i32) -> bool {
        matches!(self.index, PidIndex::Direct) || self.process_index(snapshot, pid) == Some(idx)
    }

    fn classify(&mut self, snapshot: &Snapshot, known_helpers: &BTreeSet<Identity>) {
        let app = snapshot
            .processes
            .iter()
            .enumerate()
            .find(|&(idx, process)| {
                self.is_canonical_process(snapshot, idx, process.pid)
                    && process.uid == snapshot.user
                    && is_codex_app(snapshot, process)
            })
            .map(|(_, process)| process);
        let mut in_app: Vec<i32> = app
            .map(|app| self.descendants(snapshot, app.pid))
            .unwrap_or_default();
        in_app.sort_unstable();

        let mut has_orphan_candidates = false;
        for (idx, process) in snapshot.processes.iter().enumerate() {
            if process.uid != snapshot.user
                || !self.is_canonical_process(snapshot, idx, process.pid)
            {
                continue;
            }
            if is_claude_session(snapshot, process) {
                let helpers = self.helper_children(snapshot, process.pid);
                self.claude_sessions.push(Session {
                    pid: process.pid,
                    helpers,
                });
            } else if is_codex(snapshot, process) && in_app.binary_search(&process.pid).is_err() {
                let helpers = self.helper_children(snapshot, process.pid);
                self.codex_runtime_helpers.extend(helpers);
            } else if !has_orphan_candidates
                && is_orphan_base_candidate(snapshot, process, snapshot.user)
            {
                has_orphan_candidates = true;
            }
        }

        let distinctive: Vec<&ServerSignature> =
            if app.is_some() || (has_orphan_candidates && snapshot.launchd_jobs.is_some()) {
                snapshot
                    .mcp_servers
                    .iter()
                    .filter(|server| server.is_distinctive())
                    .collect()
            } else {
                Vec::new()
            };

        if let Some(app) = app {
            let app_exe = snapshot.exe(app);
            let bundle = app_exe
                .strip_suffix("Contents/MacOS/ChatGPT")
                .unwrap_or(app_exe);
            let runtimes: SmallVec<[i32; 4]> = in_app
                .iter()
                .copied()
                .filter(|pid| {
                    self.process(snapshot, *pid)
                        .is_some_and(|p| is_codex(snapshot, p))
                })
                .collect();
            let mut pools = Vec::new();
            let mut commands: SmallVec<[i32; 4]> = SmallVec::new();
            for runtime in &runtimes {
                for pid in self.helper_children(snapshot, *runtime) {
                    let is_pool = self.process(snapshot, pid).is_some_and(|process| {
                        // A generic signature such as `npx` would also match
                        // `npx vitest`; such a child counts as work.
                        snapshot.exe(process).starts_with(bundle)
                            || distinctive
                                .iter()
                                .any(|server| server.matches(snapshot, process))
                    });
                    if is_pool {
                        pools.push(pid);
                    } else {
                        commands.push(pid);
                    }
                }
            }
            commands.extend(in_app.iter().copied().filter(|pid| {
                self.process(snapshot, *pid).is_some_and(|process| {
                    is_shell(snapshot, process)
                        || (is_codex(snapshot, process)
                            && snapshot
                                .args(process)
                                .split_whitespace()
                                .any(|word| word == "exec"))
                })
            }));
            commands.sort_unstable();
            self.codex_app = Some(CodexApp {
                pid: app.pid,
                runtimes,
                pools,
                commands,
            });
        }

        if !has_orphan_candidates {
            return;
        }
        // Without the launchd job list a service cannot be told from an orphan.
        let Some(launchd_jobs) = &snapshot.launchd_jobs else {
            return;
        };
        let launchd_sorted = launchd_jobs.is_sorted();
        let is_launchd_job = |pid: i32| {
            if launchd_sorted {
                launchd_jobs.binary_search(&pid).is_ok()
            } else {
                launchd_jobs.contains(&pid)
            }
        };
        self.orphans = snapshot
            .processes
            .iter()
            .enumerate()
            .filter(|&(idx, process)| {
                self.is_canonical_process(snapshot, idx, process.pid)
                    && is_orphan_base_candidate(snapshot, process, snapshot.user)
                    && !is_launchd_job(process.pid)
                    && (Identity::in_set(known_helpers, process.pid, snapshot.started(process))
                        || distinctive
                            .iter()
                            .any(|server| server.matches(snapshot, process)))
            })
            .map(|(_, process)| process.pid)
            .collect();
    }

    fn helper_children(&self, snapshot: &Snapshot, pid: i32) -> SmallVec<[i32; 8]> {
        self.direct_children(pid)
            .iter()
            .map(|&(_, child)| child)
            .filter(|child| {
                self.process(snapshot, *child).is_some_and(|process| {
                    !is_shell(snapshot, process) && !is_agent_host(snapshot, process)
                })
            })
            .collect()
    }

    pub fn process<'a>(&self, snapshot: &'a Snapshot, pid: i32) -> Option<&'a Process> {
        self.process_index(snapshot, pid)
            .map(|position| &snapshot.processes[position])
    }

    /// Visit descendants of `pid` (excluding `pid` itself) in DFS order; stops
    /// early if `visit` returns `true`.
    fn visit_descendants<'a>(
        &self,
        snapshot: &'a Snapshot,
        pid: i32,
        mut visit: impl FnMut(i32, &'a Process) -> bool,
    ) -> bool {
        if self.direct_children(pid).is_empty() {
            return false;
        }
        let mut visited = Visited::new(snapshot.processes.len());
        if let Some(root_idx) = self.process_index(snapshot, pid) {
            visited.insert(root_idx);
        }
        let mut stack = SmallVec::<[i32; 16]>::new();
        stack.push(pid);
        while let Some(current) = stack.pop() {
            for &(_, child) in self.direct_children(current) {
                let Some(idx) = self.process_index(snapshot, child) else {
                    continue;
                };
                if visited.insert(idx) {
                    if visit(child, &snapshot.processes[idx]) {
                        return true;
                    }
                    stack.push(child);
                }
            }
        }
        false
    }

    /// True if any descendant of `pid` satisfies `predicate`, with early exit.
    pub fn any_descendant<'a>(
        &self,
        snapshot: &'a Snapshot,
        pid: i32,
        mut predicate: impl FnMut(&'a Process) -> bool,
    ) -> bool {
        self.visit_descendants(snapshot, pid, |_, process| predicate(process))
    }

    /// All descendants of `pid`, excluding `pid` itself.
    pub fn descendants(&self, snapshot: &Snapshot, pid: i32) -> Vec<i32> {
        let mut found = Vec::new();
        self.visit_descendants(snapshot, pid, |child, _| {
            found.push(child);
            false
        });
        found
    }

    /// `root` and its descendants, deepest first, so children stop before parents.
    pub fn tree<'a>(&self, snapshot: &'a Snapshot, root: i32) -> SmallVec<[&'a Process; 4]> {
        let mut result = SmallVec::new();
        self.visit_descendants(snapshot, root, |_, process| {
            result.push(process);
            false
        });
        result.reverse();
        if let Some(process) = self.process(snapshot, root) {
            result.push(process);
        }
        result
    }

    pub fn tree_footprint(&self, snapshot: &Snapshot, root: i32) -> u64 {
        let mut total = self
            .process(snapshot, root)
            .and_then(|process| process.footprint_bytes)
            .unwrap_or(0);
        self.visit_descendants(snapshot, root, |_, process| {
            if let Some(bytes) = process.footprint_bytes {
                total += bytes;
            }
            false
        });
        total
    }

    /// Insert helper roots currently serving an agent directly into `identities`
    /// without cloning `started` for processes already present.
    pub fn extend_helper_identities(
        &self,
        snapshot: &Snapshot,
        identities: &mut BTreeSet<Identity>,
    ) {
        let mut record_pid = |pid: i32| {
            if let Some(process) = self.process(snapshot, pid)
                && !Identity::in_set(identities, process.pid, snapshot.started(process))
            {
                identities.insert(snapshot.identity(process));
            }
        };
        for session in &self.claude_sessions {
            for &helper in &session.helpers {
                record_pid(helper);
            }
        }
        for &helper in &self.codex_runtime_helpers {
            record_pid(helper);
        }
        if let Some(app) = &self.codex_app {
            for &pool in &app.pools {
                record_pid(pool);
            }
            // The app's own native helpers outlive a quit; remembering them lets
            // the orphan rule reclaim them after a restart.
            for &(_, child) in self.direct_children(app.pid) {
                if let Some(process) = self.process(snapshot, child)
                    && !is_shell(snapshot, process)
                    && !is_agent_host(snapshot, process)
                    && !Identity::in_set(identities, process.pid, snapshot.started(process))
                {
                    identities.insert(snapshot.identity(process));
                }
            }
        }
    }

    /// Helper roots currently serving an agent, for the history of known
    /// helpers. Only roots: a helper's descendant may detach on purpose to be
    /// shared, such as a language server daemon, and is not the helper itself.
    #[cfg(test)]
    pub fn helper_identities(&self, snapshot: &Snapshot) -> BTreeSet<Identity> {
        let mut identities = BTreeSet::new();
        self.extend_helper_identities(snapshot, &mut identities);
        identities
    }

    /// True if `root` or any of its descendants is protected from signalling.
    pub fn is_tree_protected(&self, snapshot: &Snapshot, root: i32) -> bool {
        self.process(snapshot, root)
            .is_none_or(|p| is_protected_process(snapshot, p))
            || self.any_descendant(snapshot, root, |p| is_protected_process(snapshot, p))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use smallvec::smallvec;

    use super::Ownership;
    use crate::model::{ServerSignature, Snapshot};

    pub(crate) fn add_proc(
        snapshot: &mut Snapshot,
        pid: i32,
        ppid: i32,
        exe: &str,
        args: &str,
        age: u64,
        mb: u64,
    ) {
        snapshot.push_process(
            pid,
            ppid,
            501,
            &format!("start-{pid}"),
            age,
            false,
            0,
            Some(mb << 20),
            exe,
            args,
        );
    }

    const CLAUDE: &str = "/Users/u/Library/Application Support/Claude/claude-code/2.1.286/x/claude.app/Contents/MacOS/claude";
    const NODE: &str = "/Users/u/.codegraph/versions/v1.6.2/node";
    const CODEGRAPH: &str =
        "/Users/u/.codegraph/versions/v1.6.2/node lib/bin/codegraph.js serve --mcp";

    /// The machine on 2026-10-06, reduced: two Claude sessions, the Codex app with
    /// one pool, a user launchd service, and orphans left by an app restart.
    pub(crate) fn machine() -> Snapshot {
        let mut snapshot = Snapshot {
            taken_at: 1_791_290_000,
            user: 501,
            launchd_jobs: Some(vec![300, 301]),
            mcp_servers: Arc::from([
                ServerSignature {
                    command: "/Users/u/.local/bin/codegraph".into(),
                    args: smallvec!["serve".into(), "--mcp".into()],
                },
                ServerSignature {
                    command: "railway".into(),
                    args: smallvec!["mcp".into()],
                },
            ]),
            ..Snapshot::default()
        };
        add_proc(
            &mut snapshot,
            1,
            0,
            "/sbin/launchd",
            "/sbin/launchd",
            400_000,
            20,
        );
        // Claude desktop app and two sessions.
        add_proc(
            &mut snapshot,
            100,
            1,
            "/Applications/Claude.app/Contents/MacOS/Claude",
            "Claude",
            90_000,
            500,
        );
        add_proc(&mut snapshot, 110, 100, CLAUDE, "claude", 9000, 300);
        add_proc(&mut snapshot, 111, 110, NODE, CODEGRAPH, 9000, 64);
        add_proc(
            &mut snapshot,
            112,
            110,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            9000,
            8,
        );
        add_proc(
            &mut snapshot,
            113,
            110,
            "/bin/zsh",
            "/bin/zsh -c source snapshot.sh && cargo test",
            30,
            4,
        );
        add_proc(
            &mut snapshot,
            114,
            113,
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
            30,
            200,
        );
        add_proc(&mut snapshot, 120, 100, CLAUDE, "claude", 9000, 280);
        add_proc(&mut snapshot, 121, 120, NODE, CODEGRAPH, 9000, 64);
        add_proc(
            &mut snapshot,
            122,
            120,
            "/opt/homebrew/bin/gopls",
            "gopls mcp",
            9000,
            220,
        );
        // Codex inside the ChatGPT app, with one pool and a running command.
        add_proc(
            &mut snapshot,
            200,
            1,
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            "ChatGPT",
            2000,
            400,
        );
        add_proc(
            &mut snapshot,
            210,
            200,
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/Codex",
            "codex app-server",
            2000,
            450,
        );
        add_proc(&mut snapshot, 211, 210, NODE, CODEGRAPH, 1900, 64);
        add_proc(
            &mut snapshot,
            212,
            210,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            1900,
            8,
        );
        add_proc(
            &mut snapshot,
            213,
            210,
            "/bin/zsh",
            "/bin/zsh -lc rg foo",
            40,
            3,
        );
        add_proc(
            &mut snapshot,
            215,
            200,
            "/Applications/ChatGPT.app/Contents/Resources/native/bare-modifier-monitor",
            "bare-modifier-monitor",
            2000,
            5,
        );
        // A user service run by launchd, and an interactive shell.
        add_proc(
            &mut snapshot,
            300,
            1,
            "/Applications/OrbStack.app/Contents/MacOS/OrbStack Helper",
            "OrbStack Helper",
            90_000,
            600,
        );
        add_proc(
            &mut snapshot,
            301,
            1,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            90_000,
            8,
        );
        // Orphans: codegraph with its child, gopls known from history, and a
        // fresh orphan still inside its grace period.
        add_proc(&mut snapshot, 400, 1, NODE, CODEGRAPH, 5000, 64);
        add_proc(&mut snapshot, 401, 400, NODE, "node worker", 5000, 30);
        add_proc(
            &mut snapshot,
            402,
            1,
            "/opt/homebrew/bin/gopls",
            "gopls mcp",
            5000,
            220,
        );
        add_proc(
            &mut snapshot,
            403,
            1,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            30,
            8,
        );
        snapshot
    }

    fn push_server(snapshot: &mut Snapshot, server: ServerSignature) {
        let mut servers = snapshot.mcp_servers.to_vec();
        servers.push(server);
        snapshot.mcp_servers = Arc::from(servers);
    }

    pub(crate) fn known_gopls() -> BTreeSet<crate::model::Identity> {
        let snapshot = machine();
        snapshot
            .processes
            .iter()
            .filter(|p| p.pid == 402)
            .map(|p| snapshot.identity(p))
            .collect()
    }

    #[test]
    fn sessions_pools_and_orphans_are_attributed() {
        let snapshot = machine();
        let ownership = Ownership::new(&snapshot, &known_gopls());
        let sessions: Vec<_> = ownership
            .claude_sessions
            .iter()
            .map(|s| (s.pid, s.helpers.as_slice()))
            .collect();
        assert_eq!(sessions, [(110, &[111, 112][..]), (120, &[121, 122][..])]);

        let app = ownership.codex_app.as_ref().unwrap();
        assert_eq!(app.pid, 200);
        assert_eq!(app.runtimes.as_slice(), [210]);
        assert_eq!(app.pools, [211, 212]);
        // The shell running `rg` is work in progress, not a pool.
        assert_eq!(app.commands.as_slice(), [213]);

        // 300/301 are launchd jobs, 403 is inside its grace period.
        assert_eq!(ownership.orphans.as_slice(), [400, 402]);
        assert_eq!(
            ownership
                .tree(&snapshot, 400)
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
            [401, 400]
        );
        assert_eq!(ownership.tree_footprint(&snapshot, 400), 94 << 20);
    }

    #[test]
    fn unknown_ppid_one_processes_are_not_orphans() {
        let snapshot = machine();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        // gopls is neither configured nor known from history.
        assert_eq!(ownership.orphans.as_slice(), [400]);
    }

    #[test]
    fn helper_history_keeps_helper_roots_only() {
        let snapshot = machine();
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        let pids: Vec<i32> = ownership
            .helper_identities(&snapshot)
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
        let known = Ownership::new(&before, &BTreeSet::new()).helper_identities(&before);
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

    #[test]
    fn commands_under_the_codex_app_are_work_not_pools() {
        let mut snapshot = machine();
        snapshot.processes.retain(|p| p.pid != 213);
        // `zsh -lc 'cargo test'` execs cargo, so it is the runtime's child.
        add_proc(
            &mut snapshot,
            216,
            210,
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
            3600,
            300,
        );
        // A bundled helper is a pool; `exec-server` is a runtime, not a command.
        add_proc(
            &mut snapshot,
            217,
            210,
            "/Applications/ChatGPT.app/Contents/Resources/cua_node/bin/node_repl",
            "node_repl",
            1900,
            40,
        );
        add_proc(
            &mut snapshot,
            218,
            200,
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/Codex",
            "codex exec-server --remote x",
            2000,
            30,
        );
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        let app = ownership.codex_app.as_ref().unwrap();
        assert_eq!(app.pools, [211, 212, 217]);
        assert_eq!(app.commands.as_slice(), [216]);

        // With `npx` configured as an MCP server, `npx vitest` is still work.
        add_proc(
            &mut snapshot,
            219,
            210,
            "/opt/homebrew/bin/npx",
            "npx vitest",
            7200,
            90,
        );
        push_server(
            &mut snapshot,
            crate::model::ServerSignature {
                command: "npx".into(),
                args: smallvec![],
            },
        );
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert_eq!(
            ownership.codex_app.as_ref().unwrap().commands.as_slice(),
            [216, 219]
        );
        assert_eq!(app.runtimes.as_slice(), [210, 218]);
    }

    #[test]
    fn generic_signatures_and_unknown_launchd_state_find_no_orphans() {
        let mut snapshot = machine();
        // A user's detached `node server.js` matches a configured `node server.js`.
        add_proc(
            &mut snapshot,
            404,
            1,
            "/usr/local/bin/node",
            "node server.js",
            5000,
            90,
        );
        push_server(
            &mut snapshot,
            crate::model::ServerSignature {
                command: "node".into(),
                args: smallvec!["server.js".into()],
            },
        );
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert_eq!(ownership.orphans.as_slice(), [400]);

        snapshot.launchd_jobs = None;
        let ownership = Ownership::new(&snapshot, &known_gopls());
        assert!(ownership.orphans.is_empty());
    }

    #[test]
    fn handles_unsorted_tables_cycles_and_large_process_counts() {
        let mut snapshot = machine();
        snapshot.processes.reverse();
        let ownership = Ownership::new(&snapshot, &known_gopls());
        assert_eq!(ownership.orphans.as_slice(), [402, 400]);
        assert_eq!(ownership.tree_footprint(&snapshot, 400), 94 << 20);

        // Synthetic cycle: 900 -> 901 -> 900.
        let mut cyclic = Snapshot {
            user: 501,
            ..Snapshot::default()
        };
        add_proc(&mut cyclic, 900, 901, "/bin/a", "a", 500, 10);
        add_proc(&mut cyclic, 901, 900, "/bin/b", "b", 500, 20);
        let ownership = Ownership::new(&cyclic, &BTreeSet::new());
        assert_eq!(ownership.descendants(&cyclic, 900), [901]);
        assert_eq!(ownership.tree_footprint(&cyclic, 900), 30 << 20);

        // > 1024 processes exercises the heap-backed visited bitset.
        cyclic.processes.clear();
        cyclic.strings.clear();
        for pid in 1..=1200 {
            add_proc(
                &mut cyclic,
                pid,
                if pid == 1 { 0 } else { pid - 1 },
                "/bin/x",
                "x",
                500,
                1,
            );
        }
        let ownership = Ownership::new(&cyclic, &BTreeSet::new());
        assert_eq!(ownership.descendants(&cyclic, 1).len(), 1199);
        assert_eq!(ownership.tree_footprint(&cyclic, 1), 1200 << 20);
    }
}

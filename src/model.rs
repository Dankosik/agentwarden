//! One sample of the machine, as plain data that rules read and fixtures record.

use std::{borrow::Borrow, cmp::Ordering, collections::BTreeSet, sync::Arc};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Kernel memory pressure as macOS reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pressure {
    #[default]
    Normal,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swap {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

/// Byte range into [`Snapshot::strings`], allowing all process strings
/// (`started`, `exe`, `args`) to live in one contiguous buffer instead of
/// hundreds of per-process heap allocations across many allocator size classes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StrSpan {
    start: u32,
    len: u32,
}

impl StrSpan {
    pub const EMPTY: Self = Self { start: 0, len: 0 };

    #[inline]
    pub fn slice(self, arena: &str) -> &str {
        if self.len == 0 {
            return "";
        }
        let start = self.start as usize;
        let end = start + self.len as usize;
        arena.get(start..end).unwrap_or("")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    /// Start time as `ps -o lstart` prints it; with the PID it identifies the process.
    pub started: StrSpan,
    pub age_secs: u64,
    pub has_tty: bool,
    /// Accumulated CPU time in hundredths of a second.
    pub cpu_centis: u64,
    /// Physical footprint, including compressed and swapped pages.
    pub footprint_bytes: Option<u64>,
    /// Executable path.
    pub exe: StrSpan,
    /// Full command line. Used for matching only, never printed.
    pub args: StrSpan,
}

/// A PID alone is reused by the kernel; PID plus start time is not.
///
/// `started` (`en_US` `lstart`, 23–24 ASCII bytes) fits inline inside
/// `CompactString` with zero heap allocation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub pid: i32,
    pub started: CompactString,
}

trait IdentityKey {
    fn key(&self) -> (i32, &str);
}

impl IdentityKey for Identity {
    fn key(&self) -> (i32, &str) {
        (self.pid, self.started.as_str())
    }
}

struct IdentityRef<'a>(i32, &'a str);

impl IdentityKey for IdentityRef<'_> {
    fn key(&self) -> (i32, &str) {
        (self.0, self.1)
    }
}

impl PartialEq for dyn IdentityKey + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for dyn IdentityKey + '_ {}

impl PartialOrd for dyn IdentityKey + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for dyn IdentityKey + '_ {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl<'a> Borrow<dyn IdentityKey + 'a> for Identity {
    fn borrow(&self) -> &(dyn IdentityKey + 'a) {
        self
    }
}

impl Identity {
    /// Membership check without cloning `started`.
    pub fn in_set(set: &BTreeSet<Self>, pid: i32, started: &str) -> bool {
        !set.is_empty() && set.contains(&IdentityRef(pid, started) as &dyn IdentityKey)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Unix time in seconds.
    pub taken_at: u64,
    pub user: u32,
    pub pressure: Pressure,
    pub swap: Swap,
    /// Seconds since the last keyboard or pointer input.
    pub user_idle_secs: Option<u64>,
    /// Unix time of the newest Codex session (rollout) write.
    pub codex_activity_at: Option<u64>,
    /// PIDs that launchd runs as jobs; they are services, not orphans. `None`
    /// when `launchctl` could not tell, which suspends the orphan rule.
    pub launchd_jobs: Option<Vec<i32>>,
    /// Command and arguments of MCP servers configured for Claude Code and Codex.
    pub mcp_servers: Arc<[ServerSignature]>,
    pub processes: Vec<Process>,
    /// Flat backing arena for `Process::{started, exe, args}` spans.
    pub strings: String,
}

impl Snapshot {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn intern(&mut self, text: &str) -> StrSpan {
        if text.is_empty() {
            return StrSpan::EMPTY;
        }
        let Ok(start) = u32::try_from(self.strings.len()) else {
            return StrSpan::EMPTY;
        };
        let Ok(len) = u32::try_from(text.len()) else {
            return StrSpan::EMPTY;
        };
        if start.checked_add(len).is_none() {
            return StrSpan::EMPTY;
        }
        self.strings.push_str(text);
        StrSpan { start, len }
    }

    #[inline]
    pub fn str(&self, span: StrSpan) -> &str {
        span.slice(&self.strings)
    }

    #[inline]
    pub fn started(&self, process: &Process) -> &str {
        self.str(process.started)
    }

    #[inline]
    pub fn exe(&self, process: &Process) -> &str {
        self.str(process.exe)
    }

    #[inline]
    pub fn exe_name(&self, process: &Process) -> &str {
        let exe = self.exe(process);
        exe.rsplit('/').next().unwrap_or(exe)
    }

    #[inline]
    pub fn args(&self, process: &Process) -> &str {
        self.str(process.args)
    }

    pub fn identity(&self, process: &Process) -> Identity {
        Identity {
            pid: process.pid,
            started: CompactString::from(self.started(process)),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn push_process(
        &mut self,
        pid: i32,
        ppid: i32,
        uid: u32,
        started: &str,
        age_secs: u64,
        has_tty: bool,
        cpu_centis: u64,
        footprint_bytes: Option<u64>,
        exe: &str,
        args: &str,
    ) {
        let started = self.intern(started);
        let exe = self.intern(exe);
        let args = self.intern(args);
        self.processes.push(Process {
            pid,
            ppid,
            uid,
            started,
            age_secs,
            has_tty,
            cpu_centis,
            footprint_bytes,
            exe,
            args,
        });
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSignature {
    pub command: CompactString,
    /// Inline up to 2 arguments (`["mcp"]`, `["serve", "--mcp"]`) while keeping
    /// `ServerSignature` compact (80 B) so typical `Arc<[ServerSignature]>`
    /// tables stay in `MALLOC_TINY`.
    pub args: SmallVec<[CompactString; 2]>,
}

/// Launchers that run many unrelated programs; their name says nothing.
const GENERIC_COMMANDS: [&str; 22] = [
    "node", "nodejs", "npx", "npm", "pnpm", "pnpx", "yarn", "bun", "bunx", "deno", "python",
    "python3", "uv", "uvx", "pipx", "docker", "podman", "java", "env", "sh", "bash", "zsh",
];

impl ServerSignature {
    /// Whether a match alone is evidence that a process is this server: a
    /// specific program with its arguments, not a generic launcher.
    pub fn is_distinctive(&self) -> bool {
        let name = self.command.rsplit('/').next().unwrap_or(&self.command);
        !self.args.is_empty() && !GENERIC_COMMANDS.contains(&name)
    }

    /// True when `process` runs this server, directly or through a launcher.
    ///
    /// A launcher such as `codegraph` may exec an interpreter, so the command
    /// name must appear somewhere in the command line rather than as the
    /// executable, and the configured arguments must appear in order.
    pub fn matches(&self, snapshot: &Snapshot, process: &Process) -> bool {
        self.matches_parts(snapshot.exe(process), snapshot.args(process))
    }

    /// Match directly against `exe` and `args` slices without requiring `args`
    /// to be heap-allocated first.
    pub fn matches_parts(&self, exe: &str, args: &str) -> bool {
        let name = self.command.rsplit('/').next().unwrap_or(&self.command);
        if name.is_empty() {
            return false;
        }
        let exe_name = exe.rsplit('/').next().unwrap_or(exe);
        let named = exe_name == name
            || (args.contains(name)
                && args.split_whitespace().any(|token| {
                    let base = token.rsplit('/').next().unwrap_or(token);
                    base == name
                        || base
                            .strip_prefix(name)
                            .is_some_and(|rest| rest.starts_with('.'))
                }));
        if !named {
            return false;
        }
        let Some((first, rest)) = self.args.split_first() else {
            return true;
        };
        let mut tokens = args.split_whitespace();
        while let Some(token) = tokens.next() {
            if token == first {
                let mut lookahead = tokens.clone();
                if rest
                    .iter()
                    .all(|expected| lookahead.next() == Some(expected.as_str()))
                {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use smallvec::smallvec;

    use super::{ServerSignature, Snapshot};

    fn snap_with(exe: &str, args: &str) -> Snapshot {
        let mut snap = Snapshot::default();
        snap.push_process(
            10,
            1,
            501,
            "Tue Oct  6 12:00:00 2026",
            600,
            false,
            0,
            Some(64 << 20),
            exe,
            args,
        );
        snap
    }

    #[test]
    fn signature_matches_launcher_exec_and_plain_binary() {
        let codegraph = ServerSignature {
            command: "/Users/u/.local/bin/codegraph".into(),
            args: smallvec!["serve".into(), "--mcp".into()],
        };
        let node = snap_with(
            "/Users/u/.codegraph/versions/v1.6.2/node",
            "/Users/u/.codegraph/versions/v1.6.2/node --liftoff-only /Users/u/.codegraph/lib/bin/codegraph.js serve --mcp",
        );
        assert!(codegraph.matches(&node, &node.processes[0]));
        let sync = snap_with(
            "/Users/u/.codegraph/versions/v1.6.2/node",
            "node codegraph.js sync /Users/u/project",
        );
        assert!(!codegraph.matches(&sync, &sync.processes[0]));

        let railway = ServerSignature {
            command: "railway".into(),
            args: smallvec!["mcp".into()],
        };
        let r_mcp = snap_with("/opt/homebrew/bin/railway", "railway mcp");
        let r_up = snap_with("/opt/homebrew/bin/railway", "railway up");
        let r_ctl = snap_with("/usr/bin/railwayctl", "railwayctl mcp");
        assert!(railway.matches(&r_mcp, &r_mcp.processes[0]));
        assert!(!railway.matches(&r_up, &r_up.processes[0]));
        assert!(!railway.matches(&r_ctl, &r_ctl.processes[0]));
    }

    #[test]
    fn signature_matches_overlapping_arg_sequences_and_empty_args() {
        let repeated = ServerSignature {
            command: "tool".into(),
            args: smallvec!["--flag".into(), "--flag".into(), "mcp".into()],
        };
        let ok = snap_with("/bin/tool", "tool --flag --flag --flag mcp");
        let short = snap_with("/bin/tool", "tool --flag mcp");
        assert!(repeated.matches(&ok, &ok.processes[0]));
        assert!(!repeated.matches(&short, &short.processes[0]));

        let no_args = ServerSignature {
            command: "tool".into(),
            args: smallvec![],
        };
        let tool = snap_with("/bin/tool", "");
        let other = snap_with("/bin/other", "");
        assert!(no_args.matches(&tool, &tool.processes[0]));
        assert!(!no_args.matches(&other, &other.processes[0]));
    }

    #[test]
    fn identity_in_set_matches_without_allocating() {
        let mut set = std::collections::BTreeSet::new();
        assert!(!super::Identity::in_set(&set, 10, "start-10"));
        let lstart = super::Identity {
            pid: 10,
            started: "Thu Nov 26 21:00:00 2026".into(),
        };
        assert!(!lstart.started.is_heap_allocated());
        set.insert(super::Identity {
            pid: 10,
            started: "start-10".into(),
        });
        assert!(super::Identity::in_set(&set, 10, "start-10"));
        assert!(!super::Identity::in_set(&set, 10, "start-11"));
        assert!(!super::Identity::in_set(&set, 11, "start-10"));

        let sig = ServerSignature {
            command: "railway".into(),
            args: smallvec!["serve".into(), "--mcp".into()],
        };
        assert!(!sig.command.is_heap_allocated());
        assert!(!sig.args.spilled());
        assert!(sig.args.iter().all(|arg| !arg.is_heap_allocated()));
    }
}

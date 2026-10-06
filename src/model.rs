//! One sample of the machine, as plain data that rules read and fixtures record.

use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    /// Start time as `ps -o lstart` prints it; with the PID it identifies the process.
    pub started: String,
    pub age_secs: u64,
    pub has_tty: bool,
    /// Accumulated CPU time in hundredths of a second.
    pub cpu_centis: u64,
    /// Physical footprint, including compressed and swapped pages.
    pub footprint_bytes: Option<u64>,
    /// Executable path.
    pub exe: String,
    /// Full command line. Used for matching only, never printed.
    pub args: String,
}

impl Process {
    pub fn identity(&self) -> Identity {
        Identity {
            pid: self.pid,
            started: self.started.clone(),
        }
    }

    pub fn exe_name(&self) -> &str {
        self.exe.rsplit('/').next().unwrap_or(&self.exe)
    }
}

/// A PID alone is reused by the kernel; PID plus start time is not.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub pid: i32,
    pub started: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    pub mcp_servers: Vec<ServerSignature>,
    pub processes: Vec<Process>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSignature {
    pub command: String,
    pub args: Vec<String>,
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
    pub fn matches(&self, process: &Process) -> bool {
        let name = self.command.rsplit('/').next().unwrap_or(&self.command);
        if name.is_empty() {
            return false;
        }
        let tokens: Vec<&str> = process.args.split_whitespace().collect();
        let named = process.exe_name() == name
            || tokens.iter().any(|token| {
                let base = token.rsplit('/').next().unwrap_or(token);
                base == name
                    || base
                        .strip_prefix(name)
                        .is_some_and(|rest| rest.starts_with('.'))
            });
        named
            && (self.args.is_empty()
                || tokens
                    .windows(self.args.len())
                    .any(|window| window.iter().zip(&self.args).all(|(a, b)| a == b)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Process, ServerSignature};

    fn process(exe: &str, args: &str) -> Process {
        Process {
            pid: 10,
            ppid: 1,
            uid: 501,
            started: "Tue Oct  6 12:00:00 2026".into(),
            age_secs: 600,
            has_tty: false,
            cpu_centis: 0,
            footprint_bytes: Some(64 << 20),
            exe: exe.into(),
            args: args.into(),
        }
    }

    #[test]
    fn signature_matches_launcher_exec_and_plain_binary() {
        let codegraph = ServerSignature {
            command: "/Users/u/.local/bin/codegraph".into(),
            args: vec!["serve".into(), "--mcp".into()],
        };
        let node = process(
            "/Users/u/.codegraph/versions/v1.6.2/node",
            "/Users/u/.codegraph/versions/v1.6.2/node --liftoff-only /Users/u/.codegraph/lib/bin/codegraph.js serve --mcp",
        );
        assert!(codegraph.matches(&node));
        assert!(!codegraph.matches(&process(
            "/Users/u/.codegraph/versions/v1.6.2/node",
            "node codegraph.js sync /Users/u/project",
        )));

        let railway = ServerSignature {
            command: "railway".into(),
            args: vec!["mcp".into()],
        };
        assert!(railway.matches(&process("/opt/homebrew/bin/railway", "railway mcp")));
        assert!(!railway.matches(&process("/opt/homebrew/bin/railway", "railway up")));
        assert!(!railway.matches(&process("/usr/bin/railwayctl", "railwayctl mcp")));
    }
}

//! One sample of the machine, as plain data that rules read and fixtures record.

use std::{
    borrow::Borrow,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

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

/// Family of AI coding agent recognized by agentwarden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    Antigravity,
    Opencode,
    Other,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Opencode => "opencode",
            Self::Other => "other",
        }
    }
}

/// Launch mode of an agent session running outside a desktop app bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    Cli,
    Service,
    Subagent,
}

impl SessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Service => "service",
            Self::Subagent => "subagent",
        }
    }
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
    /// Compact identifier for the controlling terminal (`0` when `!has_tty`),
    /// distinguishing an inherited controlling TTY from a freshly opened PTY.
    pub tty_id: u16,
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
    /// Unix time of the newest default Antigravity (`~/.gemini/antigravity`) conversation write.
    pub antigravity_activity_at: Option<u64>,
    /// Per-`--app_data_dir` Antigravity conversation write timestamps (`dir_name` -> `Option<unix_secs>`).
    pub antigravity_activities: BTreeMap<CompactString, Option<u64>>,
    /// Unix time of the newest OpenCode database/log write.
    pub opencode_activity_at: Option<u64>,
    /// PIDs that launchd runs as jobs; they are services, not orphans. `None`
    /// when `launchctl` could not tell, which suspends the orphan rule.
    pub launchd_jobs: Option<Vec<i32>>,
    /// Command and arguments of MCP servers configured across supported agents.
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

    /// Activity timestamp for an Antigravity instance using `data_dir` under
    /// `~/.gemini/` (defaulting to `"antigravity"`). Checks per-dir
    /// `antigravity_activities` first, falling back to `antigravity_activity_at`
    /// only for the default directory or legacy fixtures whose per-dir map is empty.
    pub fn antigravity_activity_for(&self, data_dir: Option<&str>) -> Option<u64> {
        let dir = data_dir.unwrap_or(crate::owners::DEFAULT_ANTIGRAVITY_DATA_DIR);
        if let Some(&ts) = self.antigravity_activities.get(dir) {
            return ts;
        }
        if dir == crate::owners::DEFAULT_ANTIGRAVITY_DATA_DIR
            || self.antigravity_activities.is_empty()
        {
            self.antigravity_activity_at
        } else {
            None
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
        self.push_process_with_tty(
            pid,
            ppid,
            uid,
            started,
            age_secs,
            if has_tty { 1 } else { 0 },
            cpu_centis,
            footprint_bytes,
            exe,
            args,
        );
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn push_process_with_tty(
        &mut self,
        pid: i32,
        ppid: i32,
        uid: u32,
        started: &str,
        age_secs: u64,
        tty_id: u16,
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
            has_tty: tty_id != 0,
            tty_id,
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

const SCRIPT_EXTENSIONS: [&str; 7] = [".sh", ".bash", ".zsh", ".py", ".js", ".mjs", ".cjs"];

const JS_RUNNERS: [&str; 9] = [
    "node", "nodejs", "npm", "npx", "pnpm", "pnpx", "yarn", "bun", "bunx",
];

fn is_python_interpreter(name: &str) -> bool {
    name.eq_ignore_ascii_case("python")
        || (name.len() >= 7
            && name[..7].eq_ignore_ascii_case("python3")
            && name.as_bytes().get(7).is_none_or(|b| *b == b'.'))
}

fn is_py_runner(name: &str) -> bool {
    is_python_interpreter(name) || matches!(name, "uv" | "uvx" | "pipx")
}

fn is_generic_command(name: &str) -> bool {
    GENERIC_COMMANDS.contains(&name) || is_py_runner(name)
}

fn is_script_interpreter(name: &str) -> bool {
    JS_RUNNERS.contains(&name)
        || is_py_runner(name)
        || matches!(
            name,
            "deno" | "docker" | "podman" | "java" | "ruby" | "perl" | "php" | "env"
        )
        || name.starts_with("npm exec")
}

/// Trailing unscoped names inside scoped npm packages (`@playwright/mcp`,
/// `@org/server`, `@org/cli`) that are too generic to match standalone tokens
/// like `node mcp.js` or `node server.js`.
const GENERIC_SCOPED_BINS: [&str; 8] = [
    "mcp", "server", "cli", "index", "main", "app", "run", "stdio",
];

fn token_contains_scoped_pkg(token: &str, scoped: &str) -> bool {
    let mut rest = token;
    while let Some(pos) = rest.find(scoped) {
        let before_ok = pos == 0 || matches!(rest.as_bytes()[pos - 1], b'/' | b'\\' | b'=' | b':');
        let after = &rest[pos + scoped.len()..];
        let after_ok = after.is_empty()
            || matches!(
                after.as_bytes()[0],
                b'/' | b'\\' | b'@' | b'.' | b':' | b' '
            );
        if before_ok && after_ok {
            return true;
        }
        rest = &rest[pos + 1..];
    }
    false
}

fn strip_script_extension(name: &str) -> Option<&str> {
    for ext in SCRIPT_EXTENSIONS {
        if let Some(stem) = name.strip_suffix(ext) {
            if !stem.is_empty() && !is_generic_command(stem) {
                return Some(stem);
            }
            return None;
        }
    }
    None
}

fn token_matches_name(token: &str, name: &str) -> bool {
    let base = token.rsplit('/').next().unwrap_or(token);
    base == name
        || base
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('.'))
}

fn args_contain_sequence(args: &str, expected: &[CompactString]) -> bool {
    let Some((first, rest)) = expected.split_first() else {
        return true;
    };
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == first {
            let mut lookahead = tokens.clone();
            if rest
                .iter()
                .all(|exp| lookahead.next() == Some(exp.as_str()))
            {
                return true;
            }
        }
    }
    false
}

/// Strip `@version` from an npm-style package specifier while keeping any
/// leading `@scope/` (`@scope/pkg@1.2.3` -> `@scope/pkg`, `pkg@latest` -> `pkg`).
fn strip_package_version(spec: &str) -> &str {
    let search_start = usize::from(spec.starts_with('@'));
    match spec[search_start..].find('@') {
        Some(offset) => &spec[..search_start + offset],
        None => spec,
    }
}

/// For package-runner configs (`npx -y pkg@latest arg1`, `uvx pkg arg1`),
/// extract `(package_spec, trailing_args)` while skipping leading runner flags.
fn runner_package_and_tail<'a>(
    runner: &str,
    args: &'a [CompactString],
) -> Option<(&'a str, &'a [CompactString])> {
    let mut idx = 0usize;
    if runner == "pipx" && args.first().map(CompactString::as_str) == Some("run") {
        idx = 1;
    }
    while idx < args.len() {
        let tok = args[idx].as_str();
        if tok == "--" {
            idx += 1;
            break;
        }
        // Flags that consume the next argument (e.g. `npx -p pkg` / `uvx --from pkg`).
        if matches!(tok, "-p" | "--package" | "--from" | "--with" | "--python") {
            idx += 2;
            continue;
        }
        if tok.starts_with('-') {
            idx += 1;
            continue;
        }
        break;
    }
    let pkg = args.get(idx)?.as_str();
    let tail = &args[idx + 1..];
    Some((pkg, tail))
}

impl ServerSignature {
    /// Whether a match alone is evidence that an unattributed `ppid == 1`
    /// process is this server: a specific program with its arguments, not a
    /// generic launcher.
    pub fn is_distinctive(&self) -> bool {
        let name = self.command.rsplit('/').next().unwrap_or(&self.command);
        !self.args.is_empty() && !is_generic_command(name)
    }

    /// Whether a match under a live agent runtime identifies a configured MCP
    /// pool member rather than an arbitrary user command.
    ///
    /// Specific programs (e.g. `mcp-grafana.sh`, `coolify-mcp`, `node_repl`)
    /// qualify even without arguments; generic launchers (`npx`, `node`, `uvx`,
    /// `python3`, `python3.14`, `docker`) require non-empty arguments so bare
    /// `npx` never matches `npx vitest`.
    pub fn is_pool_signature(&self) -> bool {
        let name = self.command.rsplit('/').next().unwrap_or(&self.command);
        if name.is_empty() {
            return false;
        }
        !is_generic_command(name) || !self.args.is_empty()
    }

    /// Fast pre-filter on `exe` alone before fetching `args`: returns `true`
    /// when a process with executable `exe` could possibly match this signature
    /// (either directly by binary/script name, via a package-runner binary, or
    /// through an interpreter/runtime that a launcher script may `exec`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn could_match_exe(&self, exe: &str) -> bool {
        let sig_name = self.command.rsplit('/').next().unwrap_or(&self.command);
        let is_npm_exec = exe.starts_with("npm exec");
        let exe_name = if is_npm_exec {
            exe
        } else {
            exe.rsplit('/').next().unwrap_or(exe)
        };
        if sig_name.is_empty() || exe_name.is_empty() {
            return false;
        }
        if exe_name == sig_name
            || strip_script_extension(sig_name).is_some_and(|stem| exe_name == stem)
        {
            return true;
        }
        if matches!(sig_name, "npx" | "pnpx" | "bunx" | "uvx" | "pipx") {
            let is_js = matches!(sig_name, "npx" | "pnpx" | "bunx");
            if is_js && (JS_RUNNERS.contains(&exe_name) || is_npm_exec) {
                return true;
            }
            if !is_js && is_py_runner(exe_name) {
                return true;
            }
            if let Some((pkg, _)) = runner_package_and_tail(sig_name, &self.args) {
                let pkg_base = strip_package_version(pkg);
                let pkg_bin = pkg_base.rsplit('/').next().unwrap_or(pkg_base);
                if !pkg_bin.is_empty() && exe_name == pkg_bin {
                    return true;
                }
            }
            return false;
        }
        if is_generic_command(sig_name) {
            if sig_name == "env" {
                return is_script_interpreter(exe_name)
                    || self.args.iter().any(|arg| {
                        let base = arg.rsplit('/').next().unwrap_or(arg);
                        base == exe_name
                    });
            }
            return (is_py_runner(sig_name) && is_py_runner(exe_name))
                || (JS_RUNNERS.contains(&sig_name)
                    && (JS_RUNNERS.contains(&exe_name) || is_npm_exec));
        }
        // A specific command (e.g. `codegraph`, `mcp-grafana.sh`) may be a wrapper
        // that execs an interpreter (`node`, `python`, etc.).
        is_script_interpreter(exe_name)
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
        let exe_name = if exe.starts_with("npm exec") {
            exe
        } else {
            exe.rsplit('/').next().unwrap_or(exe)
        };
        let stem = strip_script_extension(name);

        let name_matches = |target: &str| {
            exe_name == target
                || (args.contains(target)
                    && args
                        .split_whitespace()
                        .any(|token| token_matches_name(token, target)))
        };

        let interpreter_alias_match = !self.args.is_empty()
            && ((is_python_interpreter(name) && is_python_interpreter(exe_name))
                || (matches!(name, "node" | "nodejs") && matches!(exe_name, "node" | "nodejs")));

        let named = name_matches(name) || stem.is_some_and(name_matches) || interpreter_alias_match;
        if named && args_contain_sequence(args, &self.args) {
            return true;
        }

        // Package runners (`npx`, `pnpx`, `bunx`, `uvx`, `pipx`) frequently
        // rewrite `argv` on exec (e.g. `npx -y next-devtools-mcp@latest`
        // becomes `npm exec next-devtools-mcp@latest` or
        // `node .../.bin/next-devtools-mcp`).
        if !self.args.is_empty()
            && matches!(name, "npx" | "pnpx" | "bunx" | "uvx" | "pipx")
            && self.matches_package_runner(name, exe_name, args)
        {
            return true;
        }

        false
    }

    fn matches_package_runner(&self, runner: &str, exe_name: &str, args: &str) -> bool {
        let Some((pkg, tail)) = runner_package_and_tail(runner, &self.args) else {
            return false;
        };
        let pkg_base = strip_package_version(pkg);
        let is_scoped = pkg_base.starts_with('@') && pkg_base.contains('/');
        let pkg_bin = pkg_base.rsplit('/').next().unwrap_or(pkg_base);
        if pkg_bin.is_empty() || (!is_scoped && is_generic_command(pkg_bin)) {
            return false;
        }
        let allow_unscoped_bin =
            !is_scoped || (!is_generic_command(pkg_bin) && !GENERIC_SCOPED_BINS.contains(&pkg_bin));

        let is_js = matches!(runner, "npx" | "pnpx" | "bunx");
        let first_arg_base = args
            .split_whitespace()
            .next()
            .map(|t| t.rsplit('/').next().unwrap_or(t))
            .unwrap_or("");
        let runtime_ok = if is_js {
            JS_RUNNERS.contains(&exe_name)
                || JS_RUNNERS.contains(&first_arg_base)
                || exe_name.starts_with("npm exec")
                || (allow_unscoped_bin && exe_name == pkg_bin)
        } else {
            is_py_runner(exe_name)
                || is_py_runner(first_arg_base)
                || (allow_unscoped_bin && exe_name == pkg_bin)
        };
        if !runtime_ok {
            return false;
        }

        let token_matches_pkg = |token: &str| {
            token == pkg
                || token == pkg_base
                || (is_scoped && token_contains_scoped_pkg(token, pkg_base))
                || (allow_unscoped_bin
                    && (token_matches_name(token, pkg_bin)
                        || strip_package_version(token.rsplit('/').next().unwrap_or(token))
                            == pkg_bin))
        };

        // Check `exe_name` directly (e.g. macOS `process.title = "npm exec next-devtools-mcp@latest"`).
        if tail.is_empty() && exe_name.split_whitespace().any(token_matches_pkg) {
            return true;
        }

        // Check `args` tokens for package match followed in order by `tail`
        // (allowing an optional `--` separator inserted by `npm exec`).
        let mut tokens = args.split_whitespace();
        while let Some(token) = tokens.next() {
            if token_matches_pkg(token) {
                let mut lookahead = tokens.clone();
                if tail.first().map(CompactString::as_str) != Some("--")
                    && lookahead.clone().next() == Some("--")
                {
                    let _ = lookahead.next();
                }
                if tail
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

    #[test]
    fn pool_signatures_cover_generic_launchers_with_args_and_wrapper_scripts() {
        let bare_npx = ServerSignature {
            command: "npx".into(),
            args: smallvec![],
        };
        assert!(!bare_npx.is_distinctive());
        assert!(!bare_npx.is_pool_signature());

        let next_mcp = ServerSignature {
            command: "npx".into(),
            args: smallvec!["-y".into(), "next-devtools-mcp@latest".into()],
        };
        assert!(!next_mcp.is_distinctive());
        assert!(next_mcp.is_pool_signature());
        assert!(next_mcp.matches_parts(
            "npm exec next-devtools-mcp@latest",
            "npm exec next-devtools-mcp@latest",
        ));
        assert!(next_mcp.matches_parts(
            "/opt/homebrew/bin/node",
            "/opt/homebrew/bin/node /Users/u/.npm/_npx/1/node_modules/.bin/next-devtools-mcp",
        ));
        assert!(!next_mcp.matches_parts("/opt/homebrew/bin/npx", "npx vitest"));
        assert!(!next_mcp.matches_parts("/opt/homebrew/bin/node", "node server.js"));

        let shadcn = ServerSignature {
            command: "npx".into(),
            args: smallvec!["shadcn@latest".into(), "mcp".into()],
        };
        assert!(shadcn.is_pool_signature());
        assert!(shadcn.matches_parts("/opt/homebrew/bin/node", "node /tmp/shadcn mcp"));
        assert!(!shadcn.matches_parts("/opt/homebrew/bin/node", "node /tmp/shadcn add button"));

        let uvx_git = ServerSignature {
            command: "uvx".into(),
            args: smallvec![
                "mcp-server-git".into(),
                "--repository".into(),
                "/repo".into()
            ],
        };
        assert!(uvx_git.is_pool_signature());
        assert!(uvx_git.matches_parts(
            "/opt/homebrew/bin/python3",
            "python3 /Users/u/.cache/uv/bin/mcp-server-git --repository /repo",
        ));
        assert!(!uvx_git.matches_parts("/opt/homebrew/bin/python3", "python3 app.py"));

        let grafana_wrapper = ServerSignature {
            command: "/Users/u/.config/bitrina-grafana/mcp-grafana.sh".into(),
            args: smallvec![],
        };
        assert!(!grafana_wrapper.is_distinctive());
        assert!(grafana_wrapper.is_pool_signature());
        assert!(grafana_wrapper.matches_parts(
            "/opt/homebrew/bin/mcp-grafana",
            "/opt/homebrew/bin/mcp-grafana --disable-write",
        ));
        assert!(!grafana_wrapper.matches_parts("/bin/sh", "sh -c echo hi"));

        let docker_mcp = ServerSignature {
            command: "docker".into(),
            args: smallvec!["run".into(), "-i".into(), "mcp/fetch".into()],
        };
        assert!(docker_mcp.is_pool_signature());
        assert!(docker_mcp.matches_parts("/usr/local/bin/docker", "docker run -i mcp/fetch"));
        assert!(!docker_mcp.matches_parts("/usr/local/bin/docker", "docker compose up"));

        // Scoped npm packages like `@playwright/mcp@latest`: match real
        // `node_modules/@playwright/mcp/cli.js` and `npm exec`, without
        // false-matching unrelated `node mcp.js` via the generic suffix `mcp`.
        let playwright = ServerSignature {
            command: "npx".into(),
            args: smallvec!["-y".into(), "@playwright/mcp@latest".into()],
        };
        assert!(playwright.is_pool_signature());
        assert!(playwright.matches_parts(
            "/opt/homebrew/bin/node",
            "/opt/homebrew/bin/node /Users/u/.npm/_npx/1/node_modules/@playwright/mcp/cli.js",
        ));
        assert!(playwright.matches_parts(
            "npm exec @playwright/mcp@latest",
            "npm exec @playwright/mcp@latest",
        ));
        assert!(!playwright.matches_parts("/opt/homebrew/bin/node", "node mcp.js"));
        assert!(!playwright.matches_parts("/opt/homebrew/bin/node", "node /tmp/mcp"));

        // `npm exec` inserting `--` before trailing flags.
        let shadcn_dash = ServerSignature {
            command: "npx".into(),
            args: smallvec!["shadcn@latest".into(), "mcp".into()],
        };
        assert!(
            shadcn_dash.matches_parts("npm exec shadcn@latest", "npm exec shadcn@latest -- mcp",)
        );

        // macOS Framework `Python` (capital P) and versioned `python3.14` match `uvx`.
        assert!(uvx_git.matches_parts(
            "/opt/homebrew/Cellar/python@3.14/3.14.3_1/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python",
            "Python /Users/u/.cache/uv/bin/mcp-server-git --repository /repo",
        ));
        assert!(uvx_git.could_match_exe(
            "/opt/homebrew/Cellar/python@3.14/3.14.3_1/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python"
        ));
        assert!(playwright.could_match_exe("/opt/homebrew/bin/node"));
        assert!(playwright.could_match_exe("npm exec @playwright/mcp@latest"));
        assert!(!playwright.could_match_exe("/usr/bin/clang"));
        assert!(grafana_wrapper.could_match_exe("/opt/homebrew/bin/mcp-grafana"));
        assert!(!grafana_wrapper.could_match_exe("/usr/bin/clang"));
        // Direct `python3` MCP config matches macOS Framework `Python` executable when args match,
        // and versioned `python3.14` is treated as a generic launcher (not distinctive).
        let py_mcp = ServerSignature {
            command: "python3".into(),
            args: smallvec!["/tmp/dummy_mcp.py".into()],
        };
        assert!(!py_mcp.is_distinctive());
        assert!(py_mcp.is_pool_signature());
        assert!(py_mcp.matches_parts(
            "/opt/homebrew/Cellar/python@3.14/3.14.3_1/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python",
            "Python /tmp/dummy_mcp.py",
        ));
        assert!(!py_mcp.matches_parts(
            "/opt/homebrew/Cellar/python@3.14/3.14.3_1/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python",
            "Python /tmp/other.py",
        ));
        let bare_py314 = ServerSignature {
            command: "python3.14".into(),
            args: smallvec![],
        };
        assert!(!bare_py314.is_distinctive());
        assert!(!bare_py314.is_pool_signature());
        let py314_script = ServerSignature {
            command: "python3.14".into(),
            args: smallvec!["mcp.py".into()],
        };
        assert!(!py314_script.is_distinctive());
        assert!(py314_script.is_pool_signature());
    }

    #[test]
    fn process_stays_72_bytes_and_antigravity_activity_lookup_is_per_dir() {
        assert_eq!(std::mem::size_of::<super::Process>(), 72);

        let mut snap = Snapshot {
            antigravity_activity_at: Some(1000),
            ..Snapshot::default()
        };
        // Legacy fixture without per-dir map falls back to `antigravity_activity_at`.
        assert_eq!(snap.antigravity_activity_for(None), Some(1000));
        assert_eq!(
            snap.antigravity_activity_for(Some("antigravity-ide")),
            Some(1000)
        );

        // Once per-dir entries are probed, each `--app_data_dir` resolves independently,
        // including explicit `None` when a clone's dir has no conversation files yet,
        // and an unprobed custom dir never leaks the default dir's timestamp.
        snap.antigravity_activities
            .insert("antigravity".into(), Some(2000));
        snap.antigravity_activities
            .insert("antigravity-ide".into(), None);
        snap.antigravity_activities
            .insert("antigravity-cli".into(), Some(3000));
        assert_eq!(snap.antigravity_activity_for(None), Some(2000));
        assert_eq!(
            snap.antigravity_activity_for(Some("antigravity")),
            Some(2000)
        );
        assert_eq!(snap.antigravity_activity_for(Some("antigravity-ide")), None);
        assert_eq!(
            snap.antigravity_activity_for(Some("antigravity-cli")),
            Some(3000)
        );
        assert_eq!(
            snap.antigravity_activity_for(Some("antigravity-unprobed")),
            None
        );
    }
}

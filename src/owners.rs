//! Who owns each process: agent hosts, their helper servers, and orphans.

use std::collections::BTreeSet;

use compact_str::CompactString;
use smallvec::SmallVec;

use crate::model::{AgentKind, Identity, Process, ServerSignature, SessionMode, Snapshot};

/// A helper must have lived this long before it can be judged an orphan, so a
/// process caught between fork and adoption is never taken for one.
pub const ORPHAN_MIN_AGE_SECS: u64 = 120;

const SHELLS: [&str; 10] = [
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "login",
];

/// Utilities that agents legitimately invoke directly (without a shell) and
/// that may detach intentionally (e.g. `git gc --auto`, `git fsmonitor--daemon`).
/// They are never MCP helper servers and must never enter `known_helpers` or be
/// stopped as orphans or idle-session helpers.
const NON_HELPER_COMMANDS: [&str; 19] = [
    "git",
    "git-fsmonitor--daemon",
    "git-lfs",
    "git-upload-pack",
    "git-receive-pack",
    "ssh",
    "ssh-agent",
    "gpg",
    "gpg-agent",
    "watchman",
    "caffeinate",
    "osascript",
    "open",
    "pbcopy",
    "pbpaste",
    "mdfind",
    "mdls",
    "xcrun",
    "xcode-select",
];

/// Interactive terminal editors, pagers, and multiplexers that must remain
/// protected when attached to a TTY even if they share a parent `claude` CLI's
/// controlling terminal.
const INTERACTIVE_TTY_PROGRAMS: [&str; 10] = [
    "vim", "nvim", "vi", "nano", "emacs", "less", "more", "fzf", "tmux", "screen",
];

fn exe_name(exe: &str) -> &str {
    exe.rsplit('/').next().unwrap_or(exe)
}

fn is_interactive_tty_program_exe(exe: &str) -> bool {
    INTERACTIVE_TTY_PROGRAMS.contains(&exe_name(exe))
}

fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Top-level macOS `.app` bundle path for a main app executable, excluding
/// framework-embedded bundles (such as Homebrew/Framework `Python.framework/.../Python.app`)
/// and helper `.app` bundles nested under `Contents/`.
///
/// `/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real`
/// → `Some("/Users/u/Applications/Antigravity US.app")`.
pub fn app_bundle_path(exe: &str) -> Option<&str> {
    let (prefix, _) = exe.split_once(".app/Contents/MacOS/")?;
    if prefix.contains(".framework/")
        || prefix.contains("/Contents/Frameworks/")
        || prefix.contains("/Contents/SharedSupport/")
        || prefix.contains("/Contents/Resources/")
        || prefix.contains("/Contents/PlugIns/")
        || prefix.contains("/Contents/Helpers/")
        || prefix.contains("/Contents/Library/")
        || prefix.contains("/Contents/XPCServices/")
    {
        return None;
    }
    Some(&exe[..prefix.len() + ".app".len()])
}

/// Top-level `.app` bundle prefix for any executable living anywhere under
/// `<bundle>.app/Contents/` (e.g. `Antigravity US.app/Contents/Resources/bin/language_server.real`
/// → `Some("/Users/u/Applications/Antigravity US.app")`), excluding framework bundles.
fn enclosing_app_bundle(exe: &str) -> Option<&str> {
    let (prefix, _) = exe.split_once(".app/Contents/")?;
    if prefix.contains(".framework/") {
        return None;
    }
    Some(&exe[..prefix.len() + ".app".len()])
}

/// Human-readable name derived from a `.app` bundle path
/// (`/Users/u/Applications/Antigravity US.app` → `Antigravity US`).
pub fn app_bundle_name(bundle: &str) -> &str {
    let base = bundle.rsplit('/').next().unwrap_or(bundle);
    base.strip_suffix(".app").unwrap_or(base)
}

pub fn is_shell_exe(exe: &str) -> bool {
    SHELLS.contains(&exe_name(exe))
}

pub fn is_shell(snapshot: &Snapshot, process: &Process) -> bool {
    is_shell_exe(snapshot.exe(process))
}

pub fn is_non_helper_command_exe(exe: &str) -> bool {
    let name = exe_name(exe);
    NON_HELPER_COMMANDS.contains(&name)
        || name.starts_with("git-credential-")
        || name.starts_with("git-remote-")
}

/// A Claude Code session: the desktop app's per-session binary or the CLI.
pub fn is_claude_session_exe(exe: &str) -> bool {
    exe_name(exe) == "claude" || exe.contains("/.local/share/claude/versions/")
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

pub fn is_codex_exec(snapshot: &Snapshot, process: &Process) -> bool {
    is_codex(snapshot, process)
        && snapshot
            .args(process)
            .split_whitespace()
            .skip(1)
            .any(|word| word == "exec")
}

/// Antigravity CLI (`agy`).
pub fn is_antigravity_cli_exe(exe: &str) -> bool {
    exe_name(exe) == "agy"
}

pub fn is_antigravity_cli(snapshot: &Snapshot, process: &Process) -> bool {
    is_antigravity_cli_exe(snapshot.exe(process))
}

/// Candidate executable name for Antigravity's Go `language_server` hub
/// (including proxy wrappers like `language_server.real` or arch-suffixed builds).
pub fn is_antigravity_server_candidate_exe(exe: &str) -> bool {
    let name = exe_name(exe);
    name == "language_server"
        || name.starts_with("language_server.")
        || name.starts_with("language_server_")
}

fn has_host_bridge_url_flag(args: &str) -> bool {
    let mut tokens = args.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok.starts_with("--host_bridge_url=")
            || (tok == "--host_bridge_url" && tokens.next().is_some_and(|v| !v.starts_with('-')))
        {
            return true;
        }
    }
    false
}

/// Confirmed Antigravity `language_server` runtime (matches standard installs,
/// renamed clones like `Antigravity US.app`, and any bundle launching
/// `language_server` with `--override_ide_name antigravity` / `--app_data_dir antigravity`).
pub fn is_antigravity_server(snapshot: &Snapshot, process: &Process) -> bool {
    let exe = snapshot.exe(process);
    is_antigravity_server_candidate_exe(exe)
        && (contains_ascii_ci(exe, "antigravity")
            || contains_ascii_ci(snapshot.args(process), "antigravity"))
}

/// Default `--app_data_dir` name under `~/.gemini/` for Antigravity.
pub const DEFAULT_ANTIGRAVITY_DATA_DIR: &str = "antigravity";

/// Extract the `--app_data_dir` subdirectory name from an Antigravity
/// `language_server` command line (`--app_data_dir antigravity`,
/// `--app_data_dir=antigravity-ide`, `-app_data_dir ...`), rejecting empty or
/// path-traversal values so resolution stays bounded inside `~/.gemini/`.
pub fn extract_antigravity_data_dir(args: &str) -> Option<&str> {
    let mut tokens = args.split_whitespace();
    while let Some(tok) = tokens.next() {
        let value = if tok == "--app_data_dir" || tok == "-app_data_dir" {
            tokens.next()
        } else {
            tok.strip_prefix("--app_data_dir=")
                .or_else(|| tok.strip_prefix("-app_data_dir="))
        };
        if let Some(name) = value
            && !name.is_empty()
            && !name.contains('/')
            && !name.contains('\\')
            && name != "."
            && name != ".."
            && !name.starts_with('-')
        {
            return Some(name);
        }
    }
    None
}

/// True when a command line belongs to Antigravity's Node `sidecar_sdk`
/// launched through its Electron executable (`--import=.../sidecar_sdk/register_loader.js`).
pub fn is_antigravity_sidecar_args(args: &str) -> bool {
    args.contains("node_modules/sidecar_sdk/")
        || args.contains("sidecar_sdk/register_loader")
        || (args.contains("--import=") && args.contains("sidecar_sdk"))
}

/// Antigravity's Node sidecar launched via its Electron executable (`sidecar_sdk`).
pub fn is_antigravity_sidecar(snapshot: &Snapshot, process: &Process) -> bool {
    is_antigravity_sidecar_args(snapshot.args(process))
}

/// Any OpenCode runtime (`opencode` CLI or `opencode-cli` desktop/service binary).
pub fn is_opencode_runtime_exe(exe: &str) -> bool {
    let name = exe_name(exe);
    name.eq_ignore_ascii_case("opencode") || name.eq_ignore_ascii_case("opencode-cli")
}

pub fn is_opencode_runtime(snapshot: &Snapshot, process: &Process) -> bool {
    is_opencode_runtime_exe(snapshot.exe(process))
}

/// OpenCode's detached background server (`opencode-cli serve --service`).
pub fn is_opencode_service(snapshot: &Snapshot, process: &Process) -> bool {
    if !is_opencode_runtime(snapshot, process) {
        return false;
    }
    let mut has_serve = false;
    let mut has_service = false;
    for word in snapshot.args(process).split_whitespace() {
        if word == "serve" {
            has_serve = true;
        } else if word == "--service" {
            has_service = true;
        }
    }
    has_serve && has_service
}

pub fn is_opencode_pty_exe(exe: &str) -> bool {
    exe_name(exe) == "opencode-pty"
}

fn bundle_name_matches_prefix(bundle_name: &str, prefix: &str) -> bool {
    bundle_name.eq_ignore_ascii_case(prefix)
        || (bundle_name.len() > prefix.len()
            && bundle_name[..prefix.len()].eq_ignore_ascii_case(prefix)
            && matches!(bundle_name.as_bytes()[prefix.len()], b' ' | b'-' | b'_'))
}

/// Fallback Codex desktop app recognition when no `codex` runtime has spawned
/// inside the bundle yet (matches `ChatGPT.app`, `Codex.app`, `ChatGPT US.app`,
/// `Codex US.app`, etc.). When a bundled `codex` runtime is running, ANY `.app`
/// bundle hosting it is also discovered dynamically.
pub fn is_codex_app_exe(exe: &str) -> bool {
    let Some(bundle) = app_bundle_path(exe) else {
        return false;
    };
    let name = app_bundle_name(bundle);
    bundle_name_matches_prefix(name, "ChatGPT") || bundle_name_matches_prefix(name, "Codex")
}

pub fn is_antigravity_app_fallback_exe(exe: &str) -> bool {
    app_bundle_path(exe)
        .is_some_and(|bundle| contains_ascii_ci(app_bundle_name(bundle), "antigravity"))
}

pub fn is_opencode_app_fallback_exe(exe: &str) -> bool {
    app_bundle_path(exe)
        .is_some_and(|bundle| contains_ascii_ci(app_bundle_name(bundle), "opencode"))
}

pub fn is_claude_app_exe(exe: &str) -> bool {
    app_bundle_path(exe)
        .is_some_and(|bundle| bundle_name_matches_prefix(app_bundle_name(bundle), "Claude"))
}

/// Known AI agent desktop app main executables (excluding framework bundles
/// like `Python.app` and browser bundles like Playwright's `Chromium.app`).
/// Arbitrarily renamed desktop app clones discovered dynamically via their
/// bundled runtimes are additionally protected via `Ownership::desktop_apps`.
pub fn is_agent_app_exe(exe: &str) -> bool {
    is_codex_app_exe(exe)
        || is_antigravity_app_fallback_exe(exe)
        || is_opencode_app_fallback_exe(exe)
        || is_claude_app_exe(exe)
}

/// Processes that `platform::begin_pass` must retain even at `ppid <= 1` so
/// dynamic parent-walking and agent/service attribution see every running tool
/// (including arbitrarily named app clones such as `Antigravity US.app` or
/// `MyCustomIDE.app`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn is_agent_or_app_candidate_exe(exe: &str) -> bool {
    is_claude_session_exe(exe)
        || is_codex_exe(exe)
        || is_antigravity_cli_exe(exe)
        || is_antigravity_server_candidate_exe(exe)
        || is_opencode_runtime_exe(exe)
        || (app_bundle_path(exe).is_some()
            && !exe.starts_with("/System/")
            && !exe.starts_with("/usr/"))
}

/// Agent runtimes and known agent desktop apps are never signalled by a stop rule.
pub fn is_agent_host_exe(exe: &str) -> bool {
    is_claude_session_exe(exe)
        || is_codex_exe(exe)
        || is_antigravity_cli_exe(exe)
        || is_opencode_runtime_exe(exe)
        || (is_antigravity_server_candidate_exe(exe) && contains_ascii_ci(exe, "antigravity"))
        || is_agent_app_exe(exe)
}

pub fn is_agent_host(snapshot: &Snapshot, process: &Process) -> bool {
    if is_antigravity_sidecar(snapshot, process) {
        return false;
    }
    is_agent_host_exe(snapshot.exe(process)) || is_antigravity_server(snapshot, process)
}

/// Whether `process` is an external runtime helper shipped outside the `.app`
/// bundle (e.g. `~/.codex/computer-use/.../SkyComputerUseClient`,
/// `~/.cache/codex-runtimes/...`, `~/.codex/plugins/...`, `codex-code-mode-host`,
/// `node_repl`, `~/.gemini/antigravity/...`, `~/.cache/opencode/...`).
fn is_agent_runtime_helper(
    agent: AgentKind,
    exe: &str,
    args: &str,
    data_dir: Option<&str>,
) -> bool {
    match agent {
        AgentKind::Codex => {
            let name = exe_name(exe);
            name == "codex-code-mode-host"
                || name == "node_repl"
                || exe.contains("/.codex/computer-use/")
                || exe.contains("/.codex/node_repl/")
                || exe.contains("/.codex/plugins/")
                || args.contains("/.codex/plugins/")
                || exe.contains("/.cache/codex-runtimes/")
                || exe.contains("/Codex Computer Use.app/")
        }
        AgentKind::Antigravity => {
            if is_antigravity_sidecar_args(args)
                || exe.contains("/.gemini/antigravity/")
                || args.contains("/.gemini/antigravity/lib/")
            {
                return true;
            }
            if let Some(dir) = data_dir
                && dir != DEFAULT_ANTIGRAVITY_DATA_DIR
            {
                let needle = format!("/.gemini/{dir}/");
                return exe.contains(&needle) || args.contains(&needle);
            }
            false
        }
        AgentKind::Opencode => {
            exe.contains("/.cache/opencode/")
                || exe.contains("/Application Support/ai.opencode.desktop/")
        }
        AgentKind::Claude | AgentKind::Other => false,
    }
}

#[derive(Debug)]
pub struct Session {
    pub pid: i32,
    /// Non-shell, non-VCS direct children: MCP and similar helper servers.
    pub helpers: SmallVec<[i32; 8]>,
}

#[derive(Debug)]
pub struct CodexApp {
    pub pid: i32,
    /// Codex runtimes inside the app, such as the app server.
    pub runtimes: SmallVec<[i32; 4]>,
    /// Helper servers of those runtimes: configured MCP servers and helpers
    /// bundled with the app or its external runtime dirs.
    pub pools: Vec<i32>,
    /// Work in progress inside the app: shells, `codex exec`, nested agents,
    /// and any other non-pool child of a runtime.
    pub commands: SmallVec<[i32; 4]>,
}

/// A dynamically discovered desktop agent app (Codex/ChatGPT, Antigravity or
/// any clone like `Antigravity US.app`, OpenCode, or any other IDE/app hosting
/// configured MCP servers such as Cursor, Windsurf, or Zed).
#[derive(Debug)]
pub struct DesktopApp {
    pub agent: AgentKind,
    pub pid: i32,
    pub app_name: CompactString,
    pub bundle_path: CompactString,
    /// For Antigravity apps, the `--app_data_dir` subdirectory under `~/.gemini/`
    /// (defaults to `"antigravity"`). `None` for non-Antigravity apps.
    pub data_dir: Option<CompactString>,
    /// Agent runtimes inside the app (e.g. `codex app-server`, `language_server`,
    /// or OpenCode's attached `opencode-cli serve --service`).
    pub runtimes: SmallVec<[i32; 4]>,
    /// MCP servers and bundled/runtime helpers under those runtimes.
    pub pools: Vec<i32>,
    /// Running work inside the app: shells, TTY commands, nested agents, and
    /// non-pool children; while non-empty, the app is never restarted.
    pub commands: SmallVec<[i32; 4]>,
    /// Detached `opencode-cli serve --service` PID when serving a running OpenCode app.
    pub attached_service: Option<i32>,
}

/// An agent session running outside a desktop app bundle: standalone CLI,
/// background service (`opencode-cli serve --service`), nested subagent
/// (e.g. `codex` spawned by Antigravity US or Claude Code), or any other
/// dynamically discovered CLI tool running configured MCP servers.
#[derive(Debug)]
pub struct AgentSession {
    pub agent: AgentKind,
    pub mode: SessionMode,
    pub pid: i32,
    /// Real executable name for generic (`AgentKind::Other`) CLI tools.
    pub name: Option<CompactString>,
    /// Enclosing agent name when invoked as a subagent (e.g. `"Antigravity US"`, `"claude"`).
    pub parent_agent: Option<CompactString>,
    /// MCP / runtime helpers belonging to this session (tracked in `known_helpers`).
    pub helpers: SmallVec<[i32; 8]>,
    /// Running commands, shells, TTY children, or nested agents under this session.
    pub commands: SmallVec<[i32; 4]>,
}

/// Returns whether raw process fields qualify as an orphan base candidate
/// before checking `launchd_jobs`, `args` (e.g. `sidecar_sdk`), and
/// `known_helpers`/`mcp_servers`.
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
        && !is_non_helper_command_exe(exe)
        && app_bundle_path(exe).is_none()
}

/// Returns whether a process is a candidate for the orphan rule before
/// checking `launchd_jobs` and `known_helpers`/`mcp_servers`.
pub fn is_orphan_base_candidate(snapshot: &Snapshot, process: &Process, user: u32) -> bool {
    if process.ppid != 1
        || process.uid != user
        || process.has_tty
        || process.age_secs < ORPHAN_MIN_AGE_SECS
    {
        return false;
    }
    let exe = snapshot.exe(process);
    if is_shell_exe(exe) || is_non_helper_command_exe(exe) {
        return false;
    }
    if is_antigravity_sidecar(snapshot, process) {
        return true;
    }
    !is_agent_host(snapshot, process) && app_bundle_path(exe).is_none()
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
    /// Primary Codex desktop app (for backwards compatibility with existing callers/schema).
    pub codex_app: Option<CodexApp>,
    /// All dynamically discovered desktop agent apps (Codex/ChatGPT, Antigravity/clones, OpenCode).
    pub desktop_apps: Vec<DesktopApp>,
    /// Standalone CLI, background service, and nested subagent sessions (Codex, OpenCode, Antigravity).
    pub agent_sessions: Vec<AgentSession>,
    pub orphans: SmallVec<[i32; 4]>,
    /// Desktop-bundled Antigravity `language_server` processes verified orphaned
    /// at `ppid == 1` with `--host_bridge_url=` after their enclosing `.app` exited.
    orphaned_antigravity_servers: SmallVec<[i32; 2]>,
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
            codex_app: None,
            desktop_apps: Vec::new(),
            agent_sessions: Vec::new(),
            orphans: SmallVec::new(),
            orphaned_antigravity_servers: SmallVec::new(),
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

    /// Walk up the parent chain from `pid` (excluding `pid`) to find the topmost
    /// desktop `.app` root whose bundle encloses `runtime_exe`.
    fn enclosing_bundle_app<'a>(
        &self,
        snapshot: &'a Snapshot,
        pid: i32,
        runtime_exe: &str,
    ) -> Option<(i32, &'a str)> {
        let mut current = self.process(snapshot, pid)?.ppid;
        let mut found = None;
        for _ in 0..32 {
            if current <= 1 {
                break;
            }
            let Some(parent) = self.process(snapshot, current) else {
                break;
            };
            if parent.uid == snapshot.user
                && !is_antigravity_sidecar(snapshot, parent)
                && let Some(bundle) = app_bundle_path(snapshot.exe(parent))
                && runtime_exe.starts_with(bundle)
                && runtime_exe.as_bytes().get(bundle.len()) == Some(&b'/')
            {
                found = Some((parent.pid, bundle));
            }
            if parent.ppid == current {
                break;
            }
            current = parent.ppid;
        }
        found
    }

    /// True if `pid` has any ancestor (excluding itself) whose executable lives
    /// inside `bundle/`. Prevents inner Electron sidecars like `Antigravity.real`
    /// under `language_server.real` from looking like a second desktop app.
    fn has_same_bundle_ancestor(&self, snapshot: &Snapshot, pid: i32, bundle: &str) -> bool {
        let mut current = match self.process(snapshot, pid) {
            Some(p) => p.ppid,
            None => return false,
        };
        for _ in 0..32 {
            if current <= 1 {
                break;
            }
            let Some(parent) = self.process(snapshot, current) else {
                break;
            };
            let exe = snapshot.exe(parent);
            if exe.starts_with(bundle) && exe.as_bytes().get(bundle.len()) == Some(&b'/') {
                return true;
            }
            if parent.ppid == current {
                break;
            }
            current = parent.ppid;
        }
        false
    }

    /// Find the nearest enclosing agent (desktop app or session) above `pid`, if any.
    fn enclosing_agent_name(&self, snapshot: &Snapshot, pid: i32) -> Option<CompactString> {
        let mut current = self.process(snapshot, pid)?.ppid;
        for _ in 0..32 {
            if current <= 1 {
                break;
            }
            if let Some(app) = self.desktop_apps.iter().find(|app| app.pid == current) {
                return Some(app.app_name.clone());
            }
            if let Some(session) = self
                .agent_sessions
                .iter()
                .find(|s| s.pid == current && s.agent == AgentKind::Other)
                && let Some(name) = &session.name
            {
                return Some(name.clone());
            }
            let Some(parent) = self.process(snapshot, current) else {
                break;
            };
            if parent.uid == snapshot.user {
                if is_claude_session(snapshot, parent) {
                    return Some(CompactString::const_new("claude"));
                }
                if is_codex(snapshot, parent) {
                    return Some(CompactString::const_new("codex"));
                }
                if is_opencode_runtime(snapshot, parent) {
                    return Some(CompactString::const_new("opencode"));
                }
                if is_antigravity_cli(snapshot, parent) {
                    return Some(CompactString::const_new("agy"));
                }
                if is_antigravity_server(snapshot, parent) {
                    // Continue climbing if this language_server belongs to a desktop app
                    // so we report the real app name (e.g. "Antigravity US").
                    if let Some(app) = self
                        .desktop_apps
                        .iter()
                        .find(|app| app.runtimes.contains(&parent.pid))
                    {
                        return Some(app.app_name.clone());
                    }
                    return Some(CompactString::const_new("antigravity"));
                }
            }
            if parent.ppid == current {
                break;
            }
            current = parent.ppid;
        }
        None
    }

    /// True if any ancestor of `pid` below `stop_at` is already in sorted `roots`.
    fn has_ancestor_in(&self, snapshot: &Snapshot, pid: i32, stop_at: i32, roots: &[i32]) -> bool {
        if roots.is_empty() {
            return false;
        }
        let mut current = match self.process(snapshot, pid) {
            Some(p) => p.ppid,
            None => return false,
        };
        for _ in 0..32 {
            if current <= 1 || current == stop_at {
                return false;
            }
            if roots.binary_search(&current).is_ok() {
                return true;
            }
            let Some(parent) = self.process(snapshot, current) else {
                return false;
            };
            if parent.ppid == current {
                return false;
            }
            current = parent.ppid;
        }
        false
    }

    fn classify(&mut self, snapshot: &Snapshot, known_helpers: &BTreeSet<Identity>) {
        // 1. Collect all canonical user runtimes, Claude sessions, and detached OpenCode services.
        let mut claude_pids: Vec<i32> = Vec::new();
        let mut codex_runtimes: Vec<i32> = Vec::new();
        let mut antigravity_servers: Vec<i32> = Vec::new();
        let mut antigravity_clis: Vec<i32> = Vec::new();
        let mut opencode_runtimes: Vec<i32> = Vec::new();
        let mut opencode_services: Vec<i32> = Vec::new();
        let mut has_orphan_candidates = false;
        let mut has_opencode_pty = false;

        for (idx, process) in snapshot.processes.iter().enumerate() {
            if process.uid != snapshot.user
                || !self.is_canonical_process(snapshot, idx, process.pid)
            {
                continue;
            }
            if is_claude_session(snapshot, process) {
                claude_pids.push(process.pid);
            } else if is_codex(snapshot, process) {
                codex_runtimes.push(process.pid);
            } else if is_antigravity_server(snapshot, process) {
                antigravity_servers.push(process.pid);
            } else if is_antigravity_cli(snapshot, process) {
                antigravity_clis.push(process.pid);
            } else if is_opencode_runtime(snapshot, process) {
                if is_opencode_service(snapshot, process) {
                    opencode_services.push(process.pid);
                } else {
                    opencode_runtimes.push(process.pid);
                }
            } else {
                if is_opencode_pty_exe(snapshot.exe(process)) {
                    has_opencode_pty = true;
                }
                if !has_orphan_candidates
                    && is_orphan_base_candidate(snapshot, process, snapshot.user)
                {
                    has_orphan_candidates = true;
                }
            }
        }

        // 2. Dynamically discover running desktop agent apps by walking up from
        //    bundled runtimes to their enclosing `.app` root. This works for
        //    `ChatGPT.app`, `Codex.app`, `Antigravity.app`, `Antigravity US.app`,
        //    `OpenCode.app`, and any renamed/cloned bundle without hardcoding.
        let mut discovered: Vec<(i32, AgentKind, CompactString)> = Vec::new();
        let mut register_app = |pid: i32, agent: AgentKind, bundle: &str| {
            if !discovered.iter().any(|(existing, _, _)| *existing == pid) {
                discovered.push((pid, agent, CompactString::from(bundle)));
            }
        };

        for &pid in &codex_runtimes {
            if let Some(process) = self.process(snapshot, pid)
                && let Some((app_pid, bundle)) =
                    self.enclosing_bundle_app(snapshot, pid, snapshot.exe(process))
            {
                register_app(app_pid, AgentKind::Codex, bundle);
            }
        }
        for &pid in &antigravity_servers {
            if let Some(process) = self.process(snapshot, pid)
                && let Some((app_pid, bundle)) =
                    self.enclosing_bundle_app(snapshot, pid, snapshot.exe(process))
            {
                register_app(app_pid, AgentKind::Antigravity, bundle);
            }
        }
        for &pid in &opencode_runtimes {
            if let Some(process) = self.process(snapshot, pid)
                && let Some((app_pid, bundle)) =
                    self.enclosing_bundle_app(snapshot, pid, snapshot.exe(process))
            {
                register_app(app_pid, AgentKind::Opencode, bundle);
            }
        }

        // Fallback discovery for desktop apps that have not spawned a bundled
        // runtime yet (e.g. OpenCode.app using a detached service, or a freshly
        // started ChatGPT/Codex/Antigravity app).
        for (idx, process) in snapshot.processes.iter().enumerate() {
            if process.uid != snapshot.user
                || !self.is_canonical_process(snapshot, idx, process.pid)
                || is_antigravity_sidecar(snapshot, process)
            {
                continue;
            }
            let exe = snapshot.exe(process);
            let Some(bundle) = app_bundle_path(exe) else {
                continue;
            };
            if self.has_same_bundle_ancestor(snapshot, process.pid, bundle) {
                continue;
            }
            if is_codex_app_exe(exe) {
                register_app(process.pid, AgentKind::Codex, bundle);
            } else if is_antigravity_app_fallback_exe(exe) {
                register_app(process.pid, AgentKind::Antigravity, bundle);
            } else if is_opencode_app_fallback_exe(exe) {
                register_app(process.pid, AgentKind::Opencode, bundle);
            }
        }
        discovered.sort_unstable_by_key(|(pid, _, _)| *pid);

        let pool_sigs: Vec<&ServerSignature> = snapshot
            .mcp_servers
            .iter()
            .filter(|server| server.is_pool_signature())
            .collect();

        for pid in claude_pids {
            let helpers = self.claude_helper_children(snapshot, pid, &pool_sigs);
            self.claude_sessions.push(Session { pid, helpers });
        }

        // 3. Build DesktopApp records and partition their runtime children into pools vs commands.
        let mut desktop_runtime_pids: Vec<i32> = Vec::new();
        let mut attached_opencode_service: Option<i32> = None;

        for (app_pid, agent, bundle_path) in discovered {
            let mut in_app = self.descendants(snapshot, app_pid);
            in_app.sort_unstable();
            let bundle = bundle_path.as_str();
            let in_bundle = |exe: &str| {
                exe.starts_with(bundle) && exe.as_bytes().get(bundle.len()) == Some(&b'/')
            };
            let is_owned_exe = |exe: &str| {
                in_bundle(exe)
                    || (agent == AgentKind::Opencode
                        && exe.contains("/Application Support/ai.opencode.desktop/"))
            };

            let mut runtimes: SmallVec<[i32; 4]> = in_app
                .iter()
                .copied()
                .filter(|&pid| {
                    self.process(snapshot, pid).is_some_and(|p| {
                        let exe = snapshot.exe(p);
                        if !is_owned_exe(exe) {
                            return false;
                        }
                        match agent {
                            AgentKind::Codex => {
                                is_codex(snapshot, p) && !is_codex_exec(snapshot, p)
                            }
                            AgentKind::Antigravity => is_antigravity_server(snapshot, p),
                            AgentKind::Opencode => is_opencode_runtime(snapshot, p),
                            AgentKind::Claude | AgentKind::Other => false,
                        }
                    })
                })
                .collect();
            // Keep only top-level runtime roots so nested runtimes under a runtime
            // are not double-counted in `desktop_app_footprint`.
            runtimes.sort_unstable();
            let rt_snapshot: SmallVec<[i32; 4]> = runtimes.clone();
            runtimes.retain(|pid| !self.has_ancestor_in(snapshot, *pid, app_pid, &rt_snapshot));

            let data_dir = if agent == AgentKind::Antigravity {
                let dir = runtimes
                    .iter()
                    .find_map(|&rt_pid| {
                        self.process(snapshot, rt_pid)
                            .and_then(|p| extract_antigravity_data_dir(snapshot.args(p)))
                    })
                    .unwrap_or(DEFAULT_ANTIGRAVITY_DATA_DIR);
                Some(CompactString::from(dir))
            } else {
                None
            };

            // OpenCode.app talks to the detached `opencode-cli serve --service`
            // daemon (ppid == 1); attach it to the first running OpenCode app.
            let mut attached_service = None;
            if agent == AgentKind::Opencode
                && attached_opencode_service.is_none()
                && let Some(&svc_pid) = opencode_services.first()
            {
                attached_opencode_service = Some(svc_pid);
                attached_service = Some(svc_pid);
                if !runtimes.contains(&svc_pid) {
                    runtimes.push(svc_pid);
                }
            }

            desktop_runtime_pids.extend(runtimes.iter().copied());

            let mut pools = Vec::new();
            let mut commands: SmallVec<[i32; 4]> = SmallVec::new();

            for &runtime in &runtimes {
                for &(_, child_pid) in self.direct_children(runtime) {
                    let Some(child) = self.process(snapshot, child_pid) else {
                        continue;
                    };
                    if is_shell(snapshot, child) || child.has_tty {
                        continue;
                    }
                    if runtimes.contains(&child_pid) {
                        continue;
                    }
                    let exe = snapshot.exe(child);
                    let args = snapshot.args(child);
                    let is_external_agent = !is_owned_exe(exe)
                        && (is_claude_session(snapshot, child)
                            || is_codex(snapshot, child)
                            || is_opencode_runtime(snapshot, child)
                            || is_antigravity_cli(snapshot, child)
                            || is_antigravity_server(snapshot, child));
                    if is_external_agent || is_codex_exec(snapshot, child) {
                        commands.push(child_pid);
                        continue;
                    }
                    let is_pool = !is_non_helper_command_exe(exe)
                        && (in_bundle(exe)
                            || is_agent_runtime_helper(agent, exe, args, data_dir.as_deref())
                            || pool_sigs
                                .iter()
                                .any(|server| server.matches(snapshot, child)));
                    if is_pool {
                        pools.push(child_pid);
                    } else {
                        commands.push(child_pid);
                    }
                }
            }

            // Sweep descendants of the app (and any attached OpenCode service)
            // for shells, TTY processes, `codex exec`, or nested external agents.
            let mut sweep_candidates: Vec<i32> = Vec::new();
            let mut collect_work = |descendants: &[i32]| {
                for &pid in descendants {
                    let Some(process) = self.process(snapshot, pid) else {
                        continue;
                    };
                    let exe = snapshot.exe(process);
                    let is_external_agent = !is_owned_exe(exe)
                        && (is_claude_session(snapshot, process)
                            || is_codex(snapshot, process)
                            || is_opencode_runtime(snapshot, process)
                            || is_antigravity_cli(snapshot, process)
                            || is_antigravity_server(snapshot, process));
                    if is_shell(snapshot, process)
                        || process.has_tty
                        || is_codex_exec(snapshot, process)
                        || is_external_agent
                    {
                        sweep_candidates.push(pid);
                    }
                }
            };
            collect_work(&in_app);
            if let Some(svc_pid) = attached_service {
                let svc_desc = self.descendants(snapshot, svc_pid);
                collect_work(&svc_desc);
            }

            commands.extend(sweep_candidates);
            commands.sort_unstable();
            commands.dedup();
            // Keep top-level command roots so a shell or TTY process with child
            // processes is counted once instead of once per descendant.
            let roots_snapshot: SmallVec<[i32; 8]> = commands.iter().copied().collect();
            commands.retain(|pid| !self.has_ancestor_in(snapshot, *pid, app_pid, &roots_snapshot));

            pools.sort_unstable();
            pools.dedup();

            let app_name = CompactString::from(app_bundle_name(bundle));
            self.desktop_apps.push(DesktopApp {
                agent,
                pid: app_pid,
                app_name,
                bundle_path,
                data_dir,
                runtimes,
                pools,
                commands,
                attached_service,
            });
        }

        desktop_runtime_pids.sort_unstable();
        desktop_runtime_pids.dedup();

        // Populate backwards-compatible `codex_app` from the first Codex desktop app.
        if let Some(codex) = self
            .desktop_apps
            .iter()
            .find(|app| app.agent == AgentKind::Codex)
        {
            self.codex_app = Some(CodexApp {
                pid: codex.pid,
                runtimes: codex.runtimes.clone(),
                pools: codex.pools.clone(),
                commands: codex.commands.clone(),
            });
        }

        // Identify desktop-bundled Antigravity `language_server` processes left
        // orphaned at `ppid == 1` with `--host_bridge_url=` after their desktop
        // app exited (or relaunched with a new child `language_server`).
        let is_orphaned_desktop_ag_server =
            |process: &Process| -> bool {
                process.ppid == 1
                    && process.uid == snapshot.user
                    && !process.has_tty
                    && process.age_secs >= ORPHAN_MIN_AGE_SECS
                    && is_antigravity_server(snapshot, process)
                    && has_host_bridge_url_flag(snapshot.args(process))
                    && enclosing_app_bundle(snapshot.exe(process)).is_some_and(|bundle| match self
                        .desktop_apps
                        .iter()
                        .find(|app| app.bundle_path.as_str() == bundle)
                    {
                        None => true,
                        Some(app) => !app.runtimes.is_empty(),
                    })
            };

        let mut orphaned_ag_candidates: SmallVec<[i32; 2]> = SmallVec::new();
        for &pid in &antigravity_servers {
            if desktop_runtime_pids.binary_search(&pid).is_err()
                && self
                    .process(snapshot, pid)
                    .is_some_and(is_orphaned_desktop_ag_server)
            {
                orphaned_ag_candidates.push(pid);
            }
        }

        // 4. Classify standalone CLI, background service, and nested subagent sessions
        //    (Codex CLI / Codex under Antigravity or Claude, OpenCode service/CLI,
        //    Antigravity CLI `agy` or standalone `language_server`).
        let mut standalone_candidates: Vec<(i32, AgentKind, bool)> = Vec::new();
        for &pid in &codex_runtimes {
            if desktop_runtime_pids.binary_search(&pid).is_err() {
                standalone_candidates.push((pid, AgentKind::Codex, false));
            }
        }
        for &pid in &opencode_services {
            if attached_opencode_service != Some(pid) {
                standalone_candidates.push((pid, AgentKind::Opencode, true));
            }
        }
        for &pid in &opencode_runtimes {
            if desktop_runtime_pids.binary_search(&pid).is_err() {
                standalone_candidates.push((pid, AgentKind::Opencode, false));
            }
        }
        for &pid in &antigravity_clis {
            standalone_candidates.push((pid, AgentKind::Antigravity, false));
        }
        for &pid in &antigravity_servers {
            if desktop_runtime_pids.binary_search(&pid).is_err()
                && !orphaned_ag_candidates.contains(&pid)
            {
                standalone_candidates.push((pid, AgentKind::Antigravity, false));
            }
        }
        standalone_candidates.sort_unstable_by_key(|(pid, _, _)| *pid);

        for (pid, agent, is_service) in standalone_candidates {
            let (helpers, commands) =
                self.partition_runtime_children(snapshot, pid, agent, &pool_sigs);
            let (mode, parent_agent) = if is_service {
                (SessionMode::Service, None)
            } else if let Some(parent) = self.enclosing_agent_name(snapshot, pid) {
                (SessionMode::Subagent, Some(parent))
            } else {
                (SessionMode::Cli, None)
            };
            self.agent_sessions.push(AgentSession {
                agent,
                mode,
                pid,
                name: None,
                parent_agent,
                helpers,
                commands,
            });
        }

        // 4b. Generic dynamic discovery for any other running coding tool, IDE,
        //     or CLI (e.g. Cursor, Windsurf, Zed, Gemini CLI, Cline, Claude Desktop
        //     direct MCPs, or future tools/clones) parenting unattributed processes
        //     that match configured MCP `pool_sigs`. Observed in `status` and
        //     recorded in `known_helpers` so leaked helpers are reaped by `orphan`
        //     after exit, while never stopping live helpers of unknown tools.
        if !pool_sigs.is_empty() {
            let mut claimed_helpers: Vec<i32> = Vec::new();
            let mut claimed_commands: Vec<i32> = Vec::new();
            let mut known_parents: Vec<i32> = Vec::new();
            for s in &self.claude_sessions {
                known_parents.push(s.pid);
                claimed_helpers.extend(s.helpers.iter().copied());
            }
            for app in &self.desktop_apps {
                known_parents.extend(app.runtimes.iter().copied());
                claimed_helpers.extend(app.pools.iter().copied());
                claimed_commands.extend(app.commands.iter().copied());
            }
            for s in &self.agent_sessions {
                known_parents.push(s.pid);
                claimed_helpers.extend(s.helpers.iter().copied());
                claimed_commands.extend(s.commands.iter().copied());
            }
            claimed_helpers.extend(orphaned_ag_candidates.iter().copied());
            claimed_helpers.sort_unstable();
            claimed_helpers.dedup();
            claimed_commands.sort_unstable();
            claimed_commands.dedup();
            known_parents.sort_unstable();
            known_parents.dedup();

            let mut generic_mcp_pids: Vec<i32> = Vec::new();
            for (idx, process) in snapshot.processes.iter().enumerate() {
                if process.uid != snapshot.user
                    || process.ppid <= 1
                    || !self.is_canonical_process(snapshot, idx, process.pid)
                {
                    continue;
                }
                let exe = snapshot.exe(process);
                if is_shell_exe(exe)
                    || is_non_helper_command_exe(exe)
                    || is_agent_host(snapshot, process)
                    || app_bundle_path(exe).is_some()
                {
                    continue;
                }
                if known_parents.binary_search(&process.ppid).is_ok()
                    || claimed_helpers.binary_search(&process.pid).is_ok()
                    || claimed_commands.binary_search(&process.pid).is_ok()
                    || self.has_ancestor_in(snapshot, process.pid, 0, &claimed_helpers)
                    || self.has_ancestor_in(snapshot, process.pid, 0, &claimed_commands)
                {
                    continue;
                }
                if pool_sigs
                    .iter()
                    .any(|server| server.matches(snapshot, process))
                {
                    generic_mcp_pids.push(process.pid);
                }
            }

            if !generic_mcp_pids.is_empty() {
                generic_mcp_pids.sort_unstable();
                let raw_mcp = generic_mcp_pids.clone();
                generic_mcp_pids.retain(|&pid| !self.has_ancestor_in(snapshot, pid, 0, &raw_mcp));

                let mut generic_apps: Vec<(i32, CompactString, Vec<i32>)> = Vec::new();
                let mut generic_sessions: Vec<(i32, SmallVec<[i32; 8]>)> = Vec::new();

                for h_pid in generic_mcp_pids {
                    let Some(helper) = self.process(snapshot, h_pid) else {
                        continue;
                    };
                    let mut cur = helper.ppid;
                    let mut enclosing_app: Option<(i32, &str)> = None;
                    let mut last_non_shell: Option<i32> = None;
                    let mut hit_known_agent = false;

                    for _ in 0..32 {
                        if cur <= 1 {
                            break;
                        }
                        let Some(parent) = self.process(snapshot, cur) else {
                            break;
                        };
                        if parent.uid != snapshot.user {
                            break;
                        }
                        if is_shell(snapshot, parent) && parent.has_tty {
                            break;
                        }
                        if is_claude_session(snapshot, parent)
                            || is_codex(snapshot, parent)
                            || is_antigravity_server(snapshot, parent)
                            || is_antigravity_cli(snapshot, parent)
                            || is_opencode_runtime(snapshot, parent)
                        {
                            // Descendant of a known agent's shell/command subtree,
                            // not a separate unknown coding agent.
                            hit_known_agent = true;
                            break;
                        }
                        let parent_exe = snapshot.exe(parent);
                        if !is_antigravity_sidecar(snapshot, parent)
                            && let Some(bundle) = app_bundle_path(parent_exe)
                        {
                            enclosing_app = Some((parent.pid, bundle));
                        }
                        if !is_shell(snapshot, parent) && !is_non_helper_command_exe(parent_exe) {
                            last_non_shell = Some(parent.pid);
                        }
                        if parent.ppid == cur {
                            break;
                        }
                        cur = parent.ppid;
                    }

                    if hit_known_agent {
                        continue;
                    }

                    if let Some((app_pid, bundle)) = enclosing_app {
                        if let Some(existing) =
                            self.desktop_apps.iter_mut().find(|a| a.pid == app_pid)
                        {
                            if !existing.pools.contains(&h_pid) {
                                existing.pools.push(h_pid);
                                existing.pools.sort_unstable();
                            }
                        } else if let Some((_, _, pools)) =
                            generic_apps.iter_mut().find(|(pid, _, _)| *pid == app_pid)
                        {
                            pools.push(h_pid);
                        } else {
                            generic_apps.push((app_pid, CompactString::from(bundle), vec![h_pid]));
                        }
                    } else if let Some(owner_pid) = last_non_shell
                        && let Some(owner_proc) = self.process(snapshot, owner_pid)
                        && !is_orphan_base_candidate(snapshot, owner_proc, snapshot.user)
                    {
                        if let Some((_, helpers)) = generic_sessions
                            .iter_mut()
                            .find(|(pid, _)| *pid == owner_pid)
                        {
                            helpers.push(h_pid);
                        } else {
                            let mut helpers = SmallVec::new();
                            helpers.push(h_pid);
                            generic_sessions.push((owner_pid, helpers));
                        }
                    }
                }

                let added_generic_owners = !generic_apps.is_empty() || !generic_sessions.is_empty();
                for (app_pid, bundle_path, mut pools) in generic_apps {
                    pools.sort_unstable();
                    pools.dedup();
                    let app_name = CompactString::from(app_bundle_name(bundle_path.as_str()));
                    let agent = self
                        .process(snapshot, app_pid)
                        .filter(|p| is_claude_app_exe(snapshot.exe(p)))
                        .map_or(AgentKind::Other, |_| AgentKind::Claude);
                    self.desktop_apps.push(DesktopApp {
                        agent,
                        pid: app_pid,
                        app_name,
                        bundle_path,
                        data_dir: None,
                        runtimes: SmallVec::new(),
                        pools,
                        commands: SmallVec::new(),
                        attached_service: None,
                    });
                }
                self.desktop_apps.sort_unstable_by_key(|app| app.pid);

                for (owner_pid, mut helpers) in generic_sessions {
                    let Some(owner_proc) = self.process(snapshot, owner_pid) else {
                        continue;
                    };
                    helpers.sort_unstable();
                    helpers.dedup();
                    let parent_agent = self.enclosing_agent_name(snapshot, owner_pid);
                    let mode = if parent_agent.is_some() {
                        SessionMode::Subagent
                    } else {
                        SessionMode::Cli
                    };
                    self.agent_sessions.push(AgentSession {
                        agent: AgentKind::Other,
                        mode,
                        pid: owner_pid,
                        name: Some(CompactString::from(snapshot.exe_name(owner_proc))),
                        parent_agent,
                        helpers,
                        commands: SmallVec::new(),
                    });
                }
                self.agent_sessions.sort_unstable_by_key(|s| s.pid);

                if added_generic_owners {
                    // Backfill `parent_agent` for subagent CLI runs (e.g. `codex`
                    // launched by `Cursor.app` or `gemini` CLI) discovered before
                    // generic owners.
                    for idx in 0..self.agent_sessions.len() {
                        if self.agent_sessions[idx].mode == SessionMode::Cli
                            && self.agent_sessions[idx].parent_agent.is_none()
                            && let Some(parent) =
                                self.enclosing_agent_name(snapshot, self.agent_sessions[idx].pid)
                        {
                            self.agent_sessions[idx].mode = SessionMode::Subagent;
                            self.agent_sessions[idx].parent_agent = Some(parent);
                        }
                    }
                }
            }
        }

        // 5. Orphans (ppid == 1, age >= 120s, non-launchd, known_helpers or distinctive MCP,
        //    orphaned `sidecar_sdk`, orphaned `opencode-pty` with no children when no OpenCode
        //    process is alive, or orphaned desktop-bundled Antigravity `language_server` with
        //    `--host_bridge_url=`).
        if !has_orphan_candidates && !has_opencode_pty && orphaned_ag_candidates.is_empty() {
            return;
        }
        // Without the launchd job list a service cannot be told from an orphan.
        let Some(launchd_jobs) = &snapshot.launchd_jobs else {
            return;
        };
        let distinctive: Vec<&ServerSignature> = snapshot
            .mcp_servers
            .iter()
            .filter(|server| server.is_distinctive())
            .collect();
        let launchd_sorted = launchd_jobs.is_sorted();
        let is_launchd_job = |pid: i32| {
            if launchd_sorted {
                launchd_jobs.binary_search(&pid).is_ok()
            } else {
                launchd_jobs.contains(&pid)
            }
        };
        let has_any_opencode = self
            .desktop_apps
            .iter()
            .any(|app| app.agent == AgentKind::Opencode)
            || self
                .agent_sessions
                .iter()
                .any(|s| s.agent == AgentKind::Opencode);

        for &pid in &orphaned_ag_candidates {
            if !is_launchd_job(pid) {
                self.orphaned_antigravity_servers.push(pid);
            }
        }

        self.orphans = snapshot
            .processes
            .iter()
            .enumerate()
            .filter(|&(idx, process)| {
                if !self.is_canonical_process(snapshot, idx, process.pid)
                    || is_launchd_job(process.pid)
                {
                    return false;
                }
                if self.orphaned_antigravity_servers.contains(&process.pid) {
                    return true;
                }
                is_orphan_base_candidate(snapshot, process, snapshot.user)
                    && (Identity::in_set(known_helpers, process.pid, snapshot.started(process))
                        || is_antigravity_sidecar(snapshot, process)
                        || distinctive
                            .iter()
                            .any(|server| server.matches(snapshot, process))
                        || (!has_any_opencode
                            && is_opencode_pty_exe(snapshot.exe(process))
                            && self.direct_children(process.pid).is_empty()))
            })
            .map(|(_, process)| process.pid)
            .collect();
    }

    fn partition_runtime_children(
        &self,
        snapshot: &Snapshot,
        runtime_pid: i32,
        agent: AgentKind,
        pool_sigs: &[&ServerSignature],
    ) -> (SmallVec<[i32; 8]>, SmallVec<[i32; 4]>) {
        let mut helpers: SmallVec<[i32; 8]> = SmallVec::new();
        let mut commands: SmallVec<[i32; 4]> = SmallVec::new();
        let runtime_proc = self.process(snapshot, runtime_pid);
        let runtime_has_tty = runtime_proc.is_some_and(|p| p.has_tty);
        let data_dir = if agent == AgentKind::Antigravity {
            runtime_proc.and_then(|p| extract_antigravity_data_dir(snapshot.args(p)))
        } else {
            None
        };
        for &(_, child_pid) in self.direct_children(runtime_pid) {
            let Some(child) = self.process(snapshot, child_pid) else {
                continue;
            };
            // A non-TTY runtime (e.g. `opencode-cli serve --service`) spawning a
            // TTY process is running an interactive terminal command. When the
            // runtime itself is attached to a PTY/terminal (`codex exec`, CLI),
            // its stdio MCP server children inherit the controlling TTY and must
            // still be classified as helpers via `is_pool` (never signalled while
            // `has_tty`, but tracked in `known_helpers` in case they orphan later).
            if is_shell(snapshot, child)
                || (!runtime_has_tty && child.has_tty)
                || is_agent_host(snapshot, child)
            {
                commands.push(child_pid);
                continue;
            }
            let exe = snapshot.exe(child);
            let args = snapshot.args(child);
            let is_pool = !is_non_helper_command_exe(exe)
                && (is_agent_runtime_helper(agent, exe, args, data_dir)
                    || pool_sigs
                        .iter()
                        .any(|server| server.matches(snapshot, child)));
            if is_pool {
                helpers.push(child_pid);
            } else {
                commands.push(child_pid);
            }
        }
        (helpers, commands)
    }

    /// Direct helper children of a Claude Code session:
    /// - For non-TTY children: every non-shell, non-agent, non-VCS child (matching
    ///   Claude Desktop and detached CLI MCP helpers).
    /// - For TTY children under a TTY-attached Claude CLI session: only children
    ///   on the exact same inherited controlling TTY (`tty_id`) matching
    ///   configured MCP `pool_sigs`, so stdio MCP helpers spawned by terminal
    ///   `claude` CLI are tracked and reclaimable when the session goes idle
    ///   without misclassifying interactive commands like `cargo test` or a new PTY.
    fn claude_helper_children(
        &self,
        snapshot: &Snapshot,
        pid: i32,
        pool_sigs: &[&ServerSignature],
    ) -> SmallVec<[i32; 8]> {
        let session_tty = self
            .process(snapshot, pid)
            .filter(|p| p.has_tty)
            .map(|p| p.tty_id);
        self.direct_children(pid)
            .iter()
            .map(|&(_, child)| child)
            .filter(|child| {
                self.process(snapshot, *child).is_some_and(|process| {
                    if is_shell(snapshot, process)
                        || is_agent_host(snapshot, process)
                        || is_non_helper_command_exe(snapshot.exe(process))
                    {
                        return false;
                    }
                    if process.has_tty {
                        session_tty == Some(process.tty_id)
                            && pool_sigs
                                .iter()
                                .any(|server| server.matches(snapshot, process))
                    } else {
                        true
                    }
                })
            })
            .collect()
    }

    /// Direct child PIDs of `ppid`.
    pub fn direct_child_pids(&self, ppid: i32) -> impl Iterator<Item = i32> + '_ {
        self.direct_children(ppid).iter().map(|&(_, child)| child)
    }

    /// True if any discovered desktop app matches `bundle`.
    pub fn is_bundle_running(&self, bundle: &std::path::Path) -> bool {
        let Some(bundle_str) = bundle.to_str() else {
            return false;
        };
        self.desktop_apps
            .iter()
            .any(|app| app.bundle_path.as_str() == bundle_str)
    }

    /// True if any OpenCode desktop app or non-service OpenCode CLI client is running.
    pub fn has_opencode_clients(&self) -> bool {
        self.desktop_apps
            .iter()
            .any(|app| app.agent == AgentKind::Opencode)
            || self
                .agent_sessions
                .iter()
                .any(|s| s.agent == AgentKind::Opencode && s.mode != SessionMode::Service)
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

    /// Physical footprint reclaimed by restarting `app`:
    /// - Codex: runtimes and everything under them (excluding unrelated ChatGPT UI).
    /// - Antigravity: the whole app tree (Electron renderers + `language_server`
    ///   + pools), excluding active command/subagent subtrees to avoid double-counting.
    /// - OpenCode: the app tree plus any attached detached `opencode-cli serve --service`,
    ///   excluding active command/subagent subtrees.
    pub fn desktop_app_footprint(&self, snapshot: &Snapshot, app: &DesktopApp) -> u64 {
        let commands_footprint = |app: &DesktopApp| -> u64 {
            app.commands
                .iter()
                .map(|&pid| self.tree_footprint(snapshot, pid))
                .sum()
        };
        match app.agent {
            AgentKind::Codex => app
                .runtimes
                .iter()
                .map(|runtime| self.tree_footprint(snapshot, *runtime))
                .sum(),
            AgentKind::Antigravity => self
                .tree_footprint(snapshot, app.pid)
                .saturating_sub(commands_footprint(app)),
            AgentKind::Opencode => {
                let mut total = self.tree_footprint(snapshot, app.pid);
                if let Some(svc) = app.attached_service {
                    total += self.tree_footprint(snapshot, svc);
                }
                total.saturating_sub(commands_footprint(app))
            }
            AgentKind::Claude | AgentKind::Other => app
                .pools
                .iter()
                .map(|&pool| self.tree_footprint(snapshot, pool))
                .sum(),
        }
    }

    /// Insert helper roots currently serving an agent directly into `identities`
    /// without cloning `started` for processes already present.
    pub fn extend_helper_identities(
        &self,
        snapshot: &Snapshot,
        identities: &mut BTreeSet<Identity>,
    ) {
        let mut record_pid = |pid: i32| {
            if let Some(process) = self.process(snapshot, pid) {
                let exe = snapshot.exe(process);
                // Note: TTY-attached MCP helpers of CLI/subagent runs (`codex exec`,
                // terminal `claude`, generic CLIs) ARE recorded here so that if
                // their parent exits and closes the PTY (leaving them at
                // `ppid == 1, tty == ??`), the orphan rule can reap them.
                // Antigravity's `sidecar_sdk` runs via its Electron binary inside
                // `Contents/MacOS/`, so allow it while excluding real desktop apps.
                if is_non_helper_command_exe(exe)
                    || (app_bundle_path(exe).is_some()
                        && !is_antigravity_sidecar(snapshot, process))
                {
                    return;
                }
                if !Identity::in_set(identities, process.pid, snapshot.started(process)) {
                    identities.insert(snapshot.identity(process));
                }
            }
        };
        for session in &self.claude_sessions {
            for &helper in &session.helpers {
                record_pid(helper);
            }
        }
        for session in &self.agent_sessions {
            for &helper in &session.helpers {
                record_pid(helper);
            }
        }
        for app in &self.desktop_apps {
            for &pool in &app.pools {
                record_pid(pool);
            }
            if !matches!(
                app.agent,
                AgentKind::Codex | AgentKind::Antigravity | AgentKind::Opencode
            ) {
                continue;
            }
            // The app's own native helpers (e.g. `bare-modifier-monitor`) may
            // outlive a quit; remembering them lets the orphan rule reclaim them
            // after a restart. Skip Electron framework helpers (Renderers/GPU)
            // which churn across tabs and exit with the app.
            for &(_, child) in self.direct_children(app.pid) {
                if let Some(process) = self.process(snapshot, child) {
                    let exe = snapshot.exe(process);
                    if !is_shell(snapshot, process)
                        && !is_agent_host(snapshot, process)
                        && !process.has_tty
                        && !is_non_helper_command_exe(exe)
                        && !exe.contains("/Contents/Frameworks/")
                    {
                        record_pid(child);
                    }
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

    /// True if `process` is protected from `Stop` rules in the context of this
    /// ownership classification. When `inherited_tty` is `Some(tty_id)`, a
    /// stdio MCP helper (and its non-shell, non-agent, non-VCS worker
    /// descendants) carrying the exact controlling TTY inherited from its parent
    /// `claude` CLI session is not rejected on TTY alone; shells, VCS/system
    /// utilities (`git`, `ssh`, `gpg`, etc.), interactive TTY editors/pagers,
    /// agent hosts, desktop apps, and processes on any other TTY remain strictly
    /// protected.
    pub fn is_protected_with_inherited_tty(
        &self,
        snapshot: &Snapshot,
        process: &Process,
        inherited_tty: Option<u16>,
    ) -> bool {
        let exe = snapshot.exe(process);
        if is_shell_exe(exe) || is_non_helper_command_exe(exe) {
            return true;
        }
        if process.has_tty
            && (inherited_tty != Some(process.tty_id) || is_interactive_tty_program_exe(exe))
        {
            return true;
        }
        if self.desktop_apps.iter().any(|app| app.pid == process.pid) {
            return true;
        }
        if self.orphaned_antigravity_servers.contains(&process.pid) {
            return false;
        }
        is_agent_host(snapshot, process)
    }

    /// True if `root` or any of its descendants is protected from signalling,
    /// optionally permitting `inherited_tty` (used strictly for configured MCP
    /// helper trees under an idle TTY-attached `claude` CLI session).
    pub fn is_tree_protected_with_inherited_tty(
        &self,
        snapshot: &Snapshot,
        root: i32,
        inherited_tty: Option<u16>,
    ) -> bool {
        self.process(snapshot, root)
            .is_none_or(|p| self.is_protected_with_inherited_tty(snapshot, p, inherited_tty))
            || self.any_descendant(snapshot, root, |p| {
                self.is_protected_with_inherited_tty(snapshot, p, inherited_tty)
            })
    }

    /// True if `root` or any of its descendants is protected from signalling.
    pub fn is_tree_protected(&self, snapshot: &Snapshot, root: i32) -> bool {
        self.is_tree_protected_with_inherited_tty(snapshot, root, None)
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

    #[test]
    fn codex_pools_recognize_generic_launchers_wrapper_scripts_and_external_helpers() {
        let mut snapshot = machine();
        snapshot.processes.retain(|p| p.pid != 213);
        push_server(
            &mut snapshot,
            ServerSignature {
                command: "npx".into(),
                args: smallvec!["-y".into(), "next-devtools-mcp@latest".into()],
            },
        );
        push_server(
            &mut snapshot,
            ServerSignature {
                command: "uvx".into(),
                args: smallvec!["mcp-server-git".into()],
            },
        );
        push_server(
            &mut snapshot,
            ServerSignature {
                command: "/Users/u/.config/bitrina-grafana/mcp-grafana.sh".into(),
                args: smallvec![],
            },
        );
        // Generic-launcher MCP servers, wrapper script, and external Codex helpers.
        add_proc(
            &mut snapshot,
            220,
            210,
            "npm exec next-devtools-mcp@latest",
            "npm exec next-devtools-mcp@latest",
            1800,
            80,
        );
        add_proc(
            &mut snapshot,
            221,
            210,
            "/opt/homebrew/bin/python3",
            "python3 /Users/u/.cache/uv/bin/mcp-server-git",
            1800,
            40,
        );
        add_proc(
            &mut snapshot,
            222,
            210,
            "/opt/homebrew/bin/mcp-grafana",
            "/opt/homebrew/bin/mcp-grafana --disable-write",
            1800,
            20,
        );
        add_proc(
            &mut snapshot,
            223,
            210,
            "/Users/u/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient",
            "SkyComputerUseClient mcp",
            1800,
            50,
        );
        add_proc(
            &mut snapshot,
            224,
            210,
            "/Users/u/.cache/codex-runtimes/codex-primary-runtime/bin/node",
            "node runtime.js",
            1800,
            60,
        );

        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        let app = ownership.codex_app.as_ref().unwrap();
        assert_eq!(app.pools, [211, 212, 220, 221, 222, 223, 224]);
        assert!(
            app.commands.is_empty(),
            "unexpected commands: {:?}",
            app.commands
        );
    }

    #[test]
    fn git_is_never_recorded_as_a_helper_or_stopped_as_an_orphan() {
        let mut snapshot = machine();
        // Direct git child of Claude session 120 and detached git gc at ppid 1.
        add_proc(
            &mut snapshot,
            125,
            120,
            "/usr/bin/git",
            "git status",
            600,
            10,
        );
        add_proc(
            &mut snapshot,
            410,
            1,
            "/usr/bin/git",
            "git gc --auto",
            600,
            30,
        );
        let mut known = known_gopls();
        // Even if an older state.json remembered a git PID, it must never be reaped.
        known.insert(crate::model::Identity {
            pid: 410,
            started: "start-410".into(),
        });
        let ownership = Ownership::new(&snapshot, &known);
        let s120 = ownership
            .claude_sessions
            .iter()
            .find(|s| s.pid == 120)
            .unwrap();
        assert!(!s120.helpers.contains(&125));
        assert!(
            !ownership
                .helper_identities(&snapshot)
                .iter()
                .any(|id| id.pid == 125)
        );
        assert!(!ownership.orphans.contains(&410));
    }

    #[test]
    fn antigravity_clone_codex_app_nested_subagents_and_opencode_are_discovered_dynamically() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
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
        // Antigravity US clone (and proxy wrappers `.real`) with sidecar + codegraph + nested codex!
        add_proc(
            &mut snapshot,
            44215,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real --proxy-server=http://127.0.0.1:18099",
            90_000,
            277,
        );
        add_proc(
            &mut snapshot,
            44926,
            44215,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --standalone --override_ide_name antigravity",
            90_000,
            18_000,
        );
        add_proc(&mut snapshot, 44942, 44926, NODE, CODEGRAPH, 90_000, 49);
        add_proc(
            &mut snapshot,
            44943,
            44926,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real --import=file:///Users/u/.gemini/antigravity/lib/node_modules/sidecar_sdk/register_loader.js main.mjs",
            90_000,
            37,
        );
        // Nested codex CLI invoked under Antigravity US, with its own MCP server and a git child.
        add_proc(
            &mut snapshot,
            50000,
            44926,
            "/Users/u/.nvm/versions/node/v26/bin/codex",
            "codex exec review",
            600,
            120,
        );
        add_proc(&mut snapshot, 50001, 50000, NODE, CODEGRAPH, 600, 64);
        add_proc(
            &mut snapshot,
            50002,
            50000,
            "/usr/bin/git",
            "git diff",
            10,
            5,
        );

        // Standalone Codex.app (not ChatGPT.app) dynamically discovered.
        add_proc(
            &mut snapshot,
            60000,
            1,
            "/Applications/Codex.app/Contents/MacOS/Codex",
            "Codex",
            5000,
            300,
        );
        add_proc(
            &mut snapshot,
            60001,
            60000,
            "/Applications/Codex.app/Contents/Resources/codex-cli/codex",
            "codex app-server",
            5000,
            600,
        );

        // Headless OpenCode background service (no OpenCode.app running) with codegraph child.
        add_proc(
            &mut snapshot,
            37642,
            1,
            "/Users/u/Library/Application Support/ai.opencode.desktop/cli/2.4.1/opencode-cli",
            "opencode-cli serve --service",
            90_000,
            325,
        );
        add_proc(&mut snapshot, 37643, 37642, NODE, CODEGRAPH, 900, 50);

        // Claude Code invoking nested codex CLI.
        add_proc(&mut snapshot, 70000, 1, CLAUDE, "claude", 5000, 200);
        add_proc(
            &mut snapshot,
            70001,
            70000,
            "/bin/zsh",
            "zsh -c codex",
            100,
            4,
        );
        add_proc(
            &mut snapshot,
            70002,
            70001,
            "/Users/u/.nvm/versions/node/v26/bin/codex",
            "codex exec fix",
            100,
            100,
        );
        add_proc(
            &mut snapshot,
            70003,
            70002,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            100,
            10,
        );

        let ownership = Ownership::new(&snapshot, &BTreeSet::new());

        // Desktop apps: Antigravity US and Codex.app (sidecar 44943 is NOT a desktop app!).
        let ag = ownership
            .desktop_apps
            .iter()
            .find(|a| a.pid == 44215)
            .expect("Antigravity US discovered");
        assert_eq!(ag.agent, crate::model::AgentKind::Antigravity);
        assert_eq!(ag.app_name, "Antigravity US");
        assert_eq!(ag.bundle_path, "/Users/u/Applications/Antigravity US.app");
        assert_eq!(ag.runtimes.as_slice(), [44926]);
        assert_eq!(ag.pools, [44942, 44943]);
        // Nested codex 50000 marks Antigravity US as busy!
        assert_eq!(ag.commands.as_slice(), [50000]);

        let codex_app = ownership
            .desktop_apps
            .iter()
            .find(|a| a.pid == 60000)
            .expect("Codex.app discovered");
        assert_eq!(codex_app.agent, crate::model::AgentKind::Codex);
        assert_eq!(codex_app.app_name, "Codex");
        assert_eq!(ownership.codex_app.as_ref().unwrap().pid, 60000);

        // Nested codex under Antigravity US and under Claude Code, plus OpenCode service.
        let codex_under_ag = ownership
            .agent_sessions
            .iter()
            .find(|s| s.pid == 50000)
            .expect("nested codex under Antigravity US");
        assert_eq!(codex_under_ag.mode, crate::model::SessionMode::Subagent);
        assert_eq!(
            codex_under_ag.parent_agent.as_deref(),
            Some("Antigravity US")
        );
        assert_eq!(codex_under_ag.helpers.as_slice(), [50001]);
        assert_eq!(codex_under_ag.commands.as_slice(), [50002]);

        let codex_under_claude = ownership
            .agent_sessions
            .iter()
            .find(|s| s.pid == 70002)
            .expect("nested codex under Claude");
        assert_eq!(codex_under_claude.mode, crate::model::SessionMode::Subagent);
        assert_eq!(codex_under_claude.parent_agent.as_deref(), Some("claude"));
        assert_eq!(codex_under_claude.helpers.as_slice(), [70003]);

        let oc_svc = ownership
            .agent_sessions
            .iter()
            .find(|s| s.pid == 37642)
            .expect("OpenCode service");
        assert_eq!(oc_svc.agent, crate::model::AgentKind::Opencode);
        assert_eq!(oc_svc.mode, crate::model::SessionMode::Service);
        assert_eq!(oc_svc.helpers.as_slice(), [37643]);

        // Helper identities include MCP servers of nested codex & opencode service, NOT git or sidecar app exe.
        let known_pids: BTreeSet<i32> = ownership
            .helper_identities(&snapshot)
            .iter()
            .map(|id| id.pid)
            .collect();
        assert!(known_pids.contains(&44942));
        assert!(known_pids.contains(&50001));
        assert!(known_pids.contains(&70003));
        assert!(known_pids.contains(&37643));
        assert!(
            !known_pids.contains(&50002),
            "git must not enter known_helpers"
        );
        assert!(
            known_pids.contains(&44943),
            "Antigravity sidecar_sdk must be tracked in known_helpers"
        );
        assert!(
            !known_pids.contains(&44215),
            "live main Antigravity desktop app must never enter known_helpers"
        );
    }

    #[test]
    fn opencode_pty_is_reaped_only_when_opencode_is_gone_and_pty_has_no_children() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
            ..Snapshot::default()
        };
        add_proc(
            &mut snapshot,
            35296,
            1,
            "/Users/u/.cache/opencode/bin/opencode-pty/0.6.0/opencode-pty",
            "opencode-pty daemon",
            90_000,
            12,
        );
        // No OpenCode running, no children -> orphan!
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert_eq!(ownership.orphans.as_slice(), [35296]);

        // With a live OpenCode service -> NOT an orphan.
        add_proc(
            &mut snapshot,
            37642,
            1,
            "/Users/u/Library/Application Support/ai.opencode.desktop/cli/2.4.1/opencode-cli",
            "opencode-cli serve --service",
            90_000,
            325,
        );
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(ownership.orphans.is_empty());

        // Without OpenCode service, but pty still has a child -> NOT an orphan.
        snapshot.processes.retain(|p| p.pid != 37642);
        add_proc(&mut snapshot, 35297, 35296, "/bin/zsh", "zsh", 1000, 4);
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(ownership.orphans.is_empty());
    }

    #[test]
    fn pty_subagent_codex_helpers_are_classified_and_tracked_for_orphan_reaping() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_727_806,
            user: 501,
            launchd_jobs: Some(vec![]),
            mcp_servers: Arc::from([
                ServerSignature {
                    command: "/Users/u/.local/bin/codegraph".into(),
                    args: smallvec!["serve".into(), "--mcp".into()],
                },
                ServerSignature {
                    command: "/Users/u/.config/bitrina-grafana/mcp-grafana.sh".into(),
                    args: smallvec![],
                },
                ServerSignature {
                    command: "/opt/homebrew/bin/gopls".into(),
                    args: smallvec!["mcp".into()],
                },
                ServerSignature {
                    command: "railway".into(),
                    args: smallvec!["mcp".into()],
                },
                ServerSignature {
                    command: "node".into(),
                    args: smallvec!["./mcp/server.bundle.mjs".into()],
                },
            ]),
            ..Snapshot::default()
        };
        add_proc(
            &mut snapshot,
            44215,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real",
            90_000,
            277,
        );
        add_proc(
            &mut snapshot,
            44926,
            44215,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity",
            90_000,
            18_000,
        );
        let push_tty = |snap: &mut Snapshot, pid: i32, ppid: i32, exe: &str, args: &str| {
            snap.push_process(
                pid,
                ppid,
                501,
                &format!("start-{pid}"),
                200,
                true,
                0,
                Some(32 << 20),
                exe,
                args,
            );
        };
        // Live `codex exec` running in Antigravity's PTY (`ttys001`) with all real MCP children inheriting TTY.
        push_tty(
            &mut snapshot,
            57346,
            44926,
            "/Users/u/.nvm/versions/node/v26/bin/codex",
            "codex exec -s read-only review",
        );
        push_tty(
            &mut snapshot,
            57927,
            57346,
            "/opt/homebrew/bin/mcp-grafana",
            "/opt/homebrew/bin/mcp-grafana --disable-write",
        );
        push_tty(
            &mut snapshot,
            57930,
            57346,
            "/Users/u/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient",
            "SkyComputerUseClient computer-history mcp",
        );
        push_tty(
            &mut snapshot,
            57931,
            57346,
            "/opt/homebrew/bin/gopls",
            "/opt/homebrew/bin/gopls mcp",
        );
        push_tty(
            &mut snapshot,
            57932,
            57346,
            "/Users/u/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin/node",
            "node /Users/u/.codex/plugins/cache/openai-curated-remote/codex-security/0.1.33/mcp/server.mjs --stdio",
        );
        push_tty(
            &mut snapshot,
            57933,
            57346,
            "/Applications/ChatGPT.app/Contents/Resources/cua_node/bin/node_repl",
            "node_repl --require-browser-access",
        );
        push_tty(&mut snapshot, 57943, 57346, NODE, CODEGRAPH);
        push_tty(
            &mut snapshot,
            57947,
            57346,
            "/Users/u/.nvm/versions/node/v26/bin/node",
            "node ./mcp/server.bundle.mjs",
        );
        push_tty(
            &mut snapshot,
            57948,
            57346,
            "/opt/homebrew/bin/railway",
            "railway mcp",
        );
        push_tty(
            &mut snapshot,
            59089,
            57346,
            "/Users/u/.nvm/versions/node/v26/bin/codex-code-mode-host",
            "codex-code-mode-host",
        );
        // Real commands under codex: must stay in `commands`, never in `helpers` or `known_helpers`.
        push_tty(
            &mut snapshot,
            59100,
            57346,
            "/bin/zsh",
            "zsh -lc cargo test",
        );
        push_tty(&mut snapshot, 59101, 57346, "/usr/bin/git", "git diff");
        push_tty(
            &mut snapshot,
            59102,
            57346,
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
        );

        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        let codex = ownership
            .agent_sessions
            .iter()
            .find(|s| s.pid == 57346)
            .expect("codex subagent");
        assert_eq!(codex.parent_agent.as_deref(), Some("Antigravity US"));
        assert_eq!(
            codex.helpers.as_slice(),
            [
                57927, 57930, 57931, 57932, 57933, 57943, 57947, 57948, 59089
            ]
        );
        assert_eq!(codex.commands.as_slice(), [59100, 59101, 59102]);

        let known = ownership.helper_identities(&snapshot);
        for helper_pid in [
            57927, 57930, 57931, 57932, 57933, 57943, 57947, 57948, 59089,
        ] {
            assert!(
                known.iter().any(|id| id.pid == helper_pid),
                "helper {helper_pid} must be recorded in known_helpers"
            );
        }
        for cmd_pid in [59100, 59101, 59102] {
            assert!(
                !known.iter().any(|id| id.pid == cmd_pid),
                "command {cmd_pid} must not be in known_helpers"
            );
        }

        // After `codex exec` exits and closes the PTY, a leaked non-distinctive helper
        // (`node ./mcp/server.bundle.mjs`, now `ppid == 1, has_tty == false`) is reaped!
        let mut after = snapshot.clone();
        after.processes.retain(|p| p.pid == 57947);
        after.processes[0].ppid = 1;
        after.processes[0].has_tty = false;
        let after_ownership = Ownership::new(&after, &known);
        assert_eq!(after_ownership.orphans.as_slice(), [57947]);
        assert!(!after_ownership.is_tree_protected(&after, 57947));
    }

    #[test]
    fn homebrew_python_and_playwright_chromium_are_not_protected_agent_hosts() {
        const PY_FRAMEWORK: &str = "/opt/homebrew/Cellar/python@3.14/3.14.3_1/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python";
        const PLAYWRIGHT_CHROME: &str = "/Users/u/Library/Caches/ms-playwright/chromium/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing";

        assert_eq!(super::app_bundle_path(PY_FRAMEWORK), None);
        assert!(!super::is_agent_host_exe(PY_FRAMEWORK));
        assert!(!super::is_agent_host_exe(PLAYWRIGHT_CHROME));

        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
            mcp_servers: Arc::from([ServerSignature {
                command: "uvx".into(),
                args: smallvec!["mcp-server-git".into()],
            }]),
            ..Snapshot::default()
        };
        // Claude session with Homebrew Python MCP helper, plus Playwright helper with headless Chrome child.
        add_proc(&mut snapshot, 110, 1, CLAUDE, "claude", 9000, 200);
        add_proc(
            &mut snapshot,
            111,
            110,
            PY_FRAMEWORK,
            "python3 /Users/u/.cache/uv/bin/mcp-server-git",
            9000,
            40,
        );
        add_proc(
            &mut snapshot,
            112,
            110,
            "/opt/homebrew/bin/node",
            "node /Users/u/.npm/_npx/1/node_modules/@playwright/mcp/cli.js",
            9000,
            60,
        );
        add_proc(
            &mut snapshot,
            113,
            112,
            PLAYWRIGHT_CHROME,
            "Google Chrome for Testing --headless",
            9000,
            300,
        );
        // Orphaned Homebrew Python helper known from history.
        add_proc(
            &mut snapshot,
            420,
            1,
            PY_FRAMEWORK,
            "python3 /Users/u/.cache/uv/bin/mcp-server-git",
            500,
            40,
        );
        // Arbitrarily renamed Antigravity clone: dynamically discovered AND protected!
        add_proc(
            &mut snapshot,
            800,
            1,
            "/Users/u/Applications/MyCustomIDE.app/Contents/MacOS/MyCustomIDE",
            "MyCustomIDE",
            9000,
            300,
        );
        add_proc(
            &mut snapshot,
            801,
            800,
            "/Users/u/Applications/MyCustomIDE.app/Contents/Resources/bin/language_server",
            "language_server --override_ide_name antigravity",
            9000,
            4000,
        );

        let mut known = BTreeSet::new();
        known.insert(crate::model::Identity {
            pid: 420,
            started: "start-420".into(),
        });
        let ownership = Ownership::new(&snapshot, &known);
        assert_eq!(ownership.claude_sessions[0].helpers.as_slice(), [111, 112]);
        assert!(!ownership.is_tree_protected(&snapshot, 111));
        assert!(!ownership.is_tree_protected(&snapshot, 112));
        assert_eq!(ownership.orphans.as_slice(), [420]);
        assert!(!ownership.is_tree_protected(&snapshot, 420));
        assert!(ownership.desktop_apps.iter().any(|a| a.pid == 800));
        assert!(ownership.is_tree_protected(&snapshot, 800));
    }

    #[test]
    fn orphaned_antigravity_server_and_sidecar_and_opencode_cli_child_are_handled() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
            ..Snapshot::default()
        };
        // Antigravity US exited and left `language_server.real` (`ppid == 1`, `--host_bridge_url=`)
        // plus an orphaned `sidecar_sdk` at `ppid == 1`.
        add_proc(
            &mut snapshot,
            44926,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --standalone --override_ide_name antigravity --host_bridge_url=http://127.0.0.1:51982",
            90_000,
            18_000,
        );
        add_proc(
            &mut snapshot,
            44943,
            44926,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real --import=file:///Users/u/.gemini/antigravity/lib/node_modules/sidecar_sdk/register_loader.js main.mjs",
            90_000,
            37,
        );
        // Orphaned sidecar directly at `ppid == 1` must NOT look like a running desktop app!
        add_proc(
            &mut snapshot,
            44944,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real --import=file:///Users/u/.gemini/antigravity/lib/node_modules/sidecar_sdk/register_loader.js main.mjs",
            90_000,
            37,
        );
        // OpenCode.app spawning its CLI binary from Application Support as a direct child.
        add_proc(
            &mut snapshot,
            61000,
            1,
            "/Applications/OpenCode.app/Contents/MacOS/OpenCode",
            "OpenCode",
            5000,
            300,
        );
        add_proc(
            &mut snapshot,
            61001,
            61000,
            "/Users/u/Library/Application Support/ai.opencode.desktop/cli/2.4.1-arm64/opencode-cli",
            "opencode-cli serve --port 49374",
            5000,
            250,
        );

        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(!ownership.is_bundle_running(std::path::Path::new(
            "/Users/u/Applications/Antigravity US.app"
        )));
        assert_eq!(ownership.orphans.as_slice(), [44926, 44944]);
        assert!(!ownership.is_tree_protected(&snapshot, 44926));
        assert!(!ownership.is_tree_protected(&snapshot, 44944));
        assert!(!ownership.agent_sessions.iter().any(|s| s.pid == 44926));
        let oc = ownership
            .desktop_apps
            .iter()
            .find(|a| a.pid == 61000)
            .expect("OpenCode.app discovered");
        assert_eq!(oc.runtimes.as_slice(), [61001]);
        assert!(
            oc.commands.is_empty(),
            "owned opencode-cli is not a command"
        );

        // If the orphaned `language_server.real` still has a live nested `codex`, its tree IS protected!
        let mut with_child = snapshot.clone();
        add_proc(
            &mut with_child,
            50000,
            44926,
            "/Users/u/.nvm/bin/codex",
            "codex exec review",
            100,
            200,
        );
        let ownership2 = Ownership::new(&with_child, &BTreeSet::new());
        assert!(ownership2.is_tree_protected(&with_child, 44926));
    }

    #[test]
    fn terminal_claude_cli_tracks_pool_helpers_without_signalling_tty() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![]),
            mcp_servers: Arc::from([ServerSignature {
                command: "/Users/u/.local/bin/codegraph".into(),
                args: smallvec!["serve".into(), "--mcp".into()],
            }]),
            ..Snapshot::default()
        };
        snapshot.push_process(
            150,
            1,
            501,
            "start-150",
            9000,
            true,
            0,
            Some(200 << 20),
            "/Users/u/.local/share/claude/versions/2.1.300",
            "claude",
        );
        snapshot.push_process(
            151,
            150,
            501,
            "start-151",
            9000,
            true,
            0,
            Some(64 << 20),
            NODE,
            CODEGRAPH,
        );
        snapshot.push_process(
            152,
            150,
            501,
            "start-152",
            100,
            true,
            0,
            Some(100 << 20),
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
        );
        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert_eq!(ownership.claude_sessions.len(), 1);
        assert_eq!(ownership.claude_sessions[0].helpers.as_slice(), [151]);
        // General protection (`inherited_tty = None`) still protects TTY processes;
        // inherited-TTY check allows the exact parent session TTY while rejecting a different TTY.
        let session_tty = snapshot.processes[0].tty_id;
        assert!(ownership.is_tree_protected(&snapshot, 151));
        assert!(!ownership.is_tree_protected_with_inherited_tty(&snapshot, 151, Some(session_tty)));
        assert!(ownership.is_tree_protected_with_inherited_tty(
            &snapshot,
            151,
            Some(session_tty + 1)
        ));
        let known = ownership.helper_identities(&snapshot);
        assert!(known.iter().any(|id| id.pid == 151));
        assert!(!known.iter().any(|id| id.pid == 152));
    }

    #[test]
    fn extract_antigravity_data_dir_parses_flags_and_rejects_traversal() {
        assert_eq!(
            super::extract_antigravity_data_dir(
                "language_server.real --standalone --app_data_dir antigravity --enable_sidecars"
            ),
            Some("antigravity")
        );
        assert_eq!(
            super::extract_antigravity_data_dir(
                "language_server --app_data_dir=antigravity-ide --host_bridge_url=http://127.0.0.1:1"
            ),
            Some("antigravity-ide")
        );
        assert_eq!(
            super::extract_antigravity_data_dir("language_server -app_data_dir antigravity-cli"),
            Some("antigravity-cli")
        );
        assert_eq!(
            super::extract_antigravity_data_dir("language_server --app_data_dir ../escape"),
            None
        );
        assert_eq!(
            super::extract_antigravity_data_dir("language_server --app_data_dir a/b"),
            None
        );
        assert_eq!(
            super::extract_antigravity_data_dir("language_server --app_data_dir --other"),
            None
        );
    }

    #[test]
    fn renamed_clone_sidecar_and_server_and_generic_tools_are_discovered_and_reaped_on_exit() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![1000, 2000, 3000]),
            mcp_servers: Arc::from([
                ServerSignature {
                    command: "npx".into(),
                    args: smallvec!["-y".into(), "@playwright/mcp@latest".into()],
                },
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
        // 1. Renamed Antigravity clone with custom `--app_data_dir my-clone` and sidecar.
        add_proc(
            &mut snapshot,
            900,
            1,
            "/Users/u/Applications/MyCustomIDE.app/Contents/MacOS/MyCustomIDE.real",
            "MyCustomIDE.real --proxy-server=http://127.0.0.1:18099",
            90_000,
            300,
        );
        add_proc(
            &mut snapshot,
            901,
            900,
            "/Users/u/Applications/MyCustomIDE.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity --app_data_dir my-clone --host_bridge_url=http://127.0.0.1:1",
            90_000,
            4000,
        );
        add_proc(
            &mut snapshot,
            902,
            901,
            "/Users/u/Applications/MyCustomIDE.app/Contents/MacOS/MyCustomIDE.real",
            "MyCustomIDE.real --import=file:///Users/u/.gemini/my-clone/lib/node_modules/sidecar_sdk/register_loader.js main.mjs",
            90_000,
            40,
        );

        // 2. Cursor.app (generic IDE) -> Cursor Helper (Plugin) -> npx/node Playwright MCP + nested codex.
        add_proc(
            &mut snapshot,
            1000,
            1,
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
            "Cursor",
            5000,
            600,
        );
        add_proc(
            &mut snapshot,
            1001,
            1000,
            "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin).app/Contents/MacOS/Cursor Helper (Plugin)",
            "Cursor Helper (Plugin)",
            5000,
            200,
        );
        add_proc(
            &mut snapshot,
            1002,
            1001,
            "/opt/homebrew/bin/node",
            "/opt/homebrew/bin/node /Users/u/.npm/_npx/1/node_modules/@playwright/mcp/cli.js",
            5000,
            120,
        );
        add_proc(
            &mut snapshot,
            1003,
            1001,
            "/Users/u/.nvm/bin/codex",
            "codex exec review",
            100,
            200,
        );

        // 3. Ghostty.app -> login -> zsh (TTY) -> gemini CLI -> node codegraph MCP.
        add_proc(
            &mut snapshot,
            2000,
            1,
            "/Applications/Ghostty.app/Contents/MacOS/ghostty",
            "ghostty",
            9000,
            100,
        );
        snapshot.push_process_with_tty(
            2001,
            2000,
            501,
            "start-2001",
            9000,
            0x8001,
            0,
            Some(4 << 20),
            "/bin/zsh",
            "-zsh",
        );
        snapshot.push_process_with_tty(
            2002,
            2001,
            501,
            "start-2002",
            5000,
            0x8001,
            0,
            Some(80 << 20),
            "/opt/homebrew/bin/gemini",
            "gemini",
        );
        snapshot.push_process_with_tty(
            2003,
            2002,
            501,
            "start-2003",
            5000,
            0x8001,
            0,
            Some(64 << 20),
            NODE,
            CODEGRAPH,
        );
        // Direct interactive `railway mcp` typed in zsh must NOT turn zsh/Ghostty into an agent.
        snapshot.push_process_with_tty(
            2004,
            2001,
            501,
            "start-2004",
            5000,
            0x8001,
            0,
            Some(8 << 20),
            "/opt/homebrew/bin/railway",
            "railway mcp",
        );

        let ownership = Ownership::new(&snapshot, &BTreeSet::new());

        let custom_ag = ownership
            .desktop_apps
            .iter()
            .find(|a| a.pid == 900)
            .expect("renamed Antigravity clone discovered");
        assert_eq!(custom_ag.agent, crate::model::AgentKind::Antigravity);
        assert_eq!(custom_ag.data_dir.as_deref(), Some("my-clone"));
        assert_eq!(custom_ag.pools.as_slice(), [902]);

        let cursor = ownership
            .desktop_apps
            .iter()
            .find(|a| a.pid == 1000)
            .expect("Cursor.app discovered dynamically via MCP pool");
        assert_eq!(cursor.agent, crate::model::AgentKind::Other);
        assert_eq!(cursor.app_name, "Cursor");
        assert_eq!(cursor.pools.as_slice(), [1002]);
        assert_eq!(
            ownership.desktop_app_footprint(&snapshot, cursor),
            120 << 20
        );
        assert!(ownership.is_tree_protected(&snapshot, 1000));

        // Ghostty must NOT be misclassified as an agent desktop app!
        assert!(!ownership.desktop_apps.iter().any(|a| a.pid == 2000));

        // `codex` under Cursor is backfilled as subagent under Cursor.
        let codex_under_cursor = ownership
            .agent_sessions
            .iter()
            .find(|s| s.pid == 1003)
            .expect("codex under Cursor");
        assert_eq!(codex_under_cursor.mode, crate::model::SessionMode::Subagent);
        assert_eq!(codex_under_cursor.parent_agent.as_deref(), Some("Cursor"));

        // `gemini` CLI is discovered as `AgentKind::Other` CLI with its codegraph helper.
        let gemini = ownership
            .agent_sessions
            .iter()
            .find(|s| s.pid == 2002)
            .expect("gemini CLI discovered dynamically");
        assert_eq!(gemini.agent, crate::model::AgentKind::Other);
        assert_eq!(gemini.mode, crate::model::SessionMode::Cli);
        assert_eq!(gemini.name.as_deref(), Some("gemini"));
        assert_eq!(gemini.helpers.as_slice(), [2003]);
        assert!(!ownership.agent_sessions.iter().any(|s| s.pid == 2001));

        // `known_helpers` records sidecar 902, Cursor's generic node/npx MCP 1002, and gemini's 2003,
        // while excluding main apps 900/1000 and Cursor Helper 1001!
        let known = ownership.helper_identities(&snapshot);
        assert!(known.iter().any(|id| id.pid == 902));
        assert!(known.iter().any(|id| id.pid == 1002));
        assert!(known.iter().any(|id| id.pid == 2003));
        assert!(!known.iter().any(|id| id.pid == 900));
        assert!(!known.iter().any(|id| id.pid == 1000));
        assert!(!known.iter().any(|id| id.pid == 1001));

        // After MyCustomIDE and Cursor exit, leaving 901 (language_server), 902 (sidecar_sdk),
        // and 1002 (generic npx/node Playwright MCP) at `ppid == 1`, all three are reaped!
        let mut after = snapshot.clone();
        after
            .processes
            .retain(|p| ![900, 1000, 1001, 1003, 2000, 2001, 2002, 2003, 2004].contains(&p.pid));
        for p in &mut after.processes {
            p.ppid = 1;
        }
        let after_ownership = Ownership::new(&after, &known);
        assert_eq!(after_ownership.orphans.as_slice(), [901, 902, 1002]);
        assert!(!after_ownership.is_tree_protected(&after, 901));
        assert!(!after_ownership.is_tree_protected(&after, 902));
        assert!(!after_ownership.is_tree_protected(&after, 1002));
    }

    #[test]
    fn generic_discovery_ignores_known_agent_command_subtrees_and_backfills_other_cli_subagents() {
        let mut snapshot = Snapshot {
            taken_at: 1_795_725_000,
            user: 501,
            launchd_jobs: Some(vec![100, 900, 2000]),
            mcp_servers: Arc::from([
                ServerSignature {
                    command: "railway".into(),
                    args: smallvec!["mcp".into()],
                },
                ServerSignature {
                    command: "/Users/u/.local/bin/codegraph".into(),
                    args: smallvec!["serve".into(), "--mcp".into()],
                },
            ]),
            ..Snapshot::default()
        };
        // 1. Claude Desktop non-TTY Bash tool: `claude -> zsh (non-TTY) -> cargo -> railway mcp`.
        //    Must NOT create a bogus `cargo` Other CLI agent or put `railway mcp` in known_helpers!
        add_proc(&mut snapshot, 110, 1, CLAUDE, "claude", 9000, 200);
        add_proc(
            &mut snapshot,
            113,
            110,
            "/bin/zsh",
            "/bin/zsh -c cargo test",
            60,
            4,
        );
        add_proc(
            &mut snapshot,
            114,
            113,
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
            60,
            100,
        );
        add_proc(
            &mut snapshot,
            115,
            114,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            60,
            8,
        );
        // 2. Antigravity US active command subtree: `Antigravity US -> zsh (non-TTY) -> cargo -> railway mcp`.
        //    Must NOT steal 912 into `Antigravity US.pools` or `known_helpers`!
        add_proc(
            &mut snapshot,
            900,
            1,
            "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
            "Antigravity.real",
            90_000,
            300,
        );
        add_proc(
            &mut snapshot,
            901,
            900,
            "/Users/u/Applications/Antigravity US.app/Contents/Resources/bin/language_server.real",
            "language_server.real --override_ide_name antigravity",
            90_000,
            4000,
        );
        add_proc(
            &mut snapshot,
            910,
            900,
            "/bin/zsh",
            "/bin/zsh -c cargo test",
            60,
            4,
        );
        add_proc(
            &mut snapshot,
            911,
            910,
            "/Users/u/.cargo/bin/cargo",
            "cargo test",
            60,
            100,
        );
        add_proc(
            &mut snapshot,
            912,
            911,
            "/opt/homebrew/bin/railway",
            "railway mcp",
            60,
            8,
        );
        // 3. `Ghostty -> zsh (TTY) -> git -> railway mcp` must NOT classify `git` as an agent.
        add_proc(
            &mut snapshot,
            2000,
            1,
            "/Applications/Ghostty.app/Contents/MacOS/ghostty",
            "ghostty",
            9000,
            100,
        );
        snapshot.push_process_with_tty(
            2001,
            2000,
            501,
            "start-2001",
            9000,
            0x8001,
            0,
            Some(4 << 20),
            "/bin/zsh",
            "-zsh",
        );
        snapshot.push_process_with_tty(
            2002,
            2001,
            501,
            "start-2002",
            60,
            0x8001,
            0,
            Some(10 << 20),
            "/usr/bin/git",
            "git commit",
        );
        snapshot.push_process_with_tty(
            2003,
            2002,
            501,
            "start-2003",
            60,
            0x8001,
            0,
            Some(8 << 20),
            "/opt/homebrew/bin/railway",
            "railway mcp",
        );
        // 4. `Ghostty -> zsh (TTY) -> gemini -> {node codegraph, codex exec}`:
        //    `codex` is backfilled as a subagent under `gemini`!
        snapshot.push_process_with_tty(
            2010,
            2001,
            501,
            "start-2010",
            5000,
            0x8001,
            0,
            Some(80 << 20),
            "/opt/homebrew/bin/gemini",
            "gemini",
        );
        snapshot.push_process_with_tty(
            2011,
            2010,
            501,
            "start-2011",
            5000,
            0x8001,
            0,
            Some(64 << 20),
            NODE,
            CODEGRAPH,
        );
        snapshot.push_process_with_tty(
            2012,
            2010,
            501,
            "start-2012",
            100,
            0x8001,
            0,
            Some(120 << 20),
            "/Users/u/.nvm/bin/codex",
            "codex exec review",
        );
        // 5. Space-separated `--host_bridge_url http://...` on an orphaned `language_server.bin`.
        add_proc(
            &mut snapshot,
            3001,
            1,
            "/Users/u/Applications/CloneX.app/Contents/Resources/bin/language_server.bin",
            "language_server.bin --override_ide_name antigravity --host_bridge_url http://127.0.0.1:51982",
            900,
            2048,
        );

        let ownership = Ownership::new(&snapshot, &BTreeSet::new());
        assert!(
            !ownership.agent_sessions.iter().any(|s| s.pid == 114),
            "cargo under Claude Bash tool must not be an Other agent: {:?}",
            ownership.agent_sessions
        );
        assert!(
            !ownership.agent_sessions.iter().any(|s| s.pid == 2002),
            "git hook must not be an Other agent: {:?}",
            ownership.agent_sessions
        );
        let ag = ownership
            .desktop_apps
            .iter()
            .find(|a| a.pid == 900)
            .unwrap();
        assert!(
            !ag.pools.contains(&912),
            "command descendant must not enter Antigravity pools: {:?}",
            ag.pools
        );
        let known = ownership.helper_identities(&snapshot);
        assert!(!known.iter().any(|id| id.pid == 115));
        assert!(!known.iter().any(|id| id.pid == 912));
        assert!(!known.iter().any(|id| id.pid == 2003));
        assert!(known.iter().any(|id| id.pid == 2011));

        let codex_sub = ownership
            .agent_sessions
            .iter()
            .find(|s| s.pid == 2012)
            .expect("codex under gemini");
        assert_eq!(codex_sub.mode, crate::model::SessionMode::Subagent);
        assert_eq!(codex_sub.parent_agent.as_deref(), Some("gemini"));
        assert_eq!(ownership.orphans.as_slice(), [3001]);
    }
}

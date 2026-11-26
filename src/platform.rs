//! The operating system behind [`System`]. Only macOS is supported.

use std::path::PathBuf;

use crate::{actions::System, error::AppError};

pub fn home() -> Result<PathBuf, AppError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or(AppError::NoHome)
}

#[cfg(target_os = "macos")]
pub fn check_available() -> Result<(), AppError> {
    // As root, `uid` checks would admit other users' and system processes.
    if nix::unistd::geteuid().is_root() {
        return Err(AppError::Root);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn check_available() -> Result<(), AppError> {
    Err(AppError::Unsupported)
}

#[cfg(target_os = "macos")]
pub fn system(home: PathBuf) -> Result<impl System, AppError> {
    check_available()?;
    Ok(macos::MacOs::new(home))
}

#[cfg(not(target_os = "macos"))]
pub fn system(_home: PathBuf) -> Result<NoSystem, AppError> {
    Err(AppError::Unsupported)
}

/// Placeholder that never exists at run time on unsupported platforms.
#[cfg(not(target_os = "macos"))]
pub enum NoSystem {}

#[cfg(not(target_os = "macos"))]
impl System for NoSystem {
    fn snapshot(&self) -> Result<crate::model::Snapshot, AppError> {
        match *self {}
    }
    fn identify(&self, _: i32) -> Option<(compact_str::CompactString, u32)> {
        match *self {}
    }
    fn signal(&self, _: i32, _: crate::actions::Signal) -> std::io::Result<()> {
        match *self {}
    }
    fn launch_app(&self, _: &std::path::Path) -> std::io::Result<()> {
        match *self {}
    }
    fn sleep(&self, _: std::time::Duration) {
        match *self {}
    }
    fn now(&self) -> u64 {
        match *self {}
    }
}

#[cfg(target_os = "macos")]
pub mod macos {
    use std::{
        cell::RefCell,
        collections::{BTreeMap, BTreeSet},
        fmt::Write as _,
        fs::{self, File},
        io::{self, Read, Write as _},
        os::fd::AsFd,
        path::{Path, PathBuf},
        process::{Child, ChildStdout, Command, ExitStatus, Stdio},
        sync::Arc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use compact_str::CompactString;
    use nix::{
        errno::Errno,
        poll::{PollFd, PollFlags, PollTimeout, poll},
        sys::signal::{Signal as NixSignal, kill},
        unistd::{Pid, getuid},
    };

    use crate::{
        actions::{Signal, System},
        error::AppError,
        model::{Identity, Process, ServerSignature, Snapshot, StrSpan},
        owners, probe,
        store::FileStamp,
    };

    /// A system command that has not finished by then is stuck; under heavy
    /// swapping `top` takes seconds, not tens of seconds.
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

    /// Above this many candidate PIDs, a full `ps -axww -o pid=,args=` avoids
    /// overly long `-p` argument lists.
    const TARGETED_PS_MAX_PIDS: usize = 1024;

    /// Periodically refresh cached non-MCP `ppid == 1` entries (~1 hour at 60 s
    /// intervals) to bound staleness even if a detached process re-execs the
    /// exact same binary with different arguments.
    const NON_MCP_MAX_PASSES: u32 = 60;

    /// Stack-bitset capacity for zero-allocation pruning of cached `ppid == 1`
    /// entries. Above this count the cache falls back to re-seeding cleanly.
    const SEEN_BITSET_WORDS: usize = 8;
    const SEEN_BITSET_BITS: usize = SEEN_BITSET_WORDS * 64;

    #[derive(Default)]
    struct ConfigStamp {
        initialized: bool,
        stamp: Option<FileStamp>,
    }

    impl ConfigStamp {
        fn is_fresh(&self, current: Option<FileStamp>) -> bool {
            self.initialized && self.stamp == current
        }

        fn load_into(
            &mut self,
            path: &Path,
            current: Option<FileStamp>,
            parse: fn(File) -> Vec<ServerSignature>,
            out: &mut Vec<ServerSignature>,
        ) {
            if current.is_none() {
                self.initialized = true;
                self.stamp = None;
                return;
            }
            match File::open(path) {
                Ok(file) => {
                    let opened_stamp = file
                        .metadata()
                        .ok()
                        .map(|meta| FileStamp::from_metadata(&meta));
                    out.extend(parse(file));
                    self.stamp = opened_stamp;
                    self.initialized = opened_stamp.is_some();
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.initialized = true;
                    self.stamp = None;
                }
                Err(_) => {
                    // Transient open failure (e.g. EACCES/EMFILE): do not poison
                    // the cache so the next pass retries.
                    self.initialized = false;
                    self.stamp = None;
                }
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ConfigKind {
        CodexToml,
        McpJson,
        OpencodeJson,
    }

    struct ExtraConfigEntry {
        path: PathBuf,
        kind: ConfigKind,
        stamp: ConfigStamp,
        servers: Vec<ServerSignature>,
    }

    impl ExtraConfigEntry {
        fn new(path: PathBuf, kind: ConfigKind) -> Self {
            Self {
                path,
                kind,
                stamp: ConfigStamp::default(),
                servers: Vec::new(),
            }
        }

        fn refresh(&mut self) -> bool {
            let current = FileStamp::of(&self.path);
            if self.stamp.is_fresh(current) {
                return false;
            }
            self.servers.clear();
            let parse: fn(File) -> Vec<ServerSignature> = match self.kind {
                ConfigKind::CodexToml => probe::codex_servers_from_reader,
                ConfigKind::McpJson => probe::mcp_json_servers_from_reader,
                ConfigKind::OpencodeJson => probe::opencode_servers_from_reader,
            };
            self.stamp
                .load_into(&self.path, current, parse, &mut self.servers);
            true
        }
    }

    struct McpCache {
        codex: ConfigStamp,
        codex_servers: Vec<ServerSignature>,
        codex_projects: Vec<String>,
        codex_plugins_dir: ConfigStamp,
        codex_plugin_configs: Vec<PathBuf>,
        claude: ConfigStamp,
        claude_servers: Vec<ServerSignature>,
        claude_projects: Vec<String>,
        claude_plugins: ConfigStamp,
        plugin_dirs: Vec<String>,
        gemini_dir: ConfigStamp,
        gemini_data_dir_configs: Vec<PathBuf>,
        gemini_plugins_dir: ConfigStamp,
        gemini_plugin_subdirs: Vec<(PathBuf, Option<FileStamp>)>,
        gemini_plugin_configs: Vec<(PathBuf, ConfigKind)>,
        global_extras: Vec<ExtraConfigEntry>,
        dynamic_extras: Vec<ExtraConfigEntry>,
        combined: Arc<[ServerSignature]>,
        has_distinctive: bool,
        has_pool_signatures: bool,
        generation: u64,
    }

    struct GeminiPluginDiscovery {
        configs: Vec<(PathBuf, ConfigKind)>,
        subdirs: Vec<(PathBuf, Option<FileStamp>)>,
    }

    /// Discover MCP config files inside `~/.gemini/config/plugins/<plugin>/`
    /// (`.mcp.json`, `mcp.json`, `mcp_config.json`, or `plugin.json` defining
    /// `mcpServers`/`mcp`), returning both the discovered config files and
    /// per-plugin directory stamps so adding a config inside an existing plugin
    /// directory invalidates the cache on the next pass.
    fn discover_gemini_plugin_mcp_configs(root: &Path, limit: usize) -> GeminiPluginDiscovery {
        let mut configs = Vec::new();
        let mut subdirs = Vec::new();
        let Ok(plugins) = fs::read_dir(root) else {
            return GeminiPluginDiscovery { configs, subdirs };
        };
        for plugin in plugins.filter_map(Result::ok) {
            if subdirs.len() >= limit {
                break;
            }
            if !plugin.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let dir_path = plugin.path();
            let dir_stamp = FileStamp::of(&dir_path);
            subdirs.push((dir_path.clone(), dir_stamp));
            for (name, kind) in [
                (".mcp.json", ConfigKind::McpJson),
                ("mcp.json", ConfigKind::McpJson),
                ("mcp_config.json", ConfigKind::McpJson),
                ("plugin.json", ConfigKind::OpencodeJson),
            ] {
                if configs.len() >= limit {
                    break;
                }
                let cfg = dir_path.join(name);
                if cfg.is_file() {
                    configs.push((cfg, kind));
                }
            }
        }
        configs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        configs.dedup_by(|a, b| a.0 == b.0);
        subdirs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        GeminiPluginDiscovery { configs, subdirs }
    }

    /// Discover custom `~/.gemini/<app_data_dir>/mcp_config.json` files beyond
    /// the standard `antigravity`, `antigravity-ide`, `antigravity-cli`, and
    /// `config` directories, bounded at `limit` files.
    fn discover_gemini_data_dir_mcp_configs(root: &Path, limit: usize) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = fs::read_dir(root) else {
            return found;
        };
        for entry in entries.filter_map(Result::ok) {
            if found.len() >= limit {
                break;
            }
            if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some("antigravity" | "antigravity-ide" | "antigravity-cli" | "config")
            ) {
                continue;
            }
            let cfg = entry.path().join("mcp_config.json");
            if cfg.is_file() {
                found.push(cfg);
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    /// Discover `.mcp.json` files inside `~/.codex/plugins/cache/<marketplace>/<plugin>/<version>/.mcp.json`
    /// (also checking 2-level `<marketplace>/<plugin>/.mcp.json`), bounded at `limit` files.
    fn discover_codex_plugin_mcp_configs(root: &Path, limit: usize) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(marketplaces) = fs::read_dir(root) else {
            return found;
        };
        for mp in marketplaces.filter_map(Result::ok) {
            if found.len() >= limit {
                break;
            }
            if !mp.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let Ok(plugins) = fs::read_dir(mp.path()) else {
                continue;
            };
            for plugin in plugins.filter_map(Result::ok) {
                if found.len() >= limit {
                    break;
                }
                if !plugin.file_type().is_ok_and(|ft| ft.is_dir()) {
                    continue;
                }
                let direct = plugin.path().join(".mcp.json");
                if direct.is_file() {
                    found.push(direct);
                    if found.len() >= limit {
                        break;
                    }
                }
                let Ok(versions) = fs::read_dir(plugin.path()) else {
                    continue;
                };
                for ver in versions.filter_map(Result::ok) {
                    if found.len() >= limit {
                        break;
                    }
                    if !ver.file_type().is_ok_and(|ft| ft.is_dir()) {
                        continue;
                    }
                    let cfg = ver.path().join(".mcp.json");
                    if cfg.is_file() {
                        found.push(cfg);
                    }
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    impl McpCache {
        fn new(home: &Path) -> Self {
            let global_extras = vec![
                ExtraConfigEntry::new(
                    home.join("Library/Application Support/Claude/claude_desktop_config.json"),
                    ConfigKind::McpJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".gemini/antigravity/mcp_config.json"),
                    ConfigKind::McpJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".gemini/antigravity-ide/mcp_config.json"),
                    ConfigKind::McpJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".gemini/antigravity-cli/mcp_config.json"),
                    ConfigKind::McpJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".gemini/antigravity-cli/settings.json"),
                    ConfigKind::OpencodeJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".gemini/config/mcp_config.json"),
                    ConfigKind::McpJson,
                ),
                ExtraConfigEntry::new(home.join(".gemini/settings.json"), ConfigKind::OpencodeJson),
                ExtraConfigEntry::new(home.join(".cursor/mcp.json"), ConfigKind::McpJson),
                ExtraConfigEntry::new(
                    home.join(".codeium/windsurf/mcp_config.json"),
                    ConfigKind::McpJson,
                ),
                ExtraConfigEntry::new(
                    home.join("Library/Application Support/Code/User/mcp.json"),
                    ConfigKind::McpJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".config/zed/settings.json"),
                    ConfigKind::OpencodeJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".config/opencode/opencode.json"),
                    ConfigKind::OpencodeJson,
                ),
                ExtraConfigEntry::new(
                    home.join(".config/opencode/opencode.jsonc"),
                    ConfigKind::OpencodeJson,
                ),
            ];
            Self {
                codex: ConfigStamp::default(),
                codex_servers: Vec::new(),
                codex_projects: Vec::new(),
                codex_plugins_dir: ConfigStamp::default(),
                codex_plugin_configs: Vec::new(),
                claude: ConfigStamp::default(),
                claude_servers: Vec::new(),
                claude_projects: Vec::new(),
                claude_plugins: ConfigStamp::default(),
                plugin_dirs: Vec::new(),
                gemini_dir: ConfigStamp::default(),
                gemini_data_dir_configs: Vec::new(),
                gemini_plugins_dir: ConfigStamp::default(),
                gemini_plugin_subdirs: Vec::new(),
                gemini_plugin_configs: Vec::new(),
                global_extras,
                dynamic_extras: Vec::new(),
                combined: Arc::from([]),
                has_distinctive: false,
                has_pool_signatures: false,
                generation: 0,
            }
        }

        fn rebuild_dynamic_targets(&mut self, codex_config: &Path) {
            const MAX_DYNAMIC_PROJECTS: usize = 128;
            const MAX_DYNAMIC_PLUGINS: usize = 64;

            let mut project_dirs: Vec<&str> = Vec::new();
            for dir in self
                .claude_projects
                .iter()
                .chain(self.codex_projects.iter())
            {
                if project_dirs.len() >= MAX_DYNAMIC_PROJECTS {
                    break;
                }
                if !project_dirs.contains(&dir.as_str()) {
                    project_dirs.push(dir.as_str());
                }
            }

            let mut desired: Vec<(PathBuf, ConfigKind)> = Vec::new();
            for dir in project_dirs {
                let root = Path::new(dir);
                desired.push((root.join(".mcp.json"), ConfigKind::McpJson));
                let proj_codex = root.join(".codex/config.toml");
                if proj_codex != codex_config {
                    desired.push((proj_codex, ConfigKind::CodexToml));
                }
                desired.push((root.join("opencode.json"), ConfigKind::OpencodeJson));
                desired.push((root.join("opencode.jsonc"), ConfigKind::OpencodeJson));
            }
            for dir in self.plugin_dirs.iter().take(MAX_DYNAMIC_PLUGINS) {
                let root = Path::new(dir);
                desired.push((root.join(".mcp.json"), ConfigKind::McpJson));
            }
            for cfg in self.codex_plugin_configs.iter().take(MAX_DYNAMIC_PLUGINS) {
                desired.push((cfg.clone(), ConfigKind::McpJson));
            }
            for cfg in self
                .gemini_data_dir_configs
                .iter()
                .take(MAX_DYNAMIC_PLUGINS)
            {
                desired.push((cfg.clone(), ConfigKind::McpJson));
            }
            for (cfg, kind) in self.gemini_plugin_configs.iter().take(MAX_DYNAMIC_PLUGINS) {
                desired.push((cfg.clone(), *kind));
            }

            let old = std::mem::take(&mut self.dynamic_extras);
            let mut next = Vec::with_capacity(desired.len());
            for (path, kind) in desired {
                if let Some(existing) = old.iter().position(|e| e.path == path) {
                    // Reuse existing stamp & parsed servers when unchanged.
                    let entry = &old[existing];
                    next.push(ExtraConfigEntry {
                        path,
                        kind,
                        stamp: ConfigStamp {
                            initialized: entry.stamp.initialized,
                            stamp: entry.stamp.stamp,
                        },
                        servers: entry.servers.clone(),
                    });
                } else {
                    next.push(ExtraConfigEntry::new(path, kind));
                }
            }
            self.dynamic_extras = next;
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct NonMcpEntry {
        pid: i32,
        started_key: u32,
        exe_hash: u64,
    }

    #[derive(Default)]
    struct WatchCache {
        /// Live `ppid == 1` launchd-managed processes: `(pid, packed_lstart)`,
        /// sorted by `pid`.
        launchd: Vec<(i32, u32)>,
        launchd_seeded: bool,
        /// Live non-launchd `ppid == 1` processes already verified not to match
        /// any distinctive MCP server at `non_mcp_generation`, sorted by `pid`.
        non_mcp: Vec<NonMcpEntry>,
        non_mcp_generation: u64,
        non_mcp_passes: u32,
        /// Recent kept process count and string arena size to pre-size
        /// `Snapshot` buffers without stepping through multiple `MALLOC_SMALL`
        /// size classes.
        last_process_count: usize,
        last_strings_len: usize,
    }

    impl WatchCache {
        fn mark_seen(bits: &mut [u64; SEEN_BITSET_WORDS], index: usize) {
            if index < SEEN_BITSET_BITS {
                bits[index / 64] |= 1u64 << (index % 64);
            }
        }

        fn is_seen(bits: &[u64; SEEN_BITSET_WORDS], index: usize) -> bool {
            index < SEEN_BITSET_BITS && (bits[index / 64] & (1u64 << (index % 64))) != 0
        }

        fn prune_launchd(&mut self, seen: &[u64; SEEN_BITSET_WORDS]) {
            if self.launchd.len() > SEEN_BITSET_BITS {
                self.launchd.clear();
                self.launchd_seeded = false;
                return;
            }
            let mut idx = 0usize;
            self.launchd.retain(|_| {
                let keep = Self::is_seen(seen, idx);
                idx += 1;
                keep
            });
        }

        fn prune_non_mcp(&mut self, seen: &[u64; SEEN_BITSET_WORDS]) {
            if self.non_mcp.len() > SEEN_BITSET_BITS {
                self.non_mcp.clear();
                self.non_mcp_passes = 0;
                return;
            }
            let mut idx = 0usize;
            self.non_mcp.retain(|_| {
                let keep = Self::is_seen(seen, idx);
                idx += 1;
                keep
            });
        }

        fn insert_launchd_batch(&mut self, mut entries: Vec<(i32, u32)>) {
            if entries.is_empty() {
                return;
            }
            if self.launchd.is_empty() {
                entries.sort_unstable_by_key(|&(pid, _)| pid);
                entries.dedup_by_key(|entry| entry.0);
                if entries.len() > SEEN_BITSET_BITS {
                    entries.truncate(SEEN_BITSET_BITS);
                }
                self.launchd = entries;
                return;
            }
            self.launchd.extend_from_slice(&entries);
            self.launchd.sort_unstable_by_key(|&(pid, _)| pid);
            self.launchd.dedup_by_key(|entry| entry.0);
            if self.launchd.len() > SEEN_BITSET_BITS {
                self.launchd.truncate(SEEN_BITSET_BITS);
            }
        }

        fn insert_non_mcp_batch(&mut self, mut entries: Vec<NonMcpEntry>) {
            if entries.is_empty() {
                return;
            }
            if self.non_mcp.is_empty() {
                entries.sort_unstable_by_key(|entry| entry.pid);
                entries.dedup_by_key(|entry| entry.pid);
                if entries.len() > SEEN_BITSET_BITS {
                    entries.truncate(SEEN_BITSET_BITS);
                }
                self.non_mcp = entries;
                return;
            }
            self.non_mcp.extend_from_slice(&entries);
            self.non_mcp.sort_unstable_by_key(|entry| entry.pid);
            self.non_mcp.dedup_by_key(|entry| entry.pid);
            if self.non_mcp.len() > SEEN_BITSET_BITS {
                self.non_mcp.truncate(SEEN_BITSET_BITS);
            }
        }
    }

    type OpencodeActivityCache = (Option<FileStamp>, Option<FileStamp>, Option<u64>);

    pub struct MacOs {
        codex_config: PathBuf,
        codex_plugins_cache: PathBuf,
        claude_config: PathBuf,
        claude_plugins: PathBuf,
        gemini_plugins_dir: PathBuf,
        codex_sessions: PathBuf,
        gemini_dir: PathBuf,
        opencode_data: PathBuf,
        opencode_state: PathBuf,
        mcp_cache: RefCell<McpCache>,
        watch_cache: RefCell<WatchCache>,
        last_rollout: RefCell<Option<PathBuf>>,
        last_antigravity_files: RefCell<BTreeMap<CompactString, PathBuf>>,
        opencode_activity_cache: RefCell<Option<OpencodeActivityCache>>,
    }

    impl MacOs {
        pub fn new(home: PathBuf) -> Self {
            let mcp_cache = McpCache::new(&home);
            Self {
                codex_config: home.join(".codex/config.toml"),
                codex_plugins_cache: home.join(".codex/plugins/cache"),
                claude_config: home.join(".claude.json"),
                claude_plugins: home.join(".claude/plugins/installed_plugins.json"),
                gemini_plugins_dir: home.join(".gemini/config/plugins"),
                codex_sessions: home.join(".codex/sessions"),
                gemini_dir: home.join(".gemini"),
                opencode_data: home.join(".local/share/opencode"),
                opencode_state: home.join(".local/state/opencode"),
                mcp_cache: RefCell::new(mcp_cache),
                watch_cache: RefCell::new(WatchCache::default()),
                last_rollout: RefCell::new(None),
                last_antigravity_files: RefCell::new(BTreeMap::new()),
                opencode_activity_cache: RefCell::new(None),
            }
        }

        fn load_mcp_servers(&self) -> (Arc<[ServerSignature]>, bool, bool, u64) {
            let mut cache = self.mcp_cache.borrow_mut();
            let mut any_changed = false;
            let mut index_changed = false;

            let codex_stamp = FileStamp::of(&self.codex_config);
            if !cache.codex.is_fresh(codex_stamp) {
                any_changed = true;
                index_changed = true;
                cache.codex_servers.clear();
                cache.codex_projects.clear();
                if codex_stamp.is_none() {
                    cache.codex.initialized = true;
                    cache.codex.stamp = None;
                } else {
                    match File::open(&self.codex_config) {
                        Ok(file) => {
                            let opened_stamp = file
                                .metadata()
                                .ok()
                                .map(|meta| FileStamp::from_metadata(&meta));
                            let (servers, projects) = probe::codex_config_from_reader(file);
                            cache.codex_servers = servers;
                            cache.codex_projects = projects;
                            cache.codex.stamp = opened_stamp;
                            cache.codex.initialized = opened_stamp.is_some();
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            cache.codex.initialized = true;
                            cache.codex.stamp = None;
                        }
                        Err(_) => {
                            cache.codex.initialized = false;
                            cache.codex.stamp = None;
                        }
                    }
                }
            }

            let codex_plugins_stamp = FileStamp::of(&self.codex_plugins_cache);
            if !cache.codex_plugins_dir.is_fresh(codex_plugins_stamp) || index_changed {
                let next_configs = if codex_plugins_stamp.is_some() {
                    discover_codex_plugin_mcp_configs(&self.codex_plugins_cache, 64)
                } else {
                    Vec::new()
                };
                if next_configs != cache.codex_plugin_configs {
                    cache.codex_plugin_configs = next_configs;
                    any_changed = true;
                    index_changed = true;
                }
                cache.codex_plugins_dir.stamp = codex_plugins_stamp;
                cache.codex_plugins_dir.initialized = true;
            }

            let claude_stamp = FileStamp::of(&self.claude_config);
            if !cache.claude.is_fresh(claude_stamp) {
                any_changed = true;
                index_changed = true;
                cache.claude_servers.clear();
                cache.claude_projects.clear();
                if claude_stamp.is_none() {
                    cache.claude.initialized = true;
                    cache.claude.stamp = None;
                } else {
                    match File::open(&self.claude_config) {
                        Ok(file) => {
                            let opened_stamp = file
                                .metadata()
                                .ok()
                                .map(|meta| FileStamp::from_metadata(&meta));
                            let (servers, projects) = probe::claude_config_from_reader(file);
                            cache.claude_servers = servers;
                            cache.claude_projects = projects;
                            cache.claude.stamp = opened_stamp;
                            cache.claude.initialized = opened_stamp.is_some();
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            cache.claude.initialized = true;
                            cache.claude.stamp = None;
                        }
                        Err(_) => {
                            cache.claude.initialized = false;
                            cache.claude.stamp = None;
                        }
                    }
                }
            }

            let plugins_stamp = FileStamp::of(&self.claude_plugins);
            if !cache.claude_plugins.is_fresh(plugins_stamp) {
                any_changed = true;
                index_changed = true;
                cache.plugin_dirs.clear();
                if plugins_stamp.is_none() {
                    cache.claude_plugins.initialized = true;
                    cache.claude_plugins.stamp = None;
                } else {
                    match File::open(&self.claude_plugins) {
                        Ok(file) => {
                            let opened_stamp = file
                                .metadata()
                                .ok()
                                .map(|meta| FileStamp::from_metadata(&meta));
                            cache.plugin_dirs =
                                probe::claude_installed_plugin_paths_from_reader(file);
                            cache.claude_plugins.stamp = opened_stamp;
                            cache.claude_plugins.initialized = opened_stamp.is_some();
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            cache.claude_plugins.initialized = true;
                            cache.claude_plugins.stamp = None;
                        }
                        Err(_) => {
                            cache.claude_plugins.initialized = false;
                            cache.claude_plugins.stamp = None;
                        }
                    }
                }
            }

            let gemini_dir_stamp = FileStamp::of(&self.gemini_dir);
            if !cache.gemini_dir.is_fresh(gemini_dir_stamp) || index_changed {
                let next_configs = if gemini_dir_stamp.is_some() {
                    discover_gemini_data_dir_mcp_configs(&self.gemini_dir, 32)
                } else {
                    Vec::new()
                };
                if next_configs != cache.gemini_data_dir_configs {
                    cache.gemini_data_dir_configs = next_configs;
                    any_changed = true;
                    index_changed = true;
                }
                cache.gemini_dir.stamp = gemini_dir_stamp;
                cache.gemini_dir.initialized = true;
            }

            let gemini_plugins_stamp = FileStamp::of(&self.gemini_plugins_dir);
            let gemini_plugin_subdir_changed = cache
                .gemini_plugin_subdirs
                .iter()
                .any(|(dir, stamp)| FileStamp::of(dir) != *stamp);
            if !cache.gemini_plugins_dir.is_fresh(gemini_plugins_stamp)
                || gemini_plugin_subdir_changed
                || index_changed
            {
                let discovered = if gemini_plugins_stamp.is_some() {
                    discover_gemini_plugin_mcp_configs(&self.gemini_plugins_dir, 64)
                } else {
                    GeminiPluginDiscovery {
                        configs: Vec::new(),
                        subdirs: Vec::new(),
                    }
                };
                if discovered.configs != cache.gemini_plugin_configs {
                    cache.gemini_plugin_configs = discovered.configs;
                    any_changed = true;
                    index_changed = true;
                }
                cache.gemini_plugin_subdirs = discovered.subdirs;
                cache.gemini_plugins_dir.stamp = gemini_plugins_stamp;
                cache.gemini_plugins_dir.initialized = true;
            }

            if index_changed {
                cache.rebuild_dynamic_targets(&self.codex_config);
            }

            for entry in &mut cache.global_extras {
                if entry.refresh() {
                    any_changed = true;
                }
            }
            for entry in &mut cache.dynamic_extras {
                if entry.refresh() {
                    any_changed = true;
                }
            }

            if any_changed {
                let mut combined = Vec::new();
                combined.extend(cache.codex_servers.iter().cloned());
                combined.extend(cache.claude_servers.iter().cloned());
                for entry in &cache.global_extras {
                    combined.extend(entry.servers.iter().cloned());
                }
                for entry in &cache.dynamic_extras {
                    combined.extend(entry.servers.iter().cloned());
                }
                let combined = probe::dedup_signatures(combined);
                if combined.as_slice() != &*cache.combined {
                    cache.has_distinctive = combined.iter().any(ServerSignature::is_distinctive);
                    cache.has_pool_signatures =
                        combined.iter().any(ServerSignature::is_pool_signature);
                    cache.combined = Arc::from(combined);
                    cache.generation = cache.generation.wrapping_add(1);
                }
            }
            (
                Arc::clone(&cache.combined),
                cache.has_distinctive,
                cache.has_pool_signatures,
                cache.generation,
            )
        }
    }

    /// Deterministic 64-bit hash of `exe` path (FNV-1a mixed with length +
    /// SplitMix64 finalizer) to detect `exec()` while keeping `NonMcpEntry`
    /// at 16 bytes.
    fn hash_exe(exe: &str) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (exe.len() as u64);
        for &byte in exe.as_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= h >> 30;
        h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h ^= h >> 27;
        h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^ (h >> 31)
    }

    fn pack_started_str(started: &str) -> Option<u32> {
        let mut words = started.split_whitespace();
        let lstart = [
            words.next()?,
            words.next()?,
            words.next()?,
            words.next()?,
            words.next()?,
        ];
        if words.next().is_some() {
            return None;
        }
        probe::pack_lstart(lstart)
    }

    /// Splits streamed pipe chunks into `\n`-delimited lines with zero heap
    /// allocation when lines fit within a read chunk, carrying only a single
    /// partial boundary line in `carry`.
    #[derive(Default)]
    struct LineSplitter {
        carry: Vec<u8>,
    }

    impl LineSplitter {
        fn push(&mut self, bytes: &[u8], mut on_line: impl FnMut(&[u8])) {
            let mut rest = bytes;
            if !self.carry.is_empty() {
                if let Some(nl) = memchr::memchr(b'\n', rest) {
                    self.carry.extend_from_slice(&rest[..nl]);
                    let line = trim_cr(&self.carry);
                    on_line(line);
                    self.carry.clear();
                    rest = &rest[nl + 1..];
                } else {
                    self.carry.extend_from_slice(rest);
                    return;
                }
            }
            while let Some(nl) = memchr::memchr(b'\n', rest) {
                let line = trim_cr(&rest[..nl]);
                on_line(line);
                rest = &rest[nl + 1..];
            }
            if !rest.is_empty() {
                self.carry.extend_from_slice(rest);
            }
        }

        fn finish(&mut self, mut on_line: impl FnMut(&[u8])) {
            if !self.carry.is_empty() {
                let line = trim_cr(&self.carry);
                on_line(line);
                self.carry.clear();
            }
        }
    }

    fn trim_cr(bytes: &[u8]) -> &[u8] {
        bytes.strip_suffix(b"\r").unwrap_or(bytes)
    }

    fn with_line_str(bytes: &[u8], f: impl FnOnce(&str)) {
        match std::str::from_utf8(bytes) {
            Ok(text) => f(text),
            Err(_) => {
                let owned = String::from_utf8_lossy(bytes);
                f(&owned);
            }
        }
    }

    /// Spawn a read-only system probe (`ps`, `sysctl`, `ioreg`, `top`,
    /// `launchctl list`) with a minimal two-variable locale environment so
    /// `std::process::Command` never copies the entire parent `environ` into a
    /// heap `BTreeMap` or dirties extra `MALLOC_SMALL` size classes.
    fn spawn_piped(program: &str, args: &[&str]) -> Result<(Child, ChildStdout), AppError> {
        let failed = |source| AppError::Command {
            program: program.to_owned(),
            source,
        };
        let mut child = Command::new(program)
            .args(args)
            .env_clear()
            .env("LC_ALL", "en_US.UTF-8")
            .env("LANG", "en_US.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(failed)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| failed(io::Error::other("no stdout")))?;
        Ok((child, stdout))
    }

    /// Spawn a command (`open`, `launchctl bootstrap/bootout/...`) preserving
    /// the user's inherited environment without copying it into `CommandEnv`.
    fn spawn_piped_inherited(
        program: &str,
        args: &[&str],
    ) -> Result<(Child, ChildStdout), AppError> {
        let failed = |source| AppError::Command {
            program: program.to_owned(),
            source,
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(failed)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| failed(io::Error::other("no stdout")))?;
        Ok((child, stdout))
    }

    /// Reap an exiting child after stdout EOF without the 1 ms minimum latency
    /// of a coarse initial sleep: try immediately, yield twice for kernel
    /// teardown, then exponentially back off starting at 100 µs.
    fn reap_child(
        child: &mut Child,
        deadline: Instant,
        program: &str,
        timeout: Duration,
    ) -> Result<ExitStatus, AppError> {
        let failed = |source| AppError::Command {
            program: program.to_owned(),
            source,
        };
        if let Some(status) = child.try_wait().map_err(failed)? {
            return Ok(status);
        }
        for _ in 0..2 {
            std::thread::yield_now();
            if let Some(status) = child.try_wait().map_err(failed)? {
                return Ok(status);
            }
        }
        let mut backoff = Duration::from_micros(100);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::CommandTimedOut {
                    program: program.to_owned(),
                    seconds: timeout.as_secs(),
                });
            }
            std::thread::sleep(backoff.min(remaining));
            if let Some(status) = child.try_wait().map_err(failed)? {
                return Ok(status);
            }
            backoff = (backoff * 2).min(Duration::from_millis(10));
        }
    }

    /// Run a system command in US English, streaming stdout line-by-line from a
    /// stack buffer with `poll` (no reader thread, no whole-output heap buffer)
    /// and aborting on timeout.
    fn run_lines_with_timeout(
        program: &str,
        args: &[&str],
        timeout: Duration,
        mut on_line: impl FnMut(&[u8]),
    ) -> Result<ExitStatus, AppError> {
        let failed = |source| AppError::Command {
            program: program.to_owned(),
            source,
        };
        let (mut child, mut stdout) = spawn_piped(program, args)?;
        let deadline = Instant::now() + timeout;
        let mut chunk = [0u8; 8192];
        let mut splitter = LineSplitter::default();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(stdout);
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::CommandTimedOut {
                    program: program.to_owned(),
                    seconds: timeout.as_secs(),
                });
            }
            let timeout_ms = u16::try_from(remaining.as_millis().max(1)).unwrap_or(u16::MAX);
            let mut fds = [PollFd::new(stdout.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(timeout_ms)) {
                Err(Errno::EINTR) => continue,
                Err(err) => {
                    drop(stdout);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(failed(io::Error::from(err)));
                }
                Ok(0) => continue,
                Ok(_) => match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => splitter.push(&chunk[..read], &mut on_line),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        drop(stdout);
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(failed(err));
                    }
                },
            }
        }
        splitter.finish(&mut on_line);
        drop(stdout);
        reap_child(&mut child, deadline, program, timeout)
    }

    fn run_lines(program: &str, args: &[&str], on_line: impl FnMut(&[u8])) -> Result<(), AppError> {
        let status = run_lines_with_timeout(program, args, COMMAND_TIMEOUT, on_line)?;
        if !status.success() {
            return Err(AppError::CommandFailed {
                program: program.to_owned(),
                status: status.to_string(),
            });
        }
        Ok(())
    }

    fn run_discard(program: &str, args: &[&str]) -> Result<(), AppError> {
        let failed = |source| AppError::Command {
            program: program.to_owned(),
            source,
        };
        let (mut child, mut stdout) = spawn_piped_inherited(program, args)?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut chunk = [0u8; 4096];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(stdout);
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::CommandTimedOut {
                    program: program.to_owned(),
                    seconds: COMMAND_TIMEOUT.as_secs(),
                });
            }
            let timeout_ms = u16::try_from(remaining.as_millis().max(1)).unwrap_or(u16::MAX);
            let mut fds = [PollFd::new(stdout.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(timeout_ms)) {
                Err(Errno::EINTR) => continue,
                Err(err) => {
                    drop(stdout);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(failed(io::Error::from(err)));
                }
                Ok(0) => continue,
                Ok(_) => match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        drop(stdout);
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(failed(err));
                    }
                },
            }
        }
        drop(stdout);
        let status = reap_child(&mut child, deadline, program, COMMAND_TIMEOUT)?;
        if !status.success() {
            return Err(AppError::CommandFailed {
                program: program.to_owned(),
                status: status.to_string(),
            });
        }
        Ok(())
    }

    struct GroupSlot {
        program: &'static str,
        required: bool,
        eager_reap: bool,
        child: Child,
        stdout: Option<ChildStdout>,
        splitter: LineSplitter,
        reaped: Option<Result<ExitStatus, AppError>>,
    }

    impl GroupSlot {
        fn spawn(
            program: &'static str,
            args: &[&str],
            required: bool,
            eager_reap: bool,
        ) -> Result<Self, AppError> {
            let (child, stdout) = spawn_piped(program, args)?;
            Ok(Self {
                program,
                required,
                eager_reap,
                child,
                stdout: Some(stdout),
                splitter: LineSplitter::default(),
                reaped: None,
            })
        }

        fn kill_and_reap(&mut self, timeout: Duration) {
            self.stdout = None;
            if self.reaped.is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.reaped = Some(Err(AppError::CommandTimedOut {
                    program: self.program.to_owned(),
                    seconds: timeout.as_secs(),
                }));
            }
        }
    }

    impl Drop for GroupSlot {
        fn drop(&mut self) {
            if self.reaped.is_none() {
                self.stdout = None;
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    enum GroupEvent<'a> {
        Line(&'a [u8]),
        Eof(bool),
    }

    /// Drain up to 4 concurrent child stdout pipes in a single synchronous
    /// `poll` loop using one shared stack buffer (zero threads, zero whole-output
    /// heap buffers). Calls `on_event(slot_index, event)` as lines arrive and
    /// when a slot finishes.
    fn drain_group<const K: usize>(
        slots: &mut [Option<GroupSlot>; K],
        timeout: Duration,
        mut on_event: impl FnMut(usize, GroupEvent<'_>),
    ) -> Result<(), AppError> {
        let deadline = Instant::now() + timeout;
        let mut chunk = [0u8; 8192];

        loop {
            let mut active_indices = [0usize; K];
            let mut active_count = 0usize;
            for (idx, slot) in slots.iter().enumerate() {
                if slot.as_ref().is_some_and(|s| s.stdout.is_some()) {
                    active_indices[active_count] = idx;
                    active_count += 1;
                }
            }
            if active_count == 0 {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let mut err = None;
                for (idx, slot_opt) in slots.iter_mut().enumerate() {
                    let Some(slot) = slot_opt.as_mut() else {
                        continue;
                    };
                    if slot.stdout.is_some() {
                        let required = slot.required;
                        if required && err.is_none() {
                            err = Some(AppError::CommandTimedOut {
                                program: slot.program.to_owned(),
                                seconds: timeout.as_secs(),
                            });
                        }
                        slot.kill_and_reap(timeout);
                        if !required {
                            on_event(idx, GroupEvent::Eof(false));
                        }
                    }
                }
                if let Some(error) = err {
                    return Err(error);
                }
                break;
            }
            let timeout_ms = u16::try_from(remaining.as_millis().max(1)).unwrap_or(u16::MAX);
            let mut ready = [false; K];
            {
                let mut fds = std::array::from_fn::<_, K, _>(|k| {
                    let fd = slots[active_indices[k.min(active_count - 1)]]
                        .as_ref()
                        .and_then(|s| s.stdout.as_ref())
                        .expect("active slot has stdout")
                        .as_fd();
                    PollFd::new(fd, PollFlags::POLLIN)
                });
                match poll(&mut fds[..active_count], PollTimeout::from(timeout_ms)) {
                    Err(Errno::EINTR) => continue,
                    Err(err) => {
                        let program = slots
                            .iter()
                            .flatten()
                            .find(|s| s.required)
                            .map_or("/bin/ps", |s| s.program)
                            .to_owned();
                        for slot in slots.iter_mut().flatten() {
                            slot.kill_and_reap(timeout);
                        }
                        return Err(AppError::Command {
                            program,
                            source: io::Error::from(err),
                        });
                    }
                    Ok(0) => continue,
                    Ok(_) => {
                        for k in 0..active_count {
                            if fds[k].revents().is_some_and(|flags| !flags.is_empty()) {
                                ready[k] = true;
                            }
                        }
                    }
                }
            }

            for k in 0..active_count {
                if !ready[k] {
                    continue;
                }
                let idx = active_indices[k];
                let Some(slot) = slots[idx].as_mut() else {
                    continue;
                };
                let read_res = match slot.stdout.as_mut() {
                    Some(stdout) => stdout.read(&mut chunk),
                    None => continue,
                };
                match read_res {
                    Ok(0) => {
                        slot.splitter
                            .finish(|line| on_event(idx, GroupEvent::Line(line)));
                        slot.stdout = None;
                        // Give fast optional children whose verified output
                        // filters a slower sibling (e.g. `launchctl list`), or
                        // required children whose failure must abort stalled
                        // optional siblings promptly, a brief reap window at EOF.
                        let mut status = slot.child.try_wait().ok().flatten();
                        if status.is_none() && (slot.eager_reap || slot.required) {
                            for delay_us in [0u64, 0, 100, 250, 500, 1000] {
                                if delay_us == 0 {
                                    std::thread::yield_now();
                                } else {
                                    std::thread::sleep(Duration::from_micros(delay_us));
                                }
                                status = slot.child.try_wait().ok().flatten();
                                if status.is_some() {
                                    break;
                                }
                            }
                        }
                        if let Some(status) = status {
                            let ok = status.success();
                            let required = slot.required;
                            let program = slot.program;
                            slot.reaped = Some(Ok(status));
                            on_event(idx, GroupEvent::Eof(ok));
                            if required && !ok {
                                for other in slots.iter_mut().flatten() {
                                    other.kill_and_reap(timeout);
                                }
                                return Err(AppError::CommandFailed {
                                    program: program.to_owned(),
                                    status: status.to_string(),
                                });
                            }
                        }
                    }
                    Ok(read) => {
                        slot.splitter
                            .push(&chunk[..read], |line| on_event(idx, GroupEvent::Line(line)));
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        let required = slot.required;
                        let program = slot.program;
                        slot.kill_and_reap(timeout);
                        if required {
                            for other in slots.iter_mut().flatten() {
                                other.kill_and_reap(timeout);
                            }
                            return Err(AppError::Command {
                                program: program.to_owned(),
                                source: err,
                            });
                        }
                        on_event(idx, GroupEvent::Eof(false));
                    }
                }
            }
        }

        // Reap required children first so a failed/timed-out required child
        // aborts immediately without waiting on slow optional siblings.
        for required_pass in [true, false] {
            for (idx, slot_opt) in slots.iter_mut().enumerate() {
                let Some(slot) = slot_opt.as_mut() else {
                    continue;
                };
                if slot.required != required_pass {
                    continue;
                }
                if slot.reaped.is_none() {
                    let res = reap_child(&mut slot.child, deadline, slot.program, timeout);
                    let ok = res.as_ref().is_ok_and(ExitStatus::success);
                    slot.reaped = Some(res);
                    on_event(idx, GroupEvent::Eof(ok));
                }
                if slot.required {
                    match &slot.reaped {
                        Some(Ok(status)) if status.success() => {}
                        Some(Ok(status)) => {
                            let program = slot.program.to_owned();
                            let status = status.to_string();
                            for other in slots.iter_mut().flatten() {
                                other.kill_and_reap(timeout);
                            }
                            return Err(AppError::CommandFailed { program, status });
                        }
                        Some(Err(_)) => {
                            let err = slot.reaped.take().unwrap().unwrap_err();
                            slot.reaped = Some(Err(AppError::CommandTimedOut {
                                program: slot.program.to_owned(),
                                seconds: timeout.as_secs(),
                            }));
                            for other in slots.iter_mut().flatten() {
                                other.kill_and_reap(timeout);
                            }
                            return Err(err);
                        }
                        None => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    fn file_mtime_secs(path: &Path) -> Option<u64> {
        fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs())
    }

    /// Newest write to a Codex session file; Codex appends to it on every turn.
    /// When `early_exit_after` is `Some(cutoff)`, scans newest date directories
    /// first and stops as soon as any file was written strictly after `cutoff`.
    fn codex_activity_walk(root: &Path, early_exit_after: Option<u64>) -> Option<(u64, PathBuf)> {
        const MAX_DEPTH: u8 = 4;
        let mut stack = vec![(root.to_path_buf(), 0u8)];
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut newest: Option<(u64, PathBuf)> = None;

        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let descend = depth + 1 < MAX_DEPTH;
            subdirs.clear();
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name();
                let bytes = name.as_encoded_bytes();
                if bytes.starts_with(b"rollout-")
                    && bytes.ends_with(b".jsonl")
                    && let Ok(modified) = entry.metadata().and_then(|meta| meta.modified())
                    && let Ok(elapsed) = modified.duration_since(UNIX_EPOCH)
                {
                    let secs = elapsed.as_secs();
                    if newest.as_ref().is_none_or(|(prev, _)| secs > *prev) {
                        newest = Some((secs, entry.path()));
                    }
                    if early_exit_after.is_some_and(|cutoff| secs > cutoff) {
                        return newest;
                    }
                }
                if descend && entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                    subdirs.push(entry.path());
                }
            }
            if !subdirs.is_empty() {
                if early_exit_after.is_some() {
                    // Ascending sort so the LIFO stack pops newest YYYY/MM/DD first.
                    subdirs.sort_unstable();
                }
                for subdir in subdirs.drain(..) {
                    stack.push((subdir, depth + 1));
                }
            }
        }
        newest
    }

    #[cfg(test)]
    pub(crate) fn codex_activity_at(root: &Path, early_exit_after: Option<u64>) -> Option<u64> {
        codex_activity_walk(root, early_exit_after).map(|(secs, _)| secs)
    }

    /// Newest Antigravity conversation/summary write under `~/.gemini/antigravity`.
    /// Checks O(1) root summary files first, then scans `conversations/` once with
    /// early exit when `early_exit_after` is satisfied.
    fn antigravity_activity_walk(
        root: &Path,
        early_exit_after: Option<u64>,
    ) -> Option<(u64, PathBuf)> {
        let mut newest: Option<(u64, PathBuf)> = None;
        for name in [
            "conversation_summaries.db-wal",
            "conversation_summaries.db",
            "agyhub_summaries_proto.pb",
            "jetbox_summaries_proto.pb",
            "history.jsonl",
        ] {
            let path = root.join(name);
            if let Some(secs) = file_mtime_secs(&path) {
                if newest.as_ref().is_none_or(|(prev, _)| secs > *prev) {
                    newest = Some((secs, path));
                }
                if early_exit_after.is_some_and(|cutoff| secs > cutoff) {
                    return newest;
                }
            }
        }
        let conv_dir = root.join("conversations");
        if let Ok(entries) = fs::read_dir(&conv_dir) {
            for entry in entries.filter_map(Result::ok) {
                if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified())
                    && let Ok(elapsed) = modified.duration_since(UNIX_EPOCH)
                {
                    let secs = elapsed.as_secs();
                    if newest.as_ref().is_none_or(|(prev, _)| secs > *prev) {
                        newest = Some((secs, entry.path()));
                    }
                    if early_exit_after.is_some_and(|cutoff| secs > cutoff) {
                        return newest;
                    }
                }
            }
        }
        newest
    }

    const OPENCODE_SQL_V2: &str = "SELECT max(ts) FROM (SELECT max(time_updated) AS ts FROM session_v2 UNION ALL SELECT max(time_created) AS ts FROM session_message UNION ALL SELECT max(time_updated) AS ts FROM session_part);";
    const OPENCODE_SQL_V1: &str = "SELECT max(ts) FROM (SELECT max(time_updated) AS ts FROM session UNION ALL SELECT max(time_created) AS ts FROM message);";
    const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);

    fn query_sqlite_max_timestamp(db_path: &Path, sql: &str) -> Result<Option<u64>, ()> {
        let db_arg = db_path.to_str().ok_or(())?;
        let mut parsed: Option<u64> = None;
        let status = run_lines_with_timeout(
            "/usr/bin/sqlite3",
            &["-readonly", "-cmd", ".timeout 500", db_arg, sql],
            SQLITE_TIMEOUT,
            |bytes| {
                with_line_str(bytes, |line| {
                    let trimmed = line.trim();
                    if let Ok(raw) = trimmed.parse::<u64>()
                        && raw > 0
                    {
                        // OpenCode stores Unix milliseconds; normalize to seconds.
                        let secs = if raw > 10_000_000_000 {
                            raw / 1000
                        } else {
                            raw
                        };
                        parsed = Some(parsed.map_or(secs, |prev| prev.max(secs)));
                    }
                });
            },
        )
        .map_err(|_| ())?;
        if status.success() {
            Ok(parsed)
        } else {
            Err(())
        }
    }

    /// Authoritative OpenCode session activity timestamp in Unix seconds.
    ///
    /// Queries real session/message timestamps from `~/.local/share/opencode/opencode.db`
    /// via read-only `sqlite3` (which transparently reads uncommitted WAL frames
    /// while ignoring OpenCode's 5-minute background `kv` catalog writes to
    /// `opencode.db-wal` and background fs-watcher entries in `opencode.log`),
    /// combined with `~/.local/state/opencode/prompt-history.jsonl` mtime.
    fn query_opencode_db_activity(db_path: &Path) -> Option<u64> {
        if !db_path.is_file() {
            return None;
        }
        if let Ok(ts) = query_sqlite_max_timestamp(db_path, OPENCODE_SQL_V2) {
            return ts;
        }
        if let Ok(ts) = query_sqlite_max_timestamp(db_path, OPENCODE_SQL_V1) {
            return ts;
        }
        // Fallback only when `sqlite3` cannot read the DB (e.g. locked or non-SQLite fixture);
        // intentionally never reads `opencode.db-wal` or `opencode.log` mtime.
        file_mtime_secs(db_path)
    }

    /// Whether a `ppid == 1` agent/app executable needs launchd & sidecar
    /// verification: `(is_ag_bundled_srv, is_sidecar_candidate_app_exe)`.
    fn ppid1_ag_or_app_check_flags(exe: &str) -> (bool, bool) {
        let is_ag_bundled_srv =
            owners::is_antigravity_server_candidate_exe(exe) && exe.contains(".app/Contents/");
        let is_app_exe = owners::app_bundle_path(exe).is_some()
            && !owners::is_claude_app_exe(exe)
            && !owners::is_codex_app_exe(exe)
            && !owners::is_opencode_app_fallback_exe(exe);
        (is_ag_bundled_srv, is_app_exe)
    }

    /// PIDs whose `args` column is read by ownership classification:
    /// - all Codex, Antigravity `language_server*`, and OpenCode runtimes
    /// - non-launchd `ppid == 1` app bundle processes not already verified as
    ///   non-sidecar in `cached_non_mcp_apps` (to distinguish real apps from
    ///   orphaned Antigravity `sidecar_sdk`, including renamed clones)
    /// - direct helper children of Codex/Antigravity/OpenCode/`agy` runtimes (including
    ///   inherited-TTY children when the runtime itself has a TTY!)
    /// - direct inherited-TTY helper children of TTY-attached Claude CLI sessions
    /// - `ppid > 1` processes whose executable could match a configured MCP pool
    ///   signature (enabling generic dynamic discovery of Cursor, Windsurf, Zed,
    ///   Gemini CLI, and other coding tools)
    /// - non-launchd `ppid == 1` orphan candidates not already in `known_helpers`.
    fn arg_candidate_pids(
        snapshot: &Snapshot,
        user: u32,
        has_distinctive: bool,
        has_pool_signatures: bool,
        known_helpers: &BTreeSet<Identity>,
        launchd_jobs: Option<&[i32]>,
        cached_non_mcp_apps: &[i32],
    ) -> Vec<i32> {
        let processes = &snapshot.processes;
        let mut pids = Vec::new();
        let mut runtime_indices: Vec<usize> = Vec::new();
        let mut tty_claude_indices: Vec<usize> = Vec::new();

        for (idx, process) in processes.iter().enumerate() {
            if process.uid != user {
                continue;
            }
            let exe = snapshot.exe(process);
            if owners::is_codex_exe(exe)
                || owners::is_antigravity_server_candidate_exe(exe)
                || owners::is_opencode_runtime_exe(exe)
            {
                pids.push(process.pid);
                runtime_indices.push(idx);
            } else if owners::is_antigravity_cli_exe(exe) {
                runtime_indices.push(idx);
            } else if process.has_tty && owners::is_claude_session_exe(exe) {
                tty_claude_indices.push(idx);
            } else if process.ppid == 1
                && ppid1_ag_or_app_check_flags(exe).1
                && cached_non_mcp_apps.binary_search(&process.pid).is_err()
                && ((owners::is_antigravity_app_fallback_exe(exe)
                    && launchd_jobs.is_none_or(|jobs| jobs.binary_search(&process.pid).is_err()))
                    || (launchd_jobs.is_some_and(|jobs| jobs.binary_search(&process.pid).is_err())
                        && ((!process.has_tty && process.age_secs >= owners::ORPHAN_MIN_AGE_SECS)
                            || Identity::in_set(
                                known_helpers,
                                process.pid,
                                snapshot.started(process),
                            ))))
            {
                // Distinguish a real desktop app from an orphaned `sidecar_sdk`
                // (for Antigravity, Antigravity US, or any arbitrarily named clone).
                pids.push(process.pid);
            }
        }

        if !runtime_indices.is_empty() || (has_pool_signatures && !tty_claude_indices.is_empty()) {
            let mut children: Vec<(i32, i32)> = Vec::with_capacity(processes.len());
            for process in processes {
                if process.pid != process.ppid {
                    children.push((process.ppid, process.pid));
                }
            }
            children.sort_unstable();
            let direct = |ppid: i32| -> &[(i32, i32)] {
                let start = children.partition_point(|&(parent, _)| parent < ppid);
                let rest = &children[start..];
                let len = rest.partition_point(|&(parent, _)| parent == ppid);
                &rest[..len]
            };

            for &rt_idx in &runtime_indices {
                let rt = &processes[rt_idx];
                let rt_exe = snapshot.exe(rt);
                let rt_bundle = rt_exe
                    .split_once(".app/")
                    .map(|(prefix, _)| &rt_exe[..prefix.len() + ".app/".len()]);
                let is_ag_server = owners::is_antigravity_server_candidate_exe(rt_exe);

                for &(_, helper_pid) in direct(rt.pid) {
                    let Ok(h_idx) = processes.binary_search_by_key(&helper_pid, |p| p.pid) else {
                        continue;
                    };
                    let helper = &processes[h_idx];
                    if (!rt.has_tty && helper.has_tty) || owners::is_shell(snapshot, helper) {
                        continue;
                    }
                    let helper_exe = snapshot.exe(helper);
                    if owners::is_non_helper_command_exe(helper_exe) {
                        continue;
                    }
                    // Always inspect potential Electron sidecars under language_server*.
                    if is_ag_server && owners::app_bundle_path(helper_exe).is_some() {
                        pids.push(helper_pid);
                        continue;
                    }
                    if has_pool_signatures
                        && !owners::is_agent_host_exe(helper_exe)
                        && !rt_bundle.is_some_and(|bundle| helper_exe.starts_with(bundle))
                    {
                        pids.push(helper_pid);
                    }
                }
            }

            if has_pool_signatures {
                for &cl_idx in &tty_claude_indices {
                    let cl = &processes[cl_idx];
                    for &(_, helper_pid) in direct(cl.pid) {
                        let Ok(h_idx) = processes.binary_search_by_key(&helper_pid, |p| p.pid)
                        else {
                            continue;
                        };
                        let helper = &processes[h_idx];
                        if !helper.has_tty
                            || helper.tty_id != cl.tty_id
                            || owners::is_shell(snapshot, helper)
                        {
                            continue;
                        }
                        let helper_exe = snapshot.exe(helper);
                        if !owners::is_non_helper_command_exe(helper_exe)
                            && !owners::is_agent_host_exe(helper_exe)
                        {
                            pids.push(helper_pid);
                        }
                    }
                }
            }
        }

        if has_pool_signatures {
            let pool_sigs: Vec<&ServerSignature> = snapshot
                .mcp_servers
                .iter()
                .filter(|s| s.is_pool_signature())
                .collect();
            if !pool_sigs.is_empty() {
                for process in processes {
                    if process.uid != user || process.ppid <= 1 {
                        continue;
                    }
                    let exe = snapshot.exe(process);
                    if owners::is_shell_exe(exe)
                        || owners::is_non_helper_command_exe(exe)
                        || owners::is_agent_host_exe(exe)
                        || owners::app_bundle_path(exe).is_some()
                    {
                        continue;
                    }
                    if let Ok(p_idx) = processes.binary_search_by_key(&process.ppid, |p| p.pid) {
                        let parent = &processes[p_idx];
                        let parent_exe = snapshot.exe(parent);
                        // Non-TTY Claude sessions already classify all direct helpers without args.
                        if !parent.has_tty && owners::is_claude_session_exe(parent_exe) {
                            continue;
                        }
                        // Children of a `ppid == 1` non-launchd orphan candidate belong to that orphan.
                        if owners::is_orphan_base_candidate_fields(
                            parent.ppid,
                            parent.uid,
                            parent.has_tty,
                            parent.age_secs,
                            parent_exe,
                            user,
                        ) && launchd_jobs
                            .is_none_or(|jobs| jobs.binary_search(&parent.pid).is_err())
                        {
                            continue;
                        }
                    }
                    if pool_sigs.iter().any(|server| server.could_match_exe(exe)) {
                        pids.push(process.pid);
                    }
                }
            }
        }

        if has_distinctive && let Some(jobs) = launchd_jobs {
            for process in processes {
                if owners::is_orphan_base_candidate(snapshot, process, user)
                    && !Identity::in_set(known_helpers, process.pid, snapshot.started(process))
                    && jobs.binary_search(&process.pid).is_err()
                {
                    pids.push(process.pid);
                }
            }
        }

        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// Stack-backed PID buffer (up to 512 entries = 2 KiB on stack) so
    /// `launchctl list` parsing avoids allocating in `MALLOC_SMALL`, spilling
    /// to a heap `Vec` only on hosts with > 512 live launchd jobs.
    struct PidSetBuf {
        inline: [i32; 512],
        len: usize,
        heap: Vec<i32>,
    }

    impl PidSetBuf {
        fn new() -> Self {
            Self {
                inline: [0; 512],
                len: 0,
                heap: Vec::new(),
            }
        }

        fn push(&mut self, pid: i32) {
            if !self.heap.is_empty() {
                self.heap.push(pid);
            } else if self.len < self.inline.len() {
                self.inline[self.len] = pid;
                self.len += 1;
            } else {
                self.heap = Vec::with_capacity(self.inline.len() * 2);
                self.heap.extend_from_slice(&self.inline);
                self.heap.push(pid);
            }
        }

        fn sort_and_dedup(&mut self) {
            if !self.heap.is_empty() {
                self.heap.sort_unstable();
                self.heap.dedup();
            } else {
                let slice = &mut self.inline[..self.len];
                slice.sort_unstable();
                let mut write = 0usize;
                for read in 0..self.len {
                    if write == 0 || self.inline[read] != self.inline[write - 1] {
                        self.inline[write] = self.inline[read];
                        write += 1;
                    }
                }
                self.len = write;
            }
        }

        fn as_slice(&self) -> &[i32] {
            if !self.heap.is_empty() {
                self.heap.as_slice()
            } else {
                &self.inline[..self.len]
            }
        }
    }

    fn quantize_process_cap(needed: usize) -> usize {
        // Keep `Vec<Process>` (72 B/item) on fixed capacity tiers starting at
        // 112 (8,064 B, reusing dyld's already-dirty 8 KiB MALLOC_SMALL bucket)
        // so steady-state passes never step across arbitrary intermediate slabs.
        if needed <= 112 {
            112
        } else if needed <= 224 {
            224
        } else {
            needed.next_power_of_two().min(1024)
        }
    }

    fn quantize_strings_cap(needed: usize) -> usize {
        if needed <= 8192 {
            8192
        } else if needed <= 16 * 1024 {
            16 * 1024
        } else if needed <= 32 * 1024 {
            32 * 1024
        } else {
            64 * 1024
        }
    }

    fn retain_snapshot_launchd_jobs(snapshot: &Snapshot, jobs: &[i32]) -> Vec<i32> {
        let mut kept = Vec::new();
        for process in &snapshot.processes {
            if jobs.binary_search(&process.pid).is_ok() {
                kept.push(process.pid);
            }
        }
        kept
    }

    /// Format comma-separated PIDs into a 4 KiB stack buffer (fitting > 500
    /// PIDs with zero heap allocation), spilling to a `String` only if longer.
    fn with_pid_list<R>(pids: &[i32], f: impl FnOnce(&str) -> R) -> R {
        let mut stack = [0u8; 4096];
        let mut cursor = io::Cursor::new(&mut stack[..]);
        let mut fits = true;
        for (index, pid) in pids.iter().enumerate() {
            let res = if index > 0 {
                write!(cursor, ",{pid}")
            } else {
                write!(cursor, "{pid}")
            };
            if res.is_err() {
                fits = false;
                break;
            }
        }
        if fits {
            let len = cursor.position() as usize;
            if let Ok(list) = std::str::from_utf8(&stack[..len]) {
                return f(list);
            }
        }
        let mut list = String::with_capacity(pids.len() * 7);
        for (index, pid) in pids.iter().enumerate() {
            if index > 0 {
                list.push(',');
            }
            let _ = write!(list, "{pid}");
        }
        f(&list)
    }

    /// Process one `pid=,args=` line into `snapshot`, interning `args_str`
    /// whenever the process is needed for runtime/sidecar/helper classification
    /// and discarding non-matching `ppid == 1` non-MCP daemon args into
    /// `new_non_mcp`.
    fn classify_args_line(
        snapshot: &mut Snapshot,
        line: &str,
        user: u32,
        known_helpers: &BTreeSet<Identity>,
        launchd_jobs: Option<&[i32]>,
        mcp_servers: &[ServerSignature],
        new_non_mcp: &mut Vec<NonMcpEntry>,
    ) {
        let Some((pid, args_str)) = probe::parse_pid_column_line(line) else {
            return;
        };
        let Ok(idx) = snapshot.processes.binary_search_by_key(&pid, |p| p.pid) else {
            return;
        };
        let process = snapshot.processes[idx];
        let exe = snapshot.exe(&process);
        // Antigravity `language_server*` (including arbitrarily renamed clones)
        // and Antigravity `sidecar_sdk` always require their args interned for
        // `Ownership::new`, `--app_data_dir`, and orphan classification.
        let must_intern = owners::is_antigravity_server_candidate_exe(exe)
            || owners::is_antigravity_sidecar_args(args_str);
        let is_non_sidecar_app = process.ppid == 1
            && process.uid == user
            && !process.has_tty
            && process.age_secs >= owners::ORPHAN_MIN_AGE_SECS
            && owners::app_bundle_path(exe).is_some();
        let is_orphan_cand = !must_intern
            && launchd_jobs.is_some_and(|jobs| {
                (owners::is_orphan_base_candidate(snapshot, &process, user) || is_non_sidecar_app)
                    && !Identity::in_set(known_helpers, pid, snapshot.started(&process))
                    && jobs.binary_search(&pid).is_err()
            });
        if is_orphan_cand {
            if args_str.is_empty() {
                return;
            }
            let matched = !is_non_sidecar_app
                && mcp_servers
                    .iter()
                    .any(|s| s.is_distinctive() && s.matches_parts(exe, args_str));
            if matched {
                let span = snapshot.intern(args_str);
                snapshot.processes[idx].args = span;
            } else if let Some(sk) = pack_started_str(snapshot.started(&process)) {
                new_non_mcp.push(NonMcpEntry {
                    pid,
                    started_key: sk,
                    exe_hash: hash_exe(exe),
                });
            }
        } else {
            let span = snapshot.intern(args_str);
            snapshot.processes[idx].args = span;
        }
    }

    /// Fetch `args` for `candidate_pids`, matching `ppid == 1` orphan
    /// candidates directly against `mcp_servers` on the streamed stack slice
    /// without storing `args` for non-matching daemons. Returns newly verified
    /// non-MCP `ppid == 1` entries for `WatchCache`.
    fn fetch_args_and_classify_orphans(
        snapshot: &mut Snapshot,
        candidate_pids: &[i32],
        user: u32,
        known_helpers: &BTreeSet<Identity>,
        launchd_jobs: Option<&[i32]>,
        mcp_servers: &[ServerSignature],
    ) -> Result<Vec<NonMcpEntry>, AppError> {
        if candidate_pids.is_empty() {
            return Ok(Vec::new());
        }
        let mut new_non_mcp = Vec::with_capacity(candidate_pids.len());
        let mut handle_line = |line: &str| {
            classify_args_line(
                snapshot,
                line,
                user,
                known_helpers,
                launchd_jobs,
                mcp_servers,
                &mut new_non_mcp,
            );
        };

        if candidate_pids.len() <= TARGETED_PS_MAX_PIDS {
            let mut saw_output = false;
            let status = with_pid_list(candidate_pids, |list| {
                run_lines_with_timeout(
                    "/bin/ps",
                    &["-ww", "-o", "pid=,args=", "-p", list],
                    COMMAND_TIMEOUT,
                    |bytes| {
                        if !bytes.is_empty() {
                            saw_output = true;
                        }
                        with_line_str(bytes, &mut handle_line);
                    },
                )
            })?;
            if !status.success() {
                // `ps -p` exits 1 with empty output when every requested PID exited.
                if status.code() == Some(1) && !saw_output {
                    return Ok(new_non_mcp);
                }
                return Err(AppError::CommandFailed {
                    program: "/bin/ps".into(),
                    status: status.to_string(),
                });
            }
        } else {
            run_lines("/bin/ps", &["-axww", "-o", "pid=,args="], |bytes| {
                with_line_str(bytes, &mut handle_line);
            })?;
        }
        Ok(new_non_mcp)
    }

    fn fetch_launchd_pids() -> Option<PidSetBuf> {
        let mut pids = PidSetBuf::new();
        run_lines("/bin/launchctl", &["list"], |bytes| {
            with_line_str(bytes, |line| {
                if let Some(pid) = probe::launchd_pid_line(line) {
                    pids.push(pid);
                }
            });
        })
        .ok()?;
        pids.sort_and_dedup();
        Some(pids)
    }

    fn query_user_idle_secs() -> Option<u64> {
        let mut idle = None;
        let _ = run_lines(
            "/usr/sbin/ioreg",
            &["-c", "IOHIDSystem", "-d", "4", "-k", "HIDIdleTime"],
            |bytes| {
                if idle.is_none() {
                    with_line_str(bytes, |line| {
                        idle = probe::hid_idle_secs_line(line);
                    });
                }
            },
        );
        if idle.is_some() {
            return idle;
        }
        // Fallback if IOHIDSystem ever sits deeper than depth 4.
        let _ = run_lines(
            "/usr/sbin/ioreg",
            &["-r", "-c", "IOHIDSystem", "-d", "1"],
            |bytes| {
                if idle.is_none() {
                    with_line_str(bytes, |line| {
                        idle = probe::hid_idle_secs_line(line);
                    });
                }
            },
        );
        idle
    }

    impl System for MacOs {
        fn begin_pass(
            &self,
            known_helpers: &BTreeSet<Identity>,
            include_user_idle: bool,
        ) -> Result<Snapshot, AppError> {
            let user = getuid().as_raw();
            let (mcp_servers, has_distinctive, has_pool_signatures, mcp_generation) =
                self.load_mcp_servers();
            let mut cache = self.watch_cache.borrow_mut();

            if !has_distinctive {
                // Distinctively-configured MCP servers were removed or cleared:
                // reset the full daemon seed so re-enabling them overlaps
                // `launchctl` with `ps` again, while keeping cached `.app`
                // launchd/non-sidecar entries via pruning so steady-state passes
                // do not re-run `launchctl list` on every tick.
                cache.launchd_seeded = false;
            }
            if cache.non_mcp_generation != mcp_generation
                || cache.non_mcp_passes >= NON_MCP_MAX_PASSES
            {
                cache.non_mcp.clear();
                cache.non_mcp_generation = mcp_generation;
                cache.non_mcp_passes = 0;
            }

            // On the first pass with distinctive MCP servers, overlap
            // `launchctl list` with `ps` and `sysctl` so ~150+ launchd services
            // are filtered on the fly during `ps` streaming. On subsequent
            // steady-state passes `launchctl` is skipped unless an uncached
            // `ppid == 1` candidate or `known_helper` appears.
            let run_initial_launchd = has_distinctive && !cache.launchd_seeded;
            let init_cap = if cache.last_process_count > 0 {
                quantize_process_cap(cache.last_process_count + 16)
            } else {
                112
            };
            let init_strings_cap = if cache.last_strings_len > 0 {
                quantize_strings_cap(cache.last_strings_len + 1024)
            } else {
                8192
            };
            let mut snapshot = Snapshot {
                taken_at: 0,
                user,
                pressure: crate::model::Pressure::default(),
                swap: crate::model::Swap::default(),
                user_idle_secs: None,
                codex_activity_at: None,
                antigravity_activity_at: None,
                antigravity_activities: BTreeMap::new(),
                opencode_activity_at: None,
                launchd_jobs: None,
                mcp_servers: Arc::clone(&mcp_servers),
                processes: Vec::with_capacity(init_cap),
                strings: String::with_capacity(init_strings_cap),
            };

            let mut launchd_raw = PidSetBuf::new();
            let mut launchd_ready = false;
            let mut new_launchd_entries: Vec<(i32, u32)> = if run_initial_launchd {
                Vec::with_capacity(256)
            } else {
                Vec::new()
            };
            let mut cached_kept_launchd: Vec<i32> = Vec::new();
            let mut cached_non_mcp_apps: Vec<i32> = Vec::new();
            let mut launchd_seen = [0u64; SEEN_BITSET_WORDS];
            let mut non_mcp_seen = [0u64; SEEN_BITSET_WORDS];
            let mut ps_non_empty = 0usize;
            let mut ps_parsed = 0usize;
            let mut ps_saw_before_launchd = false;
            let mut has_uncached_orphan_candidate = false;
            let mut has_opencode_pty_candidate = false;

            let mut pressure = crate::model::Pressure::default();
            let mut swap = crate::model::Swap::default();
            let mut user_idle_secs: Option<u64> = None;

            const SLOT_LAUNCHD: usize = 0;
            const SLOT_SYSCTL: usize = 1;
            const SLOT_IOREG: usize = 2;
            const SLOT_PS: usize = 3;

            let mut slots: [Option<GroupSlot>; 4] = [
                if run_initial_launchd {
                    GroupSlot::spawn("/bin/launchctl", &["list"], false, true).ok()
                } else {
                    None
                },
                GroupSlot::spawn(
                    "/usr/sbin/sysctl",
                    &["kern.memorystatus_vm_pressure_level", "vm.swapusage"],
                    false,
                    false,
                )
                .ok(),
                if include_user_idle {
                    GroupSlot::spawn(
                        "/usr/sbin/ioreg",
                        &["-c", "IOHIDSystem", "-d", "4", "-k", "HIDIdleTime"],
                        false,
                        false,
                    )
                    .ok()
                } else {
                    None
                },
                Some(GroupSlot::spawn(
                    "/bin/ps",
                    &[
                        "-axww",
                        "-o",
                        "pid=,ppid=,uid=,tty=,state=,time=,etime=,lstart=,comm=",
                    ],
                    true,
                    false,
                )?),
            ];

            drain_group(&mut slots, COMMAND_TIMEOUT, |slot_idx, event| match event {
                GroupEvent::Eof(ok) => {
                    if slot_idx == SLOT_LAUNCHD && ok {
                        launchd_raw.sort_and_dedup();
                        launchd_ready = true;
                    }
                }
                GroupEvent::Line(bytes) => match slot_idx {
                    SLOT_LAUNCHD => with_line_str(bytes, |line| {
                        if let Some(pid) = probe::launchd_pid_line(line) {
                            launchd_raw.push(pid);
                        }
                    }),
                    SLOT_SYSCTL => with_line_str(bytes, |line| {
                        probe::sysctl_line(line, &mut pressure, &mut swap);
                    }),
                    SLOT_IOREG => {
                        if user_idle_secs.is_none() {
                            with_line_str(bytes, |line| {
                                user_idle_secs = probe::hid_idle_secs_line(line);
                            });
                        }
                    }
                    SLOT_PS => with_line_str(bytes, |line| {
                        if line.trim().is_empty() {
                            return;
                        }
                        ps_non_empty += 1;
                        let Some(row) = probe::parse_ps_line(line) else {
                            return;
                        };
                        ps_parsed += 1;
                        if row.zombie {
                            return;
                        }
                        let mut stack_started = [0u8; 40];
                        let mut heap_started: Option<String> = None;
                        if row.ppid <= 1 {
                            if row.uid != user {
                                return;
                            }
                            let is_agent = owners::is_agent_or_app_candidate_exe(row.exe);
                            if is_agent {
                                // A desktop-bundled Antigravity `language_server` or an
                                // orphaned `sidecar_sdk` (whose exe is inside `.app/Contents/MacOS/`)
                                // at `ppid == 1` may be an orphan after its app exits; verify
                                // launchd membership (using `cache.launchd` / `cache.non_mcp`
                                // on steady-state passes while always retaining the process in
                                // `snapshot.processes` for desktop app & parent-chain discovery).
                                if row.ppid == 1
                                    && !row.has_tty
                                    && row.age_secs >= owners::ORPHAN_MIN_AGE_SECS
                                {
                                    let (is_ag_bundled_srv, is_app_exe) =
                                        ppid1_ag_or_app_check_flags(row.exe);
                                    if is_ag_bundled_srv || is_app_exe {
                                        let started_str: &str = if let Some(s) =
                                            probe::format_lstart_stack(
                                                row.lstart,
                                                &mut stack_started,
                                            ) {
                                            s
                                        } else {
                                            let [w0, w1, w2, w3, w4] = row.lstart;
                                            heap_started
                                                .insert(probe::join_five(w0, w1, w2, w3, w4))
                                        };
                                        let in_known =
                                            Identity::in_set(known_helpers, row.pid, started_str);
                                        if in_known {
                                            has_uncached_orphan_candidate = true;
                                        } else {
                                            let started_key = probe::pack_lstart(row.lstart);
                                            let mut resolved = false;
                                            if let Some(sk) = started_key {
                                                if let Ok(idx) = cache
                                                    .launchd
                                                    .binary_search_by_key(&row.pid, |&(p, _)| p)
                                                    && cache.launchd[idx].1 == sk
                                                {
                                                    WatchCache::mark_seen(&mut launchd_seen, idx);
                                                    cached_kept_launchd.push(row.pid);
                                                    resolved = true;
                                                } else if is_app_exe
                                                    && let Ok(idx) = cache
                                                        .non_mcp
                                                        .binary_search_by_key(&row.pid, |entry| {
                                                            entry.pid
                                                        })
                                                {
                                                    let entry = cache.non_mcp[idx];
                                                    if entry.started_key == sk
                                                        && entry.exe_hash == hash_exe(row.exe)
                                                    {
                                                        WatchCache::mark_seen(
                                                            &mut non_mcp_seen,
                                                            idx,
                                                        );
                                                        cached_non_mcp_apps.push(row.pid);
                                                        resolved = true;
                                                    }
                                                }
                                            }
                                            if !resolved {
                                                if launchd_ready {
                                                    if launchd_raw
                                                        .as_slice()
                                                        .binary_search(&row.pid)
                                                        .is_ok()
                                                    {
                                                        if let Some(sk) = started_key {
                                                            new_launchd_entries.push((row.pid, sk));
                                                        }
                                                    } else {
                                                        has_uncached_orphan_candidate = true;
                                                    }
                                                } else {
                                                    if run_initial_launchd {
                                                        ps_saw_before_launchd = true;
                                                    }
                                                    has_uncached_orphan_candidate = true;
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                let started_str: &str = if let Some(s) =
                                    probe::format_lstart_stack(row.lstart, &mut stack_started)
                                {
                                    s
                                } else {
                                    let [w0, w1, w2, w3, w4] = row.lstart;
                                    heap_started.insert(probe::join_five(w0, w1, w2, w3, w4))
                                };
                                let in_known =
                                    Identity::in_set(known_helpers, row.pid, started_str);
                                let is_base_candidate = owners::is_orphan_base_candidate_fields(
                                    row.ppid,
                                    row.uid,
                                    row.has_tty,
                                    row.age_secs,
                                    row.exe,
                                    user,
                                );
                                if in_known {
                                    if is_base_candidate {
                                        has_uncached_orphan_candidate = true;
                                    }
                                } else if owners::is_opencode_pty_exe(row.exe) {
                                    if is_base_candidate {
                                        has_opencode_pty_candidate = true;
                                    }
                                } else if has_distinctive && is_base_candidate {
                                    let started_key = probe::pack_lstart(row.lstart);
                                    if let Some(sk) = started_key {
                                        if let Ok(idx) = cache
                                            .launchd
                                            .binary_search_by_key(&row.pid, |&(p, _)| p)
                                            && cache.launchd[idx].1 == sk
                                        {
                                            WatchCache::mark_seen(&mut launchd_seen, idx);
                                            return;
                                        }
                                        if let Ok(idx) = cache
                                            .non_mcp
                                            .binary_search_by_key(&row.pid, |entry| entry.pid)
                                        {
                                            let entry = cache.non_mcp[idx];
                                            if entry.started_key == sk
                                                && entry.exe_hash == hash_exe(row.exe)
                                            {
                                                WatchCache::mark_seen(&mut non_mcp_seen, idx);
                                                return;
                                            }
                                        }
                                    }
                                    if launchd_ready {
                                        if launchd_raw.as_slice().binary_search(&row.pid).is_ok() {
                                            if let Some(sk) = started_key {
                                                new_launchd_entries.push((row.pid, sk));
                                            }
                                            return;
                                        }
                                    } else if run_initial_launchd {
                                        ps_saw_before_launchd = true;
                                    }
                                    has_uncached_orphan_candidate = true;
                                } else {
                                    // Unrelated `ppid <= 1` process (TTY, < grace period,
                                    // shell, Claude.app, or no distinctive servers).
                                    return;
                                }
                            }
                        }

                        let started = if let Some(ref owned) = heap_started {
                            snapshot.intern(owned)
                        } else if let Some(s) =
                            probe::format_lstart_stack(row.lstart, &mut stack_started)
                        {
                            snapshot.intern(s)
                        } else {
                            let [w0, w1, w2, w3, w4] = row.lstart;
                            let owned = probe::join_five(w0, w1, w2, w3, w4);
                            snapshot.intern(&owned)
                        };
                        let exe = snapshot.intern(row.exe);
                        snapshot.processes.push(Process {
                            pid: row.pid,
                            ppid: row.ppid,
                            uid: row.uid,
                            started,
                            age_secs: row.age_secs,
                            has_tty: row.has_tty,
                            tty_id: row.tty_id,
                            cpu_centis: row.cpu_centis,
                            footprint_bytes: None,
                            exe,
                            args: StrSpan::EMPTY,
                        });
                    }),
                    _ => {}
                },
            })?;

            if ps_parsed == 0 || ps_parsed * 10 < ps_non_empty * 9 {
                return Err(AppError::Unreadable {
                    program: "/bin/ps".into(),
                });
            }
            if !snapshot.processes.is_sorted_by_key(|process| process.pid) {
                snapshot
                    .processes
                    .sort_unstable_by_key(|process| process.pid);
            }

            if include_user_idle && user_idle_secs.is_none() {
                // Depth-4 probe missed IOHIDSystem; fall back to full class search.
                let _ = run_lines(
                    "/usr/sbin/ioreg",
                    &["-r", "-c", "IOHIDSystem", "-d", "1"],
                    |bytes| {
                        if user_idle_secs.is_none() {
                            with_line_str(bytes, |line| {
                                user_idle_secs = probe::hid_idle_secs_line(line);
                            });
                        }
                    },
                );
            }

            cache.prune_launchd(&launchd_seen);
            cache.prune_non_mcp(&non_mcp_seen);

            let filter_launchd_processes =
                |snapshot: &mut Snapshot, jobs: &[i32], out: &mut Vec<(i32, u32)>| {
                    let arena = &snapshot.strings;
                    snapshot.processes.retain(|process| {
                        let exe = process.exe.slice(arena);
                        let started = process.started.slice(arena);
                        if jobs.binary_search(&process.pid).is_err() {
                            return true;
                        }
                        if owners::is_agent_or_app_candidate_exe(exe) {
                            // Keep desktop apps and agent runtimes in `snapshot.processes`
                            // so dynamic discovery and parent-walking see them, while
                            // caching launchd membership for the exact subset that
                            // `SLOT_PS` checks on steady-state passes.
                            let (is_ag_bundled_srv, is_app_exe) = ppid1_ag_or_app_check_flags(exe);
                            if (is_ag_bundled_srv || is_app_exe)
                                && process.ppid == 1
                                && process.uid == user
                                && !process.has_tty
                                && process.age_secs >= owners::ORPHAN_MIN_AGE_SECS
                                && !Identity::in_set(known_helpers, process.pid, started)
                                && let Some(sk) = pack_started_str(started)
                            {
                                out.push((process.pid, sk));
                            }
                            return true;
                        }
                        if owners::is_orphan_base_candidate_fields(
                            process.ppid,
                            process.uid,
                            process.has_tty,
                            process.age_secs,
                            exe,
                            user,
                        ) && !Identity::in_set(known_helpers, process.pid, started)
                        {
                            if let Some(sk) = pack_started_str(started) {
                                out.push((process.pid, sk));
                            }
                            return false;
                        }
                        true
                    });
                };

            if has_opencode_pty_candidate
                && !snapshot.processes.iter().any(|p| {
                    p.uid == user
                        && (owners::is_opencode_runtime_exe(snapshot.exe(p))
                            || owners::is_opencode_app_fallback_exe(snapshot.exe(p)))
                })
            {
                has_uncached_orphan_candidate = true;
            }

            let launchd_jobs = if run_initial_launchd {
                if launchd_ready {
                    let jobs = launchd_raw.as_slice();
                    if ps_saw_before_launchd {
                        filter_launchd_processes(&mut snapshot, jobs, &mut new_launchd_entries);
                    }
                    cache.insert_launchd_batch(new_launchd_entries);
                    if has_distinctive {
                        cache.launchd_seeded = true;
                    }
                    Some(retain_snapshot_launchd_jobs(&snapshot, jobs))
                } else {
                    None
                }
            } else if has_uncached_orphan_candidate {
                let fresh = fetch_launchd_pids();
                if let Some(ref jobs_buf) = fresh {
                    let jobs = jobs_buf.as_slice();
                    filter_launchd_processes(&mut snapshot, jobs, &mut new_launchd_entries);
                    cache.insert_launchd_batch(new_launchd_entries);
                    if has_distinctive {
                        cache.launchd_seeded = true;
                    }
                    Some(retain_snapshot_launchd_jobs(&snapshot, jobs))
                } else {
                    None
                }
            } else {
                cached_kept_launchd.sort_unstable();
                cached_kept_launchd.dedup();
                Some(cached_kept_launchd)
            };

            cached_non_mcp_apps.sort_unstable();
            cached_non_mcp_apps.dedup();
            let candidate_pids = arg_candidate_pids(
                &snapshot,
                user,
                has_distinctive,
                has_pool_signatures,
                known_helpers,
                launchd_jobs.as_deref(),
                &cached_non_mcp_apps,
            );
            let new_non_mcp = fetch_args_and_classify_orphans(
                &mut snapshot,
                &candidate_pids,
                user,
                known_helpers,
                launchd_jobs.as_deref(),
                &mcp_servers,
            )?;
            cache.insert_non_mcp_batch(new_non_mcp);

            if !cache.non_mcp.is_empty() {
                cache.non_mcp_passes = cache.non_mcp_passes.saturating_add(1);
            }
            cache.last_process_count = snapshot.processes.len();
            cache.last_strings_len = snapshot.strings.len();

            snapshot.taken_at = unix_now();
            snapshot.pressure = pressure;
            snapshot.swap = swap;
            snapshot.user_idle_secs = user_idle_secs;
            snapshot.launchd_jobs = launchd_jobs;
            Ok(snapshot)
        }

        fn user_idle_secs(&self, _snapshot: &Snapshot) -> Option<u64> {
            query_user_idle_secs()
        }

        fn codex_activity_at(
            &self,
            _snapshot: &Snapshot,
            early_exit_after: Option<u64>,
        ) -> Option<u64> {
            // Fast path: when only checking freshness against `cutoff`, stat the
            // previously newest rollout file before walking ~/.codex/sessions.
            if let Some(cutoff) = early_exit_after
                && let Some(path) = self.last_rollout.borrow().as_deref()
                && let Some(secs) = file_mtime_secs(path)
                && secs > cutoff
            {
                return Some(secs);
            }
            match codex_activity_walk(&self.codex_sessions, early_exit_after) {
                Some((secs, path)) => {
                    *self.last_rollout.borrow_mut() = Some(path);
                    Some(secs)
                }
                None => {
                    *self.last_rollout.borrow_mut() = None;
                    None
                }
            }
        }

        fn antigravity_activity_at(
            &self,
            snapshot: &Snapshot,
            early_exit_after: Option<u64>,
        ) -> Option<u64> {
            self.antigravity_activity_at_dir(
                snapshot,
                owners::DEFAULT_ANTIGRAVITY_DATA_DIR,
                early_exit_after,
            )
        }

        fn antigravity_activity_at_dir(
            &self,
            _snapshot: &Snapshot,
            data_dir: &str,
            early_exit_after: Option<u64>,
        ) -> Option<u64> {
            if let Some(cutoff) = early_exit_after
                && let Some(path) = self.last_antigravity_files.borrow().get(data_dir)
                && let Some(secs) = file_mtime_secs(path)
                && secs > cutoff
            {
                return Some(secs);
            }
            let dir_path = self.gemini_dir.join(data_dir);
            match antigravity_activity_walk(&dir_path, early_exit_after) {
                Some((secs, path)) => {
                    self.last_antigravity_files
                        .borrow_mut()
                        .insert(CompactString::from(data_dir), path);
                    Some(secs)
                }
                None => {
                    self.last_antigravity_files.borrow_mut().remove(data_dir);
                    None
                }
            }
        }

        fn opencode_activity_at(&self, _snapshot: &Snapshot) -> Option<u64> {
            let db_path = self.opencode_data.join("opencode.db");
            let wal_path = self.opencode_data.join("opencode.db-wal");
            let db_stamp = FileStamp::of(&db_path);
            let wal_stamp = FileStamp::of(&wal_path);
            let db_ts = {
                let cached = self.opencode_activity_cache.borrow();
                if let Some((prev_db, prev_wal, prev_ts)) = *cached
                    && prev_db == db_stamp
                    && prev_wal == wal_stamp
                {
                    prev_ts
                } else {
                    drop(cached);
                    let queried = query_opencode_db_activity(&db_path);
                    *self.opencode_activity_cache.borrow_mut() =
                        Some((db_stamp, wal_stamp, queried));
                    queried
                }
            };
            let prompt_ts = file_mtime_secs(&self.opencode_state.join("prompt-history.jsonl"));
            match (db_ts, prompt_ts) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            }
        }

        fn resolve_footprints(&self, snapshot: &mut Snapshot) -> Result<(), AppError> {
            let mut in_table = false;
            run_lines(
                "/usr/bin/top",
                &["-l", "1", "-F", "-R", "-stats", "pid,mem"],
                |bytes| {
                    with_line_str(bytes, |line| {
                        if !in_table {
                            if line.trim_start().starts_with("PID") {
                                in_table = true;
                            }
                            return;
                        }
                        probe::apply_top_footprint_line(line, &mut snapshot.processes);
                    });
                },
            )?;
            Ok(())
        }

        fn snapshot(&self) -> Result<Snapshot, AppError> {
            let mut snapshot = self.begin_pass(&BTreeSet::new(), true)?;
            snapshot.codex_activity_at = self.codex_activity_at(&snapshot, None);
            let default_ag = self.antigravity_activity_at(&snapshot, None);
            snapshot.antigravity_activity_at = default_ag;
            snapshot.antigravity_activities.insert(
                CompactString::const_new(owners::DEFAULT_ANTIGRAVITY_DATA_DIR),
                default_ag,
            );
            let extra_dirs: Vec<CompactString> = snapshot
                .processes
                .iter()
                .filter(|p| owners::is_antigravity_server(&snapshot, p))
                .filter_map(|p| owners::extract_antigravity_data_dir(snapshot.args(p)))
                .filter(|&dir| dir != owners::DEFAULT_ANTIGRAVITY_DATA_DIR)
                .map(CompactString::from)
                .collect();
            for dir in extra_dirs {
                if !snapshot.antigravity_activities.contains_key(dir.as_str()) {
                    let ts = self.antigravity_activity_at_dir(&snapshot, &dir, None);
                    snapshot.antigravity_activities.insert(dir, ts);
                }
            }
            snapshot.opencode_activity_at = self.opencode_activity_at(&snapshot);
            self.resolve_footprints(&mut snapshot)?;
            Ok(snapshot)
        }

        fn identify(&self, pid: i32) -> Option<(CompactString, u32)> {
            if pid <= 0 {
                return None;
            }
            // Probe existence and ownership with `kill(pid, 0)` (~1 µs) before
            // spawning `/bin/ps` so already-exited or other-user PIDs cost zero
            // subprocesses.
            if let Err(Errno::ESRCH | Errno::EPERM) = kill(Pid::from_raw(pid), None) {
                return None;
            }
            let mut pid_buf = [0u8; 16];
            let pid_len = {
                let mut cursor = io::Cursor::new(&mut pid_buf[..]);
                write!(cursor, "{pid}").ok()?;
                cursor.position() as usize
            };
            let pid_arg = std::str::from_utf8(&pid_buf[..pid_len]).ok()?;
            let mut found = None;
            let mut extra_line = false;
            run_lines("/bin/ps", &["-o", "uid=,lstart=", "-p", pid_arg], |bytes| {
                with_line_str(bytes, |line| {
                    if line.trim().is_empty() {
                        return;
                    }
                    if found.is_some() {
                        extra_line = true;
                    } else {
                        found = probe::parse_identify_line(line);
                    }
                });
            })
            .ok()?;
            if extra_line { None } else { found }
        }

        fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
            // 0 and negative PIDs address process groups; 1 is launchd.
            if pid <= 1 {
                return Err(io::Error::other(format!("refusing to signal PID {pid}")));
            }
            let signal = match signal {
                Signal::Term => NixSignal::SIGTERM,
                Signal::Kill => NixSignal::SIGKILL,
            };
            kill(Pid::from_raw(pid), signal).map_err(io::Error::from)
        }

        fn launch_app(&self, bundle: &Path) -> io::Result<()> {
            run_discard(
                "/usr/bin/open",
                &["-g", "-j", "-a", &bundle.to_string_lossy()],
            )
            .map_err(|error| io::Error::other(error.to_string()))
        }

        fn sleep(&self, duration: Duration) {
            std::thread::sleep(duration);
        }

        fn now(&self) -> u64 {
            unix_now()
        }
    }

    /// Install or remove the per-user LaunchAgent that runs `watch`.
    pub fn install(home: &Path, uninstall: bool) -> Result<String, AppError> {
        if nix::unistd::geteuid().is_root() {
            return Err(AppError::Root);
        }
        let label = crate::install::LABEL;
        let plist = home.join(format!("Library/LaunchAgents/{label}.plist"));
        let domain = format!("gui/{}", getuid().as_raw());
        let service = format!("{domain}/{label}");
        let loaded = || run_discard("/bin/launchctl", &["print", &service]).is_ok();
        let io_error = AppError::Install;
        if uninstall {
            let stopped = if loaded() {
                run_discard("/bin/launchctl", &["bootout", &service])
            } else {
                Ok(())
            };
            match fs::remove_file(&plist) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
            // A failed bootout leaves the agent running until logout; say so
            // instead of "removed".
            stopped?;
            return Ok(format!("removed {label}"));
        }
        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(io_error)?;
        let contents = crate::install::plist(&exe, home);
        if fs::read_to_string(&plist).is_ok_and(|current| current == contents) && loaded() {
            // The binary may have been replaced in place by an upgrade.
            run_discard("/bin/launchctl", &["kickstart", "-k", &service])?;
            return Ok(format!(
                "{label} already installed; restarted {}",
                exe.display()
            ));
        }
        fs::create_dir_all(plist.parent().unwrap_or(home)).map_err(io_error)?;
        fs::create_dir_all(home.join("Library/Logs")).map_err(io_error)?;
        fs::write(&plist, contents).map_err(io_error)?;
        if loaded() {
            let _ = run_discard("/bin/launchctl", &["bootout", &service]);
        }
        run_discard(
            "/bin/launchctl",
            &["bootstrap", &domain, &plist.to_string_lossy()],
        )?;
        Ok(format!("installed {label}, running {}", exe.display()))
    }

    #[cfg(test)]
    mod tests {
        use std::{fs, time::Duration};

        use super::{
            LineSplitter, MacOs, arg_candidate_pids, codex_activity_at, run_lines,
            run_lines_with_timeout,
        };
        use crate::{actions::System, error::AppError, model::Snapshot};

        #[test]
        fn poll_runner_drains_large_pipe_and_enforces_timeout() {
            let mut total = 0usize;
            // > 128 KiB exceeds the 64 KiB macOS pipe buffer; must not deadlock.
            run_lines(
                "/bin/dd",
                &["if=/dev/zero", "bs=1024", "count=192"],
                |line| total += line.len(),
            )
            .unwrap();
            assert_eq!(total, 192 * 1024);

            // Failure exit status propagates as CommandFailed.
            let err = run_lines("/usr/bin/false", &[], |_| {}).unwrap_err();
            assert!(matches!(err, AppError::CommandFailed { .. }), "{err:?}");

            // Stalled command is killed and reported as CommandTimedOut.
            let err =
                run_lines_with_timeout("/bin/sleep", &["10"], Duration::from_millis(50), |_| {})
                    .unwrap_err();
            assert!(matches!(err, AppError::CommandTimedOut { .. }), "{err:?}");
        }

        #[test]
        fn line_splitter_handles_boundaries_crlf_and_trailing_fragment() {
            let mut splitter = LineSplitter::default();
            let mut lines = Vec::new();
            for chunk in [b"hel".as_slice(), b"lo\r\nwo", b"rld\nlast"] {
                splitter.push(chunk, |line| lines.push(line.to_vec()));
            }
            splitter.finish(|line| lines.push(line.to_vec()));
            assert_eq!(
                lines,
                vec![b"hello".to_vec(), b"world".to_vec(), b"last".to_vec()]
            );
        }

        #[test]
        fn codex_activity_respects_max_depth_and_early_exit() {
            let home = tempfile::tempdir().unwrap();
            let sessions = home.path().join(".codex/sessions");
            let day = sessions.join("2026/10/06");
            let too_deep = day.join("nested");
            fs::create_dir_all(&too_deep).unwrap();
            fs::write(day.join("other.jsonl"), b"{}\n").unwrap();
            fs::write(too_deep.join("rollout-deep.jsonl"), b"{}\n").unwrap();
            assert_eq!(codex_activity_at(&sessions, None), None);

            let rollout = day.join("rollout-1.jsonl");
            fs::write(&rollout, b"{}\n").unwrap();
            let mtime = codex_activity_at(&sessions, None).unwrap();
            assert!(mtime > 0);
            assert_eq!(
                codex_activity_at(&sessions, Some(mtime.saturating_sub(10))),
                Some(mtime)
            );

            // Cached fast path in `MacOs::codex_activity_at`: populates cache on
            // first call, hits cached path on second call, and recovers if the
            // cached rollout file is removed.
            let mac = MacOs::new(home.path().to_owned());
            let snap = Snapshot::default();
            assert_eq!(
                mac.codex_activity_at(&snap, Some(mtime.saturating_sub(10))),
                Some(mtime)
            );
            assert_eq!(
                mac.last_rollout.borrow().as_deref(),
                Some(rollout.as_path())
            );
            assert_eq!(
                mac.codex_activity_at(&snap, Some(mtime.saturating_sub(10))),
                Some(mtime)
            );
            fs::remove_file(&rollout).unwrap();
            assert_eq!(
                mac.codex_activity_at(&snap, Some(mtime.saturating_sub(10))),
                None
            );
            assert!(mac.last_rollout.borrow().is_none());
        }

        #[test]
        fn identify_handles_live_and_nonexistent_pids() {
            let home = tempfile::tempdir().unwrap();
            let mac = MacOs::new(home.path().to_owned());
            let self_pid = i32::try_from(std::process::id()).unwrap();
            let (started, uid) = mac.identify(self_pid).expect("current process is live");
            assert!(!started.is_empty());
            assert_eq!(uid, nix::unistd::getuid().as_raw());

            // Non-positive and non-existent PIDs short-circuit cleanly.
            assert_eq!(mac.identify(0), None);
            assert_eq!(mac.identify(-1), None);
            assert_eq!(mac.identify(i32::MAX), None);
        }

        #[test]
        fn mcp_cache_reloads_when_config_changes_and_recovers_after_open_error() {
            use std::os::unix::fs::PermissionsExt;

            let home = tempfile::tempdir().unwrap();
            let mac = MacOs::new(home.path().to_owned());
            assert!(mac.load_mcp_servers().0.is_empty());

            fs::create_dir_all(home.path().join(".codex")).unwrap();
            let codex_cfg = home.path().join(".codex/config.toml");
            fs::write(
                &codex_cfg,
                "[mcp_servers.r]\ncommand = \"railway\"\nargs = [\"mcp\"]\n",
            )
            .unwrap();

            // Transient unreadable file must not poison the cache even when
            // mtime/len/dev/ino stay identical after permissions are restored.
            fs::set_permissions(&codex_cfg, fs::Permissions::from_mode(0o000)).unwrap();
            assert!(mac.load_mcp_servers().0.is_empty());
            fs::set_permissions(&codex_cfg, fs::Permissions::from_mode(0o644)).unwrap();

            let (first, distinctive, pool_sigs, gen1) = mac.load_mcp_servers();
            assert_eq!(first.len(), 1);
            assert_eq!(first[0].command, "railway");
            assert!(distinctive);
            assert!(pool_sigs);
            // Cached hit returns identical data and same generation without re-cloning.
            let (again, _, _, gen_again) = mac.load_mcp_servers();
            assert_eq!(again, first);
            assert_eq!(gen_again, gen1);

            fs::write(
                home.path().join(".claude.json"),
                r#"{"mcpServers":{"g":{"command":"gopls","args":["mcp"]}}}"#,
            )
            .unwrap();
            let (second, _, _, gen2) = mac.load_mcp_servers();
            assert_eq!(second.len(), 2);
            assert_ne!(gen2, gen1);

            // Antigravity, OpenCode, project .mcp.json, and installed Claude plugin configs are discovered too.
            fs::create_dir_all(home.path().join(".gemini/antigravity")).unwrap();
            fs::write(
                home.path().join(".gemini/antigravity/mcp_config.json"),
                r#"{"mcpServers":{"cg":{"command":"codegraph","args":["serve","--mcp"]}}}"#,
            )
            .unwrap();
            fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
            fs::write(
                home.path().join(".config/opencode/opencode.json"),
                r#"{"mcp":{"oc":{"type":"local","command":["uvx","mcp-server-git"]}}}"#,
            )
            .unwrap();
            let proj = home.path().join("my-proj");
            fs::create_dir_all(&proj).unwrap();
            fs::write(
                proj.join(".mcp.json"),
                r#"{"mcpServers":{"nd":{"command":"npx","args":["-y","next-devtools-mcp@latest"]}}}"#,
            )
            .unwrap();
            let plugin_dir = home.path().join("my-plugin");
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{"pw":{"command":"npx","args":["@playwright/mcp@latest"]}}"#,
            )
            .unwrap();
            fs::create_dir_all(home.path().join(".claude/plugins")).unwrap();
            fs::write(
                home.path().join(".claude/plugins/installed_plugins.json"),
                format!(
                    r#"{{"version":2,"plugins":{{"pw@m":[{{"installPath":"{}"}}]}}}}"#,
                    plugin_dir.display()
                ),
            )
            .unwrap();
            fs::write(
                home.path().join(".claude.json"),
                format!(
                    r#"{{"mcpServers":{{"g":{{"command":"gopls","args":["mcp"]}}}},"projects":{{"{}":{{"mcpServers":{{}}}}}}}}"#,
                    proj.display()
                ),
            )
            .unwrap();

            let codex_plugin = home
                .path()
                .join(".codex/plugins/cache/openai-curated-remote/creative-production/0.1.25");
            fs::create_dir_all(&codex_plugin).unwrap();
            fs::write(
                codex_plugin.join(".mcp.json"),
                r#"{"mcpServers":{"creative_production_mcp":{"command":"node","args":["./mcp/server.bundle.mjs"]}}}"#,
            )
            .unwrap();

            let (all, _, has_pool, gen3) = mac.load_mcp_servers();
            assert!(has_pool);
            assert_ne!(gen3, gen2);
            let cmds: Vec<&str> = all.iter().map(|s| s.command.as_str()).collect();
            assert!(cmds.contains(&"railway"), "{cmds:?}");
            assert!(cmds.contains(&"gopls"), "{cmds:?}");
            assert!(cmds.contains(&"codegraph"), "{cmds:?}");
            assert!(cmds.contains(&"uvx"), "{cmds:?}");
            assert!(cmds.contains(&"npx"), "{cmds:?}");
            assert!(cmds.contains(&"node"), "{cmds:?}");
            assert_eq!(all.len(), 7, "{all:?}");
        }

        #[test]
        fn arg_candidate_pids_skips_launchd_jobs_and_handles_missing_launchd() {
            let mut snap = crate::owners::tests::machine();
            snap.processes.sort_unstable_by_key(|p| p.pid);
            let known = crate::owners::tests::known_gopls();
            let launchd = snap.launchd_jobs.as_deref();

            // Launchd jobs (300, 301) and known_helpers (402) are skipped; Codex
            // runtime (210), its non-bundled helpers (211, 212), and non-launchd
            // orphan candidate (400) are included.
            let pids = arg_candidate_pids(&snap, snap.user, true, true, &known, launchd, &[]);
            assert_eq!(pids, [210, 211, 212, 400]);

            // When launchctl fails (`None`), orphan candidates are skipped entirely
            // while Codex runtime/helper candidates are still collected.
            let pids_no_launchd =
                arg_candidate_pids(&snap, snap.user, true, true, &known, None, &[]);
            assert_eq!(pids_no_launchd, [210, 211, 212]);

            // Generic-only MCP configs (`has_distinctive == false, has_pool_signatures == true`)
            // still inspect runtime helper args so generic-launcher MCP servers match pools!
            let pids_pool_only =
                arg_candidate_pids(&snap, snap.user, false, true, &known, launchd, &[]);
            assert_eq!(pids_pool_only, [210, 211, 212]);

            // TTY-attached `codex exec` runtime (500) with inherited-TTY MCP child (501),
            // plus a `ppid == 1` Antigravity app executable (502) that might be a sidecar.
            snap.push_process(
                500,
                1,
                snap.user,
                "start-500",
                300,
                true,
                0,
                None,
                "/Users/u/.nvm/bin/codex",
                "",
            );
            snap.push_process(
                501,
                500,
                snap.user,
                "start-501",
                300,
                true,
                0,
                None,
                "/opt/homebrew/bin/node",
                "",
            );
            snap.push_process(
                502,
                1,
                snap.user,
                "start-502",
                300,
                false,
                0,
                None,
                "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
                "",
            );
            snap.processes.sort_unstable_by_key(|p| p.pid);
            let pids_tty = arg_candidate_pids(
                &snap,
                snap.user,
                false,
                true,
                &known,
                snap.launchd_jobs.as_deref(),
                &[],
            );
            assert!(pids_tty.contains(&500), "{pids_tty:?}");
            assert!(
                pids_tty.contains(&501),
                "inherited-TTY MCP helper must have args fetched: {pids_tty:?}"
            );
            assert!(
                pids_tty.contains(&502),
                "ppid==1 Antigravity exe must have args fetched for sidecar check: {pids_tty:?}"
            );
            // Already-verified non-sidecar `.app` cached in `cached_non_mcp_apps` is skipped!
            let pids_cached = arg_candidate_pids(
                &snap,
                snap.user,
                false,
                true,
                &known,
                snap.launchd_jobs.as_deref(),
                &[502],
            );
            assert!(
                !pids_cached.contains(&502),
                "cached non-sidecar app must not re-fetch args: {pids_cached:?}"
            );
        }

        #[test]
        fn watch_cache_seeds_on_first_pass_and_invalidates_on_config_or_ttl() {
            let home = tempfile::tempdir().unwrap();
            fs::create_dir_all(home.path().join(".codex")).unwrap();
            let codex_cfg = home.path().join(".codex/config.toml");
            fs::write(
                &codex_cfg,
                "[mcp_servers.r]\ncommand = \"railway\"\nargs = [\"mcp\"]\n",
            )
            .unwrap();
            let mac = MacOs::new(home.path().to_owned());
            let known = std::collections::BTreeSet::new();

            // Pass 1 seeds launchd and non-MCP caches.
            let snap1 = mac.begin_pass(&known, false).unwrap();
            assert!(snap1.launchd_jobs.is_some());
            let (launchd_count_1, non_mcp_count_1, passes_1, gen_1) = {
                let cache = mac.watch_cache.borrow();
                assert!(cache.launchd_seeded);
                (
                    cache.launchd.len(),
                    cache.non_mcp.len(),
                    cache.non_mcp_passes,
                    cache.non_mcp_generation,
                )
            };
            assert!(
                launchd_count_1 > 0 || non_mcp_count_1 > 0,
                "live macOS host should seed launchd or non-MCP ppid==1 entries"
            );

            // Pass 2 reuses the caches without rebuilding from scratch.
            let snap2 = mac.begin_pass(&known, false).unwrap();
            assert!(snap2.launchd_jobs.is_some());
            {
                let cache = mac.watch_cache.borrow();
                assert!(cache.launchd_seeded);
                assert_eq!(cache.non_mcp_generation, gen_1);
                if non_mcp_count_1 > 0 {
                    assert!(cache.non_mcp_passes > passes_1);
                }
            }

            // Updating MCP config bumps generation and refreshes non-MCP cache.
            fs::write(
                home.path().join(".claude.json"),
                r#"{"mcpServers":{"g":{"command":"gopls","args":["mcp"]}}}"#,
            )
            .unwrap();
            let _snap3 = mac.begin_pass(&known, false).unwrap();
            {
                let cache = mac.watch_cache.borrow();
                assert_ne!(cache.non_mcp_generation, gen_1);
                assert!(cache.non_mcp_passes <= 1);
            }

            // Clearing MCP configs drops `has_distinctive`: full daemon seeding
            // resets (`launchd_seeded == false`), non-app daemons are pruned,
            // while live `.app` launchd entries remain cached so subsequent
            // `!has_distinctive` passes skip repeated `launchctl list` calls.
            fs::remove_file(&codex_cfg).unwrap();
            fs::remove_file(home.path().join(".claude.json")).unwrap();
            let snap4 = mac.begin_pass(&known, false).unwrap();
            assert!(snap4.launchd_jobs.is_some());
            let app_launchd_count = {
                let cache = mac.watch_cache.borrow();
                assert!(!cache.launchd_seeded);
                assert!(
                    cache.launchd.len() <= launchd_count_1,
                    "non-app launchd daemons should be pruned when !has_distinctive"
                );
                cache.launchd.len()
            };
            // Second pass without distinctive configs reuses cached `.app` launchd entries.
            let snap4b = mac.begin_pass(&known, false).unwrap();
            assert!(snap4b.launchd_jobs.is_some());
            {
                let cache = mac.watch_cache.borrow();
                assert!(!cache.launchd_seeded);
                assert_eq!(cache.launchd.len(), app_launchd_count);
            }

            fs::write(
                &codex_cfg,
                "[mcp_servers.r]\ncommand = \"railway\"\nargs = [\"mcp\"]\n",
            )
            .unwrap();
            let snap5 = mac.begin_pass(&known, false).unwrap();
            assert!(snap5.launchd_jobs.is_some());
            assert!(mac.watch_cache.borrow().launchd_seeded);
        }

        #[test]
        fn drain_group_handles_required_failure_optional_failure_and_timeouts() {
            use super::{GroupEvent, GroupSlot, drain_group};

            // 1. Required child exits non-zero while optional sibling stalls:
            // must abort promptly instead of waiting for the optional child's timeout.
            let start = std::time::Instant::now();
            let mut slots = [
                Some(GroupSlot::spawn("/bin/sleep", &["10"], false, false).unwrap()),
                Some(GroupSlot::spawn("/usr/bin/false", &[], true, false).unwrap()),
            ];
            let err = drain_group(&mut slots, Duration::from_secs(5), |_, _| {}).unwrap_err();
            assert!(matches!(err, AppError::CommandFailed { .. }), "{err:?}");
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "required failure stalled on optional sibling: {:?}",
                start.elapsed()
            );

            // 2. Optional child exits non-zero while required child succeeds:
            // returns Ok, emits Eof(false) for optional and Line+Eof(true) for required.
            let mut slots = [
                Some(GroupSlot::spawn("/usr/bin/false", &[], false, true).unwrap()),
                Some(GroupSlot::spawn("/bin/echo", &["hello-world"], true, false).unwrap()),
            ];
            let mut opt_eof = None;
            let mut req_eof = None;
            let mut req_lines = Vec::new();
            drain_group(&mut slots, Duration::from_secs(5), |idx, event| {
                match (idx, event) {
                    (0, GroupEvent::Eof(ok)) => opt_eof = Some(ok),
                    (1, GroupEvent::Line(bytes)) => {
                        req_lines.push(String::from_utf8_lossy(bytes).into_owned());
                    }
                    (1, GroupEvent::Eof(ok)) => req_eof = Some(ok),
                    _ => {}
                }
            })
            .unwrap();
            assert_eq!(opt_eof, Some(false));
            assert_eq!(req_eof, Some(true));
            assert_eq!(req_lines, ["hello-world"]);

            // 3. Optional child times out while required child already succeeded:
            // kills stalled optional child, emits Eof(false), and returns Ok.
            let mut slots = [
                Some(GroupSlot::spawn("/bin/sleep", &["10"], false, false).unwrap()),
                Some(GroupSlot::spawn("/bin/echo", &["fast"], true, false).unwrap()),
            ];
            let mut opt_eof = None;
            let mut saw_fast = false;
            drain_group(&mut slots, Duration::from_millis(80), |idx, event| {
                match (idx, event) {
                    (0, GroupEvent::Eof(ok)) => opt_eof = Some(ok),
                    (1, GroupEvent::Line(b"fast")) => saw_fast = true,
                    _ => {}
                }
            })
            .unwrap();
            assert_eq!(opt_eof, Some(false));
            assert!(saw_fast);

            // 4. Required child times out alongside optional sibling -> CommandTimedOut.
            let mut slots = [
                Some(GroupSlot::spawn("/bin/sleep", &["10"], false, false).unwrap()),
                Some(GroupSlot::spawn("/bin/sleep", &["10"], true, false).unwrap()),
            ];
            let err = drain_group(&mut slots, Duration::from_millis(80), |_, _| {}).unwrap_err();
            assert!(matches!(err, AppError::CommandTimedOut { .. }), "{err:?}");
        }

        #[test]
        fn pid_set_buf_and_pid_list_handle_inline_and_heap_spill() {
            use super::{PidSetBuf, with_pid_list};

            let mut small = PidSetBuf::new();
            for pid in [30, 10, 20, 10, 30] {
                small.push(pid);
            }
            small.sort_and_dedup();
            assert_eq!(small.as_slice(), &[10, 20, 30]);

            // Exceed inline capacity (512) to verify seamless heap spill.
            let mut large = PidSetBuf::new();
            for pid in (0..600).rev() {
                large.push(pid);
                if pid % 100 == 0 {
                    large.push(pid);
                }
            }
            large.sort_and_dedup();
            assert_eq!(large.as_slice().len(), 600);
            assert_eq!(large.as_slice()[0], 0);
            assert_eq!(large.as_slice()[599], 599);

            let short = with_pid_list(&[1, 22, 333], str::to_owned);
            assert_eq!(short, "1,22,333");

            let many: Vec<i32> = (100_000..100_700).collect();
            let long = with_pid_list(&many, str::to_owned);
            assert!(long.len() > 4096);
            assert!(long.starts_with("100000,100001,"));
            assert!(long.ends_with(",100699"));
        }

        #[test]
        fn antigravity_and_opencode_activity_probes_read_latest_mtimes() {
            let home = tempfile::tempdir().unwrap();
            let mac = MacOs::new(home.path().to_owned());
            let snap = Snapshot::default();
            assert_eq!(mac.antigravity_activity_at(&snap, None), None);
            assert_eq!(mac.opencode_activity_at(&snap), None);

            let conv_dir = home.path().join(".gemini/antigravity/conversations");
            fs::create_dir_all(&conv_dir).unwrap();
            let conv = conv_dir.join("abc.db-wal");
            fs::write(&conv, b"wal").unwrap();
            let ag_mtime = mac.antigravity_activity_at(&snap, None).unwrap();
            assert!(ag_mtime > 0);
            assert_eq!(
                mac.antigravity_activity_at(&snap, Some(ag_mtime.saturating_sub(10))),
                Some(ag_mtime)
            );

            // Real SQLite OpenCode DB in WAL mode: a 3-hour-old session turn must
            // NOT be masked by fresh background `kv` writes to `opencode.db-wal`
            // or fresh watcher entries in `opencode.log`.
            let oc_dir = home.path().join(".local/share/opencode");
            let oc_log_dir = oc_dir.join("log");
            fs::create_dir_all(&oc_log_dir).unwrap();
            let db_path = oc_dir.join("opencode.db");
            let session_ts_secs: u64 = 1_795_700_000;
            let session_ts_ms: u64 = session_ts_secs * 1000 + 544;
            let init_sql = format!(
                "PRAGMA journal_mode=WAL;\
                 CREATE TABLE session_v2 (id TEXT PRIMARY KEY, time_updated INTEGER);\
                 CREATE TABLE session_message (id TEXT PRIMARY KEY, time_created INTEGER);\
                 CREATE TABLE session_part (id TEXT PRIMARY KEY, time_updated INTEGER);\
                 CREATE TABLE kv (key TEXT PRIMARY KEY, val TEXT);\
                 INSERT INTO session_v2 VALUES ('s1', {session_ts_ms});\
                 INSERT INTO session_message VALUES ('m1', {session_ts_ms});\
                 INSERT INTO kv VALUES ('models-dev:v3:catalog', 'tick');"
            );
            let status = std::process::Command::new("/usr/bin/sqlite3")
                .arg(&db_path)
                .arg(&init_sql)
                .status()
                .expect("macOS ships /usr/bin/sqlite3");
            assert!(status.success());
            fs::write(oc_log_dir.join("opencode.log"), b"watcher restarted\n").unwrap();

            assert_eq!(mac.opencode_activity_at(&snap), Some(session_ts_secs));
            // Repeated call hits stamp cache and returns the exact same timestamp.
            assert_eq!(mac.opencode_activity_at(&snap), Some(session_ts_secs));
        }

        #[test]
        fn classify_args_line_interns_renamed_clone_server_and_orphaned_sidecars() {
            use std::collections::BTreeSet;

            use super::classify_args_line;

            let mut snap = Snapshot {
                user: 501,
                ..Snapshot::default()
            };
            // 1. Renamed Antigravity clone `language_server.real` at `ppid == 1` (no "antigravity" in exe).
            snap.push_process(
                600,
                1,
                501,
                "Tue Oct  6 12:00:00 2026",
                900,
                false,
                0,
                None,
                "/Users/u/Applications/MyCustomIDE.app/Contents/Resources/bin/language_server.real",
                "",
            );
            // 2. Orphaned `sidecar_sdk` at `ppid == 1` from a renamed clone.
            snap.push_process(
                601,
                1,
                501,
                "Tue Oct  6 12:00:00 2026",
                900,
                false,
                0,
                None,
                "/Users/u/Applications/MyCustomIDE.app/Contents/MacOS/MyCustomIDE.real",
                "",
            );
            // 3. Orphaned `sidecar_sdk` at `ppid == 1` from `Antigravity US.app`.
            snap.push_process(
                602,
                1,
                501,
                "Tue Oct  6 12:00:00 2026",
                900,
                false,
                0,
                None,
                "/Users/u/Applications/Antigravity US.app/Contents/MacOS/Antigravity.real",
                "",
            );
            // 4. Non-launchd regular `.app` at `ppid == 1` (not a sidecar) -> cached in `non_mcp`.
            snap.push_process(
                603,
                1,
                501,
                "Tue Oct  6 12:00:00 2026",
                900,
                false,
                0,
                None,
                "/Applications/SomeEditor.app/Contents/MacOS/SomeEditor",
                "",
            );
            // 5. Unrelated non-MCP daemon at `ppid == 1` -> cached in `non_mcp`, args not interned.
            snap.push_process(
                604,
                1,
                501,
                "Tue Oct  6 12:00:00 2026",
                900,
                false,
                0,
                None,
                "/usr/local/bin/unrelated-daemon",
                "",
            );

            let known = BTreeSet::new();
            let launchd: &[i32] = &[];
            let mcp = [crate::model::ServerSignature {
                command: "railway".into(),
                args: smallvec::smallvec!["mcp".into()],
            }];
            let mut non_mcp = Vec::new();

            classify_args_line(
                &mut snap,
                "600 language_server.real --override_ide_name antigravity --app_data_dir my-clone --host_bridge_url=http://127.0.0.1:51982",
                501,
                &known,
                Some(launchd),
                &mcp,
                &mut non_mcp,
            );
            classify_args_line(
                &mut snap,
                "601 MyCustomIDE.real --import=file:///Users/u/.gemini/my-clone/lib/node_modules/sidecar_sdk/register_loader.js main.mjs",
                501,
                &known,
                Some(launchd),
                &mcp,
                &mut non_mcp,
            );
            classify_args_line(
                &mut snap,
                "602 Antigravity.real --import=file:///Users/u/.gemini/antigravity/lib/node_modules/sidecar_sdk/register_loader.js main.mjs",
                501,
                &known,
                Some(launchd),
                &mcp,
                &mut non_mcp,
            );
            classify_args_line(
                &mut snap,
                "603 SomeEditor --new-window",
                501,
                &known,
                Some(launchd),
                &mcp,
                &mut non_mcp,
            );
            classify_args_line(
                &mut snap,
                "604 unrelated-daemon --serve",
                501,
                &known,
                Some(launchd),
                &mcp,
                &mut non_mcp,
            );

            assert!(
                snap.args(&snap.processes[0])
                    .contains("--override_ide_name antigravity")
            );
            assert!(snap.args(&snap.processes[1]).contains("sidecar_sdk"));
            assert!(snap.args(&snap.processes[2]).contains("sidecar_sdk"));
            assert!(snap.args(&snap.processes[3]).is_empty());
            assert!(snap.args(&snap.processes[4]).is_empty());
            let non_mcp_pids: Vec<i32> = non_mcp.iter().map(|e| e.pid).collect();
            assert_eq!(non_mcp_pids, [603, 604]);

            // Ownership::new now classifies 600, 601, and 602 as orphans and skips 603/604!
            snap.launchd_jobs = Some(Vec::new());
            let ownership = crate::owners::Ownership::new(&snap, &known);
            assert_eq!(ownership.orphans.as_slice(), [600, 601, 602]);
        }

        #[test]
        fn arg_candidate_pids_covers_renamed_sidecar_and_generic_ide_cli_mcp_helpers() {
            let mut snap = Snapshot {
                user: 501,
                launchd_jobs: Some(vec![1000]),
                mcp_servers: std::sync::Arc::from([
                    crate::model::ServerSignature {
                        command: "npx".into(),
                        args: smallvec::smallvec!["-y".into(), "@playwright/mcp@latest".into()],
                    },
                    crate::model::ServerSignature {
                        command: "/Users/u/.local/bin/codegraph".into(),
                        args: smallvec::smallvec!["serve".into(), "--mcp".into()],
                    },
                ]),
                ..Snapshot::default()
            };
            // Renamed clone orphaned sidecar at `ppid == 1` (not in launchd_jobs).
            snap.push_process(
                902,
                1,
                501,
                "start-902",
                500,
                false,
                0,
                None,
                "/Users/u/Applications/MyCustomIDE.app/Contents/MacOS/MyCustomIDE.real",
                "",
            );
            // Live Cursor.app (in launchd_jobs) -> Helper -> node Playwright MCP.
            snap.push_process(
                1000,
                1,
                501,
                "start-1000",
                5000,
                false,
                0,
                None,
                "/Applications/Cursor.app/Contents/MacOS/Cursor",
                "",
            );
            snap.push_process(
                1001,
                1000,
                501,
                "start-1001",
                5000,
                false,
                0,
                None,
                "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin).app/Contents/MacOS/Cursor Helper (Plugin)",
                "",
            );
            snap.push_process(
                1002,
                1001,
                501,
                "start-1002",
                5000,
                false,
                0,
                None,
                "/opt/homebrew/bin/node",
                "",
            );
            // Unrelated child binary under Cursor (`clang`) must NOT be fetched.
            snap.push_process(
                1003,
                1001,
                501,
                "start-1003",
                5000,
                false,
                0,
                None,
                "/usr/bin/clang",
                "",
            );
            // Standalone `gemini` CLI -> `node` codegraph helper.
            snap.push_process(
                2001,
                1,
                501,
                "start-2001",
                5000,
                true,
                0,
                None,
                "/opt/homebrew/bin/gemini",
                "",
            );
            snap.push_process(
                2002,
                2001,
                501,
                "start-2002",
                5000,
                true,
                0,
                None,
                "/Users/u/.codegraph/versions/v1.6.4/node",
                "",
            );

            let pids = arg_candidate_pids(
                &snap,
                501,
                true,
                true,
                &std::collections::BTreeSet::new(),
                snap.launchd_jobs.as_deref(),
                &[],
            );
            assert!(
                pids.contains(&902),
                "renamed clone sidecar candidate: {pids:?}"
            );
            assert!(
                !pids.contains(&1000),
                "launchd GUI app Cursor skipped: {pids:?}"
            );
            assert!(
                pids.contains(&1002),
                "Cursor generic node MCP helper: {pids:?}"
            );
            assert!(!pids.contains(&1003), "unrelated clang skipped: {pids:?}");
            assert!(pids.contains(&2002), "gemini CLI MCP helper: {pids:?}");
        }

        #[test]
        fn per_dir_antigravity_activity_and_gemini_plugin_and_cli_configs_work() {
            let home = tempfile::tempdir().unwrap();
            let mac = MacOs::new(home.path().to_owned());
            let snap = Snapshot::default();

            // Separate `--app_data_dir` directories track independently, including `history.jsonl`.
            fs::create_dir_all(home.path().join(".gemini/antigravity/conversations")).unwrap();
            fs::create_dir_all(home.path().join(".gemini/antigravity-cli")).unwrap();
            fs::create_dir_all(home.path().join(".gemini/custom-clone")).unwrap();
            fs::write(
                home.path().join(".gemini/antigravity/conversations/a.pb"),
                b"ag",
            )
            .unwrap();
            fs::write(
                home.path()
                    .join(".gemini/antigravity-cli/jetbox_summaries_proto.pb"),
                b"cli",
            )
            .unwrap();
            fs::write(
                home.path().join(".gemini/custom-clone/history.jsonl"),
                b"{}\n",
            )
            .unwrap();

            assert!(
                mac.antigravity_activity_at_dir(&snap, "antigravity", None)
                    .is_some()
            );
            assert!(
                mac.antigravity_activity_at_dir(&snap, "antigravity-cli", None)
                    .is_some()
            );
            assert!(
                mac.antigravity_activity_at_dir(&snap, "custom-clone", None)
                    .is_some()
            );
            assert_eq!(
                mac.antigravity_activity_at_dir(&snap, "antigravity-ide", None),
                None
            );

            // MCP discovery includes `~/.gemini/antigravity-cli/mcp_config.json`,
            // custom `~/.gemini/<clone>/mcp_config.json`, `~/.gemini/config/plugins/<plugin>/`,
            // `~/.cursor/mcp.json`, `~/.config/zed/settings.json`, VS Code `mcp.json`,
            // and ignores non-MCP `hooks` in `~/.gemini/settings.json`.
            fs::write(
                home.path().join(".gemini/antigravity-cli/mcp_config.json"),
                r#"{"mcpServers":{"cg":{"command":"codegraph","args":["serve","--mcp"]}}}"#,
            )
            .unwrap();
            fs::write(
                home.path().join(".gemini/custom-clone/mcp_config.json"),
                r#"{"mcpServers":{"cl":{"command":"custom-mcp","args":["serve"]}}}"#,
            )
            .unwrap();
            fs::write(
                home.path().join(".gemini/settings.json"),
                r#"{"hooks":{"pre":{"command":"rtk","args":["hook"]}}}"#,
            )
            .unwrap();
            let gemini_plugin = home.path().join(".gemini/config/plugins/my-mcp-plugin");
            fs::create_dir_all(&gemini_plugin).unwrap();
            fs::write(
                gemini_plugin.join(".mcp.json"),
                r#"{"mcpServers":{"plug":{"command":"npx","args":["-y","@playwright/mcp@latest"]}}}"#,
            )
            .unwrap();
            let gemini_plugin2 = home.path().join(".gemini/config/plugins/inline-plugin");
            fs::create_dir_all(&gemini_plugin2).unwrap();
            fs::write(
                gemini_plugin2.join("plugin.json"),
                r#"{"name":"inline","mcpServers":{"r":{"command":"railway","args":["mcp"]}}}"#,
            )
            .unwrap();
            fs::create_dir_all(home.path().join(".cursor")).unwrap();
            fs::write(
                home.path().join(".cursor/mcp.json"),
                r#"{"mcpServers":{"cur":{"command":"uvx","args":["mcp-server-git"]}}}"#,
            )
            .unwrap();
            fs::create_dir_all(home.path().join(".config/zed")).unwrap();
            fs::write(
                home.path().join(".config/zed/settings.json"),
                r#"{"context_servers":{"z":{"command":{"path":"zed-mcp","args":["--stdio"]}}}}"#,
            )
            .unwrap();
            fs::create_dir_all(home.path().join("Library/Application Support/Code/User")).unwrap();
            fs::write(
                home.path()
                    .join("Library/Application Support/Code/User/mcp.json"),
                r#"{"servers":{"v":{"type":"stdio","command":"vscode-mcp","args":["run"]}}}"#,
            )
            .unwrap();

            let (servers, _, _, gen1) = mac.load_mcp_servers();
            let cmds: Vec<&str> = servers.iter().map(|s| s.command.as_str()).collect();
            assert!(cmds.contains(&"codegraph"), "{cmds:?}");
            assert!(cmds.contains(&"custom-mcp"), "{cmds:?}");
            assert!(cmds.contains(&"npx"), "{cmds:?}");
            assert!(cmds.contains(&"railway"), "{cmds:?}");
            assert!(cmds.contains(&"uvx"), "{cmds:?}");
            assert!(cmds.contains(&"zed-mcp"), "{cmds:?}");
            assert!(cmds.contains(&"vscode-mcp"), "{cmds:?}");
            assert!(
                !cmds.contains(&"rtk"),
                "hooks in settings.json must not be parsed as MCP servers: {cmds:?}"
            );

            // Adding a brand-new `mcp_config.json` inside an already-existing plugin
            // subdirectory without modifying `~/.gemini/config/plugins/` itself must
            // still invalidate `McpCache` on the very next pass!
            fs::write(
                gemini_plugin2.join("mcp_config.json"),
                r#"{"mcpServers":{"late":{"command":"late-plugin-mcp","args":["--mcp"]}}}"#,
            )
            .unwrap();
            let (servers2, _, _, gen2) = mac.load_mcp_servers();
            assert_ne!(gen2, gen1);
            assert!(
                servers2.iter().any(|s| s.command == "late-plugin-mcp"),
                "newly added config inside existing plugin subdir must be discovered: {servers2:?}"
            );
        }
    }
}

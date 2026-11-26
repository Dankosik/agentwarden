# Changelog

Notable changes to agentwarden. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). The release workflow publishes the
section of the tagged version as its release notes.

## [Unreleased]

## [0.2.0] - 2026-11-27

Dynamic multi-agent discovery across Antigravity (including renamed/proxy-wrapped
clones such as `Antigravity US.app`), OpenCode, Codex (desktop apps, CLI, and
nested subagents), Claude Code (desktop and terminal CLI), and any other coding
IDE/CLI parenting configured MCP servers, plus reliability fixes under heavy
memory pressure.

### Added

- **Dynamic desktop app discovery (`desktop_apps[]` in JSON and `status`):**
  Discovers Codex (`ChatGPT.app`, `Codex.app`, clones), Antigravity
  (`Antigravity.app`, `Antigravity US.app`, and arbitrarily renamed clones),
  OpenCode (`OpenCode.app`), and generic MCP-hosting IDEs/apps (`other`, plus
  Claude Desktop direct MCPs) by walking up from live runtimes and MCP helpers
  rather than hardcoding app bundle paths.
- **Standalone CLI, service, and nested subagent attribution (`agent_sessions[]`
  in JSON and `status`):** Tracks `codex` / `codex exec`, `opencode`,
  `opencode-cli serve --service`, `agy`, and generic MCP-hosting CLIs, including
  nested subagent runs (`parent_agent`, e.g. Antigravity or Claude Code invoking
  `codex`), recording their MCP helpers in `known_helpers` for orphan reaping
  and blocking parent restarts/stops while a subagent is active.
- **Rule `idle-antigravity-restart` (`kind: "restart-app"`):** Gracefully quits
  an idle Antigravity desktop app (plus its `language_server` if it lingers) and
  relaunches the exact `.app` bundle in the background, tracking each instance's
  real `--app_data_dir` (`~/.gemini/<name>`) independently.
- **Rules `idle-opencode-restart` (`kind: "restart-app"`) and
  `idle-opencode-service` (`kind: "stop-graceful"`):** Gracefully stops a
  headless `opencode-cli serve --service` (SIGTERM only, never SIGKILL) or
  restarts an idle `OpenCode.app` using real SQLite session/message timestamps
  from `opencode.db` + `prompt-history.jsonl` (ignoring 5-minute background `kv`
  WAL ticks and `opencode.log`).
- **Expanded orphan reaping:** Safely reaps orphaned Antigravity `sidecar_sdk`
  processes, provably orphaned desktop-bundled Antigravity `language_server`
  processes (`ppid == 1` with `--host_bridge_url=`), and childless `opencode-pty`
  processes when no OpenCode client or service is alive.
- **Expanded MCP configuration discovery:** Reads project `.mcp.json`,
  `.codex/config.toml`, and `opencode.json[c]`; Claude and Codex plugin
  `.mcp.json` caches; `claude_desktop_config.json`; `~/.gemini/antigravity*`
  and custom `~/.gemini/<clone>/mcp_config.json` plus `~/.gemini/config/plugins/`;
  `~/.config/opencode/opencode.json[c]` (including nested `"mcp.servers"`);
  Zed `context_servers`; and VS Code `mcp.json`.

### Changed

- **Non-fatal `/usr/bin/top` degradation in `reclaim` and `watch`:** A `top`
  timeout or failure under heavy swap logs a warning and continues without
  footprints so `orphan`, `idle-claude-session`, pressure-idle
  `idle-opencode-service`, and `state.observe()` still run (while footprint-gated
  restarts conservatively skip).
- **LaunchAgent QoS:** Removed `ProcessType=Background` and `LowPriorityIO`
  (keeping `Nice=10`) from the LaunchAgent plist so Darwin QoS throttling does
  not starve `ps`/`top` during heavy swapping.
- **Terminal `claude` CLI support in `idle-claude-session`:** Reclaims configured
  stdio MCP helper trees of idle TTY-attached `claude` CLI sessions on the exact
  inherited controlling TTY while continuing to protect shells, interactive
  terminal programs (`vim`, `fzf`, `less`, `tmux`, etc.), VCS utilities, other
  TTYs, and any orphan with a TTY.

### Fixed

- **Codex restart blocked by generic-launcher MCPs and external helpers:**
  Generic launchers with arguments (`npx`, `pnpx`, `bunx`, `uvx`, `pipx`, `node`,
  `python3`, `python3.x`, macOS Framework `Python`, `docker`, `npm exec`, scoped
  npm packages such as `@playwright/mcp@latest`) and external runtime helpers
  (`codex-code-mode-host`, `node_repl`, `~/.codex/computer-use/`,
  `~/.cache/codex-runtimes/`, `~/.codex/plugins/`) now classify as MCP pools
  instead of running commands.
- **False-positive `git` orphan stops:** VCS and system utilities (`git`,
  `git-fsmonitor--daemon`, `ssh`, `gpg`, `watchman`, `caffeinate`, etc.) are
  never added to `known_helpers` and never stopped by `orphan` or
  `idle-claude-session`.
- **Framework and browser bundle false protection:** Framework-embedded `.app`
  bundles (such as Homebrew `Python.framework/.../Python.app`) and headless
  browsers (such as Playwright `Chromium.app`) are no longer misclassified as
  protected agent host apps.

## [0.1.3] - 2026-11-26

Substantial CPU and memory optimizations for both the background `watch`
daemon and interactive `status`/`reclaim` runs, without changing the CLI,
JSON schema, or reclamation rules.

### Changed

- Reduced sleeping `watch` physical memory footprint from ~3.8 MB to ~1.1 MB
  (cutting dirty `MALLOC_SMALL` pages by >95 %) through streaming line-by-line
  subprocess parsing, stack-buffered SIMD-accelerated (`memchr`) TOML/JSON
  MCP config readers, a flat `Snapshot` string arena, inline `CompactString`
  and `SmallVec` storage, minimal probe environments, and skipping `clap` /
  `stdout` initialization on bare `agentwarden watch`.
- Reduced `status` wall time and CPU by ~10× and steady-state `watch` CPU by
  ~75× by running system probes concurrently in a single `poll` loop,
  resolving `top` footprints, `ioreg` idle time, and `~/.codex/sessions`
  walks lazily only when needed, caching unchanged `state.json`, MCP configs,
  `launchd` jobs, and verified non-MCP `ppid == 1` processes across `watch`
  passes, short-circuiting `identify` with `kill(pid, 0)`, and skipping
  redundant `state.json` rewrites when nothing changed.
- Enabled fat LTO, single codegen unit, `panic = "abort"`, and symbol
  stripping for release builds.

### Fixed

- `top` PID architecture suffixes (`*`, `+`, `-`, such as Rosetta 2
  processes on Apple Silicon) are now parsed instead of silently dropping
  process footprints.
- Transient open errors when reading `~/.codex/config.toml`,
  `~/.claude.json`, or saving `state.json` no longer poison in-memory caches
  across `watch` passes.
- `ChatGPT.app` without active Codex runtimes and Claude Code sessions
  without helpers no longer trigger unnecessary `top`, `ioreg`, or
  `~/.codex/sessions` probes.

## [0.1.2] - 2026-10-06

### Fixed

- When ChatGPT quits later than the 60-second wait of an idle Codex restart,
  or opening it again fails, later passes open it again once it is gone, for
  up to 10 minutes. 0.1.1 left it closed.

### Added

- Releases carry `install.sh`, and the README installs with the script of
  the latest release, so the script and the binary come from one version.
- Each archive has a GitHub build provenance attestation:
  `gh attestation verify <archive> --repo Dankosik/agentwarden`. Releases are
  immutable, published from a draft, and only from a tag on `main`.

## [0.1.1] - 2026-10-06

Safety fixes from an independent review. Upgrading is recommended: 0.1.0 could
stop work in progress.

### Fixed

- The Codex restart no longer interrupts a long command. Any shell,
  `codex exec`, or other command under the app blocks it however long it has
  run; a shell that runs one command execs it, so 0.1.0 took such a command
  for an MCP helper.
- The Codex restart needs Codex to hold at least 1 GB, or 512 MB under memory
  pressure, counting the runtimes themselves; 0.1.0 could restart the app for
  a few megabytes whenever pressure was raised.
- A helper whose tree contains an agent, a shell, or a terminal process is
  never stopped, by any rule.
- A configured MCP server with a generic launcher (`node`, `npx`, `python`,
  `uvx`, …) or no arguments no longer makes a matching detached process an
  orphan; only helpers seen serving an agent are. Only helper roots are
  remembered, so a daemon a helper detached on purpose is not an orphan.
- After the Mac sleeps, or any gap between passes, idle time starts again
  instead of counting the sleep; 0.1.0 could stop servers the moment the user
  came back.
- A Claude Code session with a process it started within the idle window, or
  any running shell, is working;
  the idle CPU tolerance follows the real interval between passes.
- Swap filling its allocated size no longer means critical pressure; macOS
  keeps allocated swap nearly full.
- System commands run in US English, so `status` and `reclaim` read the
  machine correctly under any system language; an unreadable process table
  fails the pass instead of looking empty.
- System commands time out after 30 seconds instead of stalling the watcher.
- State is saved before the action log, and a log write failure no longer
  fails the pass; a corrupted log line no longer hides the rest. An older
  state file keeps its values.
- Zombie processes are skipped; when `launchctl` cannot list jobs, the orphan
  rule waits for the next pass; PIDs 0 and 1 are never signalled;
  agentwarden refuses to run as root.
- `install --uninstall` removes the LaunchAgent and reports an error if the
  agent could not be stopped; `install` refuses to run as root.

### Added

- `status` shows what the Codex runtimes hold and the commands running under
  the app; JSON `codex.footprint_bytes` and `codex.commands`.
- `install.sh` adds the install directory to `PATH` in the shell's startup
  file when it is missing, as rustup and uv do;
  `AGENTWARDEN_NO_MODIFY_PATH=1` skips that.

## [0.1.0] - 2026-10-06

First release, for macOS on Apple Silicon and Intel.

### Added

- `agentwarden status`: memory pressure, swap, Claude Code sessions and their
  MCP servers, the Codex app and its pools, orphaned helpers, and what the
  rules would do now, as text or `--format json`.
- `agentwarden reclaim [--dry-run]`: one pass of the rules now.
- `agentwarden watch` and `agentwarden install [--uninstall]`: a per-user
  LaunchAgent that applies the rules every 60 seconds at background priority.
- Rule `orphan`: stops helpers whose agent session has exited.
- Rule `idle-claude-session`: stops the MCP servers of a Claude Code session
  idle for 60 minutes under warning memory pressure, 15 under critical.
- Rule `idle-codex-restart`: quits and relaunches the ChatGPT app in the
  background when Codex and the user have been idle for 30 minutes and its
  helpers hold 1 GB or memory is under pressure; never forced, at most once
  every 6 hours.
- An action log in `~/Library/Application Support/agentwarden/actions.jsonl`.
- `install.sh`: install or upgrade from GitHub Releases with checksum
  verification.

[Unreleased]: https://github.com/Dankosik/agentwarden/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Dankosik/agentwarden/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/Dankosik/agentwarden/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Dankosik/agentwarden/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Dankosik/agentwarden/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Dankosik/agentwarden/releases/tag/v0.1.0

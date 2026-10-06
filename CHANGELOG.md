# Changelog

Notable changes to agentwarden. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). The release workflow publishes the
section of the tagged version as its release notes.

## [Unreleased]

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
- A Claude Code session that starts a process, or runs any shell, is working;
  the idle CPU tolerance follows the real interval between passes.
- Swap filling its allocated size no longer means critical pressure; macOS
  keeps allocated swap nearly full.
- System commands run in the C locale, so `status` and `reclaim` read the
  machine correctly under any system language; an unreadable process table
  fails the pass instead of looking empty.
- System commands time out after 30 seconds instead of stalling the watcher.
- State is saved before the action log, and a log write failure no longer
  fails the pass; a corrupted log line no longer hides the rest. An older
  state file keeps its values.
- Zombie processes are skipped; when `launchctl` cannot list jobs, the orphan
  rule waits for the next pass; PIDs 0 and 1 are never signalled;
  agentwarden refuses to run as root.
- `install --uninstall` reports an error if the agent could not be stopped.

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

[Unreleased]: https://github.com/Dankosik/agentwarden/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/Dankosik/agentwarden/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Dankosik/agentwarden/releases/tag/v0.1.0

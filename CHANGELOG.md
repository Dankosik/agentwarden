# Changelog

Notable changes to agentwarden. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). The release workflow publishes the
section of the tagged version as its release notes.

## [Unreleased]

### Added

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

[Unreleased]: https://github.com/Dankosik/agentwarden/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Dankosik/agentwarden/releases/tag/v0.1.0

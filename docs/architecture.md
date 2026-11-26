# Architecture

agentwarden is one synchronous program. A pass samples the machine, decides,
acts, and records; `watch` repeats a pass every 60 seconds. There is no async
runtime, daemon framework, or configuration file. The
[specification](../specs/agentwarden/README.md) owns behavior and the safety
contract; this file owns where each responsibility lives.

## Responsibility boundaries

| File | Owns |
| --- | --- |
| `src/main.rs` | Thin executable entry point |
| `src/lib.rs` | Argument parsing, dispatch, the watch loop, final exit status |
| `src/cli.rs` | Commands, flags, help, and the source for completions |
| `src/platform.rs` | Every macOS call behind the `System` trait: `ps`, `top`, `sysctl`, `ioreg`, `launchctl`, read-only `sqlite3`, signals, `open`, MCP config/plugin discovery, per-`--app_data_dir` activity probes, watch caches, and the LaunchAgent install |
| `src/probe.rs` | Pure parsers for those commands' output (`ps`, `tty_id`, `top`, `sysctl`, `ioreg`, `launchctl`) and for agent MCP configurations (TOML, JSON/JSONC, OpenCode) |
| `src/model.rs` | The snapshot: processes (with compact `StrSpan` arena and `tty_id`), `AgentKind`, `SessionMode`, pressure, swap, idle time, per-agent activity maps, MCP `ServerSignature` matching (including package runners) |
| `src/owners.rs` | Dynamic ownership tree: Claude Code sessions, desktop apps (Codex, Antigravity and renamed clones, OpenCode, and generic MCP-hosting `other` IDEs), CLI/service/subagent sessions, inherited-TTY safety checks, and orphans (including `sidecar_sdk`, orphaned desktop `language_server`, and childless `opencode-pty`) |
| `src/state.rs` | What passes remember: known helpers, Claude session CPU activity, last swap, per-bundle app restart timestamps, and pending late-quit relaunches |
| `src/rules.rs` | The 6 rules (`orphan`, `idle-claude-session`, `idle-codex-restart`, `idle-antigravity-restart`, `idle-opencode-restart`, `idle-opencode-service`), lazy `IdleProbeNeeds`, and constants, as pure functions of snapshot, ownership and state |
| `src/actions.rs` | Revalidation, `Stop`, `StopGraceful`, desktop app restart sequences (`RestartCodex`, `RestartApp`), and the `System` trait |
| `src/store.rs` | State file, stamp-validated state cache, and the bounded action log |
| `src/report.rs` | One pass (`pass`), lazy watch pass (`watch_pass_cached`), and text/JSON report rendering |
| `src/install.rs` | The LaunchAgent property list |
| `src/output.rs` | Text or JSON on stdout, and output-write failures |
| `src/error.rs` | Error types and diagnostic rendering |
| `tests/cli.rs` | The built binary: grammar, status, framing |
| `install.sh` | Install or upgrade from GitHub Releases: architecture, checksum, binary, `PATH` in the shell's startup file, then `agentwarden install` |
| `scripts/release.py` | Release archives, checksums, and release notes from `CHANGELOG.md` |

## Data flow

1. `platform` builds a `Snapshot` via `begin_pass` (streaming `ps` + `sysctl`,
   plus lazy/cached `launchctl` and targeted `ps -o args=`). A `/bin/ps`
   failure aborts the pass; optional signals (pressure, swap, HID idle, launchd
   jobs, disk/SQLite activity) degrade to defaults that make rules more
   conservative. In `watch` and `reclaim`, `/usr/bin/top` footprint probes run
   only when needed and degrade non-fatally so stop rules and session
   observation keep running while footprint-gated restarts conservatively skip.
2. `owners` turns the flat process list into owners: dynamically discovered
   desktop apps (including renamed clones), standalone CLI/service/subagent
   sessions, generic `other` MCP hosts, and verified orphans. Agent hosts are
   never `Stop` targets (aside from provably orphaned desktop Antigravity
   `language_server`s).
3. `rules::plan` reads the snapshot, ownership and the previous state, and
   returns actions. It has no side effects.
4. `actions::execute` revalidates each target by PID and start time and acts.
5. `state.observe` records this pass (including `known_helpers` across all
   discovered agents); `store` saves state atomically when changed and appends
   the action records.

`status` and `reclaim --dry-run` stop after step 3 and write nothing.

## Testing

Rules, ownership, state, parsers and the executor are tested on recorded or
synthetic snapshots and a fake `System`, so tests run on every platform and
never signal real processes. `tests/cli.rs` runs the binary with a temporary
`HOME`; on macOS it reads the real process table but never applies actions or
installs the LaunchAgent. Live checks on the owner's machine are recorded in
the [research](../specs/agentwarden/research.md).

## Platforms

Only macOS is implemented. Other platforms compile, and every command that
needs the system exits with `agentwarden supports macOS only`. A port adds a
`System` implementation and its probes; rules do not change.

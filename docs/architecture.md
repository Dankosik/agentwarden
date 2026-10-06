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
| `src/platform.rs` | Every macOS call behind the `System` trait: `ps`, `top`, `sysctl`, `ioreg`, `launchctl`, signals, `open`, and the LaunchAgent install |
| `src/probe.rs` | Pure parsers for those commands' output and for the agents' MCP configuration |
| `src/model.rs` | The snapshot: processes, pressure, swap, idle time, Codex activity, MCP server signatures |
| `src/owners.rs` | Ownership tree: Claude Code sessions and their helpers, Codex runtimes and the app, orphans |
| `src/state.rs` | What passes remember: known helpers, session CPU activity, last swap, last Codex restart |
| `src/rules.rs` | The rules and their constants, as pure functions of snapshot, ownership and state |
| `src/actions.rs` | Revalidation, signalling, the Codex restart sequence, and the `System` trait |
| `src/store.rs` | State file and the bounded action log |
| `src/report.rs` | One pass (`pass`) and the report it prints |
| `src/install.rs` | The LaunchAgent property list |
| `src/output.rs` | Text or JSON on stdout, and output-write failures |
| `src/error.rs` | Error types and diagnostic rendering |
| `tests/cli.rs` | The built binary: grammar, status, framing |
| `install.sh` | Install or upgrade from GitHub Releases: architecture, checksum, binary, then `agentwarden install` |
| `scripts/release.py` | Release archives, checksums, and release notes from `CHANGELOG.md` |

## Data flow

1. `platform` builds a `Snapshot`. Commands that fail fatally (`ps`, `top`)
   abort the pass; optional signals (pressure, swap, idle, launchd jobs)
   degrade to defaults that make rules more conservative.
2. `owners` turns the flat process list into owners. Agent hosts are never
   targets.
3. `rules::plan` reads the snapshot, ownership and the previous state, and
   returns actions. It has no side effects.
4. `actions::execute` revalidates each target by PID and start time and acts.
5. `state.observe` records this pass; `store` saves state atomically and
   appends the action records.

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

# agentwarden: implementation plan

Date: 2026-10-06. Source: [specification](README.md) and [research](research.md).
Each task is one reviewable change that ends with `make check` passing. Stages
follow the specification's delivery stages; a later stage starts after the
earlier one's completion evidence is recorded.

## Shape

One synchronous binary. Modules, by responsibility:

| Module | Owns |
| --- | --- |
| `cli.rs` | Commands `status`, `reclaim`, `watch`, `install`; replaces the template's `stats` |
| `snapshot.rs` | One sample of the machine: processes, footprint, CPU time, pressure, swap, HID idle, Codex rollout activity |
| `platform/macos.rs` | Every macOS call: `ps`, `libproc`, `sysctl`, `ioreg`, signals, `launchctl`, app quit and relaunch |
| `owners.rs` | Ownership tree: agent apps, Claude Code sessions, Codex app-server, helpers, orphans |
| `rules.rs` | The four rules and pressure levels, as pure functions of snapshots and history |
| `actions.rs` | Revalidation, signalling, restart sequence, results |
| `log.rs` | Action log (JSON Lines) and the history file the rules read |
| `output.rs` | Text and the documented `--json` schema |

Rules never call the platform module. They take recorded snapshots, so every
rule is tested against fixtures captured on the owner's machine.

Paths: state and the action log under `~/Library/Application Support/agentwarden/`,
the LaunchAgent at `~/Library/LaunchAgents/io.github.dankosik.agentwarden.plist`.

### Dependencies

| Need | Choice | Reason |
| --- | --- | --- |
| Footprint, CPU time, BSD info (PPID, UID, start time) | `libproc` | R4; safe API, keeps `unsafe_code = "forbid"` |
| Signals | `nix` with the `signal` feature | Safe `kill`/`killpg`; std has no signal API |
| Command lines | `ps -axww -o pid=,args=` | `libproc` has no argument vector; one call per sample |
| Pressure, swap, HID idle | `sysctl -n kern.memorystatus_vm_pressure_level`, `sysctl -n vm.swapusage`, `ioreg -c IOHIDSystem` | Checked readable without root on 2026-10-06 |
| Codex MCP server commands | `toml` (present) on `~/.codex/config.toml` | Pool membership |
| Claude MCP server commands | `serde_json` (present) on Claude Code's MCP configuration | Orphan signatures |
| Plist | Handwritten XML template | One fixed document; no crate needed |

The template's predeclared toolbox stays. New crates enter only with the task
that first uses them.

## Stage 1: Observe

| Task | Change | Depends | Done when |
| --- | --- | --- | --- |
| T1 | Replace `stats` with the new command skeleton; `status` prints an empty report; README states the agent install path | — | Help, version, completions, and exit status tests pass for the new grammar |
| T2 | `snapshot.rs` + `platform/macos.rs`: processes (PID, PPID, UID, start time, TTY, args), footprint and CPU time, pressure level, swap, HID idle, newest rollout mtime | T1 | A captured snapshot of the owner's machine round-trips through JSON; unreadable processes are counted, not fatal |
| T3 | `owners.rs`: Claude Code sessions, Codex app-server, helpers, orphans; helper signatures from both agents' MCP configs plus Codex bundled helpers | T2 | On a fixture captured from the owner's machine, the owner counts match a manual `ps` analysis of the same capture. Fixtures keep the process tree, executable names and server kinds, and drop user paths and other arguments before they are committed to this public repository |
| T4 | `status` text and `--json` with the documented schema: pressure, owner tree with footprint and idle time, recent actions | T3 | Schema test; manual comparison with `ps` recorded in the PR |

## Stage 2: Orphans

| Task | Change | Depends | Done when |
| --- | --- | --- | --- |
| T5 | `log.rs`: action log and history file with bounded size | T2 | Appends survive a crash mid-write; old entries are trimmed |
| T6 | `actions.rs`: revalidate PID by start time and args, SIGTERM, grace, SIGKILL, process group when owned; refuse TTY, other UID, agent apps | T5 | Tests against a spawned child: stopped; a recycled PID (start time changed) is skipped |
| T7 | Orphan rule and `reclaim [--dry-run]` | T3, T6 | Fixture with the 19 post-restart orphans plans exactly those; a live run on the owner's machine logs them and their recovered footprint |

## Stage 3: Install and watch

| Task | Change | Depends | Done when |
| --- | --- | --- | --- |
| T8 | `watch`: 60 s loop, one snapshot per pass, rules, actions, history; exits cleanly on SIGTERM | T7 | A two-pass run against fixtures applies rules once per pass |
| T9 | `install [--uninstall]`: write the plist, `launchctl bootstrap gui/$UID`, idempotent; refuse when another copy runs | T8 | Install, repeat, uninstall on the owner's machine with `launchctl print` evidence |
| T10 | Agent-first README section: one command to install and verify, the `status --json` fields, what each rule does | T9 | An agent following only the README installs and reports status |

## Stage 4: Idle Claude sessions

| Task | Change | Depends | Done when |
| --- | --- | --- | --- |
| T11 | Measure a waiting Claude Code session's CPU growth over 30 minutes; set the "not grown" tolerance in code | T2 | Measurement recorded in research |
| T12 | Pressure levels and the idle-session rule (Warning 60 min, Critical 15 min, Normal off) | T8, T11 | Fixture tests per level; an active session is never selected |
| T13 | Live check on the Claude Code version in use: stop an idle session's servers, next tool call succeeds | T12 | Recorded; a failing check disables the rule by version |

## Stage 5: Idle Codex restart

| Task | Change | Depends | Done when |
| --- | --- | --- | --- |
| T14 | Observe an idle Codex for one hour: rollout writes, processes started under the app-server | T2 | Recorded; activity signal stays quiet while idle, or the pool filter is adjusted |
| T15 | Find a graceful quit without a permission prompt (SIGTERM to the app first, AppleScript as fallback); confirm relaunch with `open -g -j` and that threads are listed again | — | Recorded on the owner's machine |
| T16 | Restart rule: Codex idle 30 min, HID idle 30 min, helpers ≥ 1 GB or pressure ≥ Warning, at most once per 6 h; sequence with 60 s quit timeout, never force | T8, T14, T15 | Fixture tests for each condition; refusal when the app does not quit |
| T17 | One live restart on the owner's machine | T16 | Swap and helper counts before and after, from the action log |

## Completion

Stages 2–5 together: one working day on the owner's machine with agentwarden
installed, compared with a day without it, from the action log and swap
samples. Then the task notes move into maintained documentation, as
`specs/README.md` asks.

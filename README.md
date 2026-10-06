# agentwarden

Keeps a Mac that runs many AI coding agents (Claude Code, Codex) from filling
its memory with idle agent helpers. It needs no configuration: an agent
installs it once, and it runs in the background from then on.

Status: macOS only. [Specification](specs/agentwarden/README.md) ·
[research](specs/agentwarden/research.md) · [plan](specs/agentwarden/plan.md) ·
[architecture](docs/architecture.md)

## For agents: install and verify

Run these from a checkout of this repository on the user's Mac. Rust comes
from [rustup](https://rust-lang.org/tools/install/); the checked-in toolchain
selects the version.

```sh
cargo install --path . --locked
agentwarden install
agentwarden status --format json
```

`install` writes `~/Library/LaunchAgents/io.github.dankosik.agentwarden.plist`
and starts it; it is idempotent and repeats safely after a rebuild. The agent
runs `agentwarden watch`, which applies the rules every 60 seconds at
background priority. Nothing asks the user anything, and there is no
configuration file.

To remove it: `agentwarden install --uninstall`.

## What it does

Every pass samples processes, their physical footprint (compressed and swapped
memory included, as `top` reports it), kernel memory pressure, swap, and how
long the user has been away. Then it applies four rules:

| Rule | Stops | When |
| --- | --- | --- |
| `orphan` | A helper whose agent session has exited (PPID 1), with its descendants | Always, once the helper is 2 minutes old |
| `idle-claude-session` | The MCP servers of a Claude Code session that has done nothing | Idle 60 min under warning pressure, 15 min under critical; never under normal pressure |
| `idle-codex-restart` | The ChatGPT app that hosts Codex, quit and relaunched in the background | Codex idle 30 min, user away 30 min, Codex helpers ≥ 1 GB or warning pressure, at most once per 6 hours |
| Codex helpers one by one | Never | Codex reports a stopped server as "not connected" until it refreshes |

Why these are safe: Claude Code starts a stopped MCP server again on the next
tool call; Codex saves threads to disk, so a restart loses no thread; an orphan
has no session left to use it. Measurements are in the
[research](specs/agentwarden/research.md).

Safety rules:

- A process is signalled only after its PID and start time are checked again,
  so a reused PID is never hit.
- Only the current user's processes, never one with a terminal, never a
  launchd service, never an agent itself.
- Children stop before parents: SIGTERM, then SIGKILL after 5 seconds.
- The app is only asked to quit; if it has not quit within 60 seconds, the
  restart is abandoned and recorded, never forced.

## Commands

| Command | Effect |
| --- | --- |
| `agentwarden status` | Who holds memory, what the rules would do now, recent actions. Writes nothing |
| `agentwarden reclaim [--dry-run]` | One pass now; `--dry-run` prints the plan only |
| `agentwarden watch` | The loop the LaunchAgent runs |
| `agentwarden install [--uninstall]` | Install or remove the LaunchAgent |
| `agentwarden completions <shell>` | Shell completion script |

`--format json` (or `AGENTWARDEN_FORMAT=json`) prints one JSON object:

| Field | Meaning |
| --- | --- |
| `taken_at` | Unix time of the sample |
| `pressure`, `kernel_pressure` | `normal`, `warning`, `critical`; `pressure` is the kernel level raised by swap trends, which the rules use |
| `swap` | `used_bytes`, `total_bytes` |
| `user_idle_secs` | Seconds since keyboard or pointer input |
| `claude_sessions[]` | `pid`, `idle_secs` (null until sampled twice), `helpers_footprint_bytes`, `helpers[]` |
| `codex` | `app_pid`, `idle_secs`, `pools_footprint_bytes`, `pools[]`; null when the app is not running |
| `orphans[]` | `pid`, `exe_name`, `footprint_bytes` (with descendants), `age_secs` |
| `planned[]` | Actions the rules choose now: `kind` `stop` or `restart-codex`, `rule`, targets, `footprint_bytes`, `reason` |
| `applied[]` | What this run did: `outcome` `stopped`, `skipped`, `failed`, `restarted` or `refused` |
| `recent[]` | Newest records of the action log |

Exit status: 0 success or nothing to do, 1 error (including an unsupported
platform), 2 usage error, 3 `reclaim` finished with at least one failed action.

Files: state and the action log (`actions.jsonl`) live in
`~/Library/Application Support/agentwarden/`; the LaunchAgent's errors go to
`~/Library/Logs/agentwarden.log`.

## Development

```sh
make check
cargo run --locked -- status
cargo run --locked -- reclaim --dry-run
```

Rules are pure functions of recorded snapshots and are tested without touching
the machine; tests never install the LaunchAgent. See
[contributing](CONTRIBUTING.md) and [agent workflow](docs/agent-workflow.md).

Built from [rust-cli-template](https://github.com/Dankosik/rust-cli-template).
Licensed under [MIT](LICENSE).

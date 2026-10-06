# agentwarden

[![CI](https://github.com/Dankosik/agentwarden/actions/workflows/ci.yml/badge.svg)](https://github.com/Dankosik/agentwarden/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Dankosik/agentwarden?sort=semver)](https://github.com/Dankosik/agentwarden/releases/latest)
[![Platform: macOS](https://img.shields.io/badge/platform-macOS-lightgrey)](#install)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Keeps a Mac that runs many AI coding agents (Claude Code, Codex) from filling
its memory with idle agent helpers. Install it once; it needs no configuration
and runs in the background from then on.

Every Claude Code session starts its own MCP servers, and the Codex app keeps a
pool per thread. They stay after the work is done, and helpers whose session
exited stay forever. On a 16 GB Mac with a dozen sessions that is gigabytes of
swap. On the author's machine, the first manual pass stopped 6 orphaned helper
trees holding 1.55 GB.

## Install

```sh
curl -fsSL https://github.com/Dankosik/agentwarden/releases/latest/download/install.sh | sh
```

The script downloads the latest release for your Mac (Apple Silicon or Intel),
checks it against `SHA256SUMS`, puts `agentwarden` in `~/.local/bin`, and
starts the background agent. If `~/.local/bin` is not on your `PATH`, it adds
one line to your shell's startup file (`~/.zshrc` for zsh) and asks you to open
a new terminal. Run the same command again to upgrade; it restarts the agent on
the new version. `AGENTWARDEN_VERSION=v0.1.0` pins a release,
`AGENTWARDEN_BIN_DIR` changes the directory, and `AGENTWARDEN_NO_MODIFY_PATH=1`
leaves startup files alone.

Then check it:

```console
$ agentwarden status
memory: normal pressure (kernel normal), swap 6.88 GB of 7.50 GB, user idle 0 min
claude code: 8 sessions, helpers 0.64 GB, idle over 60 min: 0
codex: app 85686, holds 6.15 GB, helpers 4.88 GB in 112 processes, running commands 0, idle 0 min
orphans: 0, 0.00 GB
planned: nothing to do
recent:
  9 min ago  Stopped  idle-claude-session  node  0.18 GB
  7 min ago  Stopped  orphan  node  0.32 GB
```

To remove it:

```sh
agentwarden install --uninstall && rm ~/.local/bin/agentwarden
```

and delete the line marked `# Added by the agentwarden installer` from your
shell's startup file if the installer added one.

The script comes from the latest release, together with the binary it
installs. Releases are immutable, and each archive carries a build provenance
attestation from GitHub Actions: with the GitHub CLI,
`gh attestation verify <archive> --repo Dankosik/agentwarden` proves it was
built by this repository's release workflow.

Other ways to install: download an archive from
[Releases](https://github.com/Dankosik/agentwarden/releases) and run
`./agentwarden install` from where you keep it, or build from source with
`cargo install --git https://github.com/Dankosik/agentwarden --locked` and then
`agentwarden install`.

### For agents

An agent asked to set this up runs the install command above, then
`agentwarden status --format json` and reports `version`, `pressure` and
`planned`. Nothing asks the user anything, and there is no configuration file.
`agentwarden install` writes
`~/Library/LaunchAgents/io.github.dankosik.agentwarden.plist` for the binary it
is run from and starts it; repeating it restarts the agent. The agent runs
`agentwarden watch`, which applies the rules every 60 seconds at background
priority.

## What it does

Every pass samples processes, their physical footprint (compressed and swapped
memory included, as `top` reports it), kernel memory pressure, swap, and how
long the user has been away. Then it applies four rules:

| Rule | Stops | When |
| --- | --- | --- |
| `orphan` | A helper whose agent session has exited (PPID 1), with its descendants | Always, once the helper is 2 minutes old; it must have been seen serving an agent, or match a configured MCP server by a specific program and arguments |
| `idle-claude-session` | The MCP servers of a Claude Code session that has done nothing: no CPU beyond its timers, no new process, no running shell | Idle 60 min under warning pressure, 15 min under critical; never under normal pressure; the clock restarts after the Mac sleeps |
| `idle-codex-restart` | The ChatGPT app that hosts Codex, quit and relaunched in the background | No Codex session write for 30 min and no command running under the app however long, user away 30 min, Codex holds ≥ 1 GB (≥ 512 MB under pressure), at most once per 6 hours |
| Codex helpers one by one | Never | Codex reports a stopped server as "not connected" until it refreshes |

Why these are safe: Claude Code starts a stopped MCP server again on the next
tool call; Codex saves threads to disk, so a restart loses no thread; an orphan
has no session left to use it. Measurements are in the
[research](specs/agentwarden/research.md).

Safety rules:

- A process is signalled only after its PID and start time are checked again,
  so a reused PID is never hit.
- Only the current user's processes, never one with a terminal, never a
  shell, never a launchd service, never an agent itself, and nothing whose
  tree contains one of those. It refuses to run as root.
- When a system command fails or prints something it cannot read, the pass
  stops instead of acting on a partial picture.
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
| `version` | agentwarden version |
| `taken_at` | Unix time of the sample |
| `pressure`, `kernel_pressure` | `normal`, `warning`, `critical`; `pressure` is the kernel level raised to `warning` while swap grows, which the rules use |
| `swap` | `used_bytes`, `total_bytes` |
| `user_idle_secs` | Seconds since keyboard or pointer input |
| `claude_sessions[]` | `pid`, `idle_secs` (null until sampled twice), `helpers_footprint_bytes`, `helpers[]` |
| `codex` | `app_pid`, `idle_secs`, `footprint_bytes` (what a restart reclaims), `pools_footprint_bytes`, `pools[]`, `commands[]` (running work that blocks a restart); null when the app is not running |
| `orphans[]` | `pid`, `exe_name`, `footprint_bytes` (with descendants), `age_secs` |
| `planned[]` | Actions the rules choose now: `kind` `stop` or `restart-codex`, `rule`, targets, `footprint_bytes`, `reason` |
| `applied[]` | What this run did: `outcome` `stopped`, `skipped`, `failed`, `restarted` or `refused` |
| `recent[]` | Newest records of the action log |

Exit status: 0 success or nothing to do, 1 error (including an unsupported
platform), 2 usage error, 3 `reclaim` finished with at least one failed action.

Files: state and the action log (`actions.jsonl`) live in
`~/Library/Application Support/agentwarden/`; the LaunchAgent's errors go to
`~/Library/Logs/agentwarden.log`.

## Versions

Releases follow [Semantic Versioning](https://semver.org/). The commands and
flags, the JSON fields, exit codes, file locations and the LaunchAgent label
are the public interface; before 1.0, a minor release may change them and says
so in the [changelog](CHANGELOG.md). `agentwarden --version` and the `version`
JSON field show what is installed.

## Development

```sh
make check
cargo run --locked -- status
cargo run --locked -- reclaim --dry-run
```

Rules are pure functions of recorded snapshots and are tested without touching
the machine; tests never install the LaunchAgent. See
[contributing](CONTRIBUTING.md), [architecture](docs/architecture.md),
[releasing](docs/releasing.md), and the
[specification](specs/agentwarden/README.md) with its
[research](specs/agentwarden/research.md).

Built from [rust-cli-template](https://github.com/Dankosik/rust-cli-template).
Licensed under [MIT](LICENSE).

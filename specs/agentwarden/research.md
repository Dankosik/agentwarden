# Research gates R1–R5

Date: 2026-10-06. Machine: macOS 26.4, Apple Silicon, 10 cores, 16 GB RAM.
Claude Code desktop 2.1.286; Codex desktop app with codex-cli 0.160.0.
Scripts ran from a scratch directory; their method is described with each
result.

The machine changed state during the research: the owner restarted the Codex
app after the definition was written. Swap use fell from 23.3 GB to 5.1 GB, and
the restart left 11 codegraph, 3 railway, 2 gopls, 2 node_repl and 1 cua-repl
processes orphaned (PPID 1). Restarting the app frees most leaked memory and
creates orphans, so orphan reaping stays a required rule.

## R1: stopping an idle session's MCP server

**Claude Code 2.1.286: recoverable.** Experiment on the researching session's
own `railway mcp` child, the only process touched:

1. SIGTERM to the child. It exited; no replacement appeared within 20 s.
2. The next `railway` tool call succeeded. Claude Code had started a new
   `railway mcp` child (new PID, start time at the call).
3. Repeated once with the same result.

Claude Code restarts a stdio server lazily, on the next call to one of its
tools. Stopping an idle server frees its memory; the cost is the server's
startup time on next use. A server that keeps state in its process, such as a
REPL (`node_repl`, `cua-repl`), loses that state.

**Codex 0.160.0: breaks the tool for the thread.** Source reading of
`openai/codex` at tag `rust-v0.160.0` and at main `588f616` (2026-10-06),
`codex-rs/codex-mcp/src/connection_manager.rs`:

- A tool call on a server whose connection is closed fails with
  `MCP server '<name>' is not connected`.
- A closed client is replaced only when the connection set is rebuilt.
  `reusable_client` rejects closed clients, so a rebuild starts a new process.
- A rebuild happens when the session's MCP runtime is marked dirty: a changed
  environment, changed authentication, a configuration reload
  (`config/mcpServer/reload`), an explicit refresh, or a thread reload. A dead
  server alone does not mark it dirty.

This was not run live: it would break a tool in one of the owner's threads.

**Decision.** Reclaiming idle servers under a live parent is allowed for Claude
Code sessions, excluding servers declared stateful. For Codex threads that are
still loaded it is not allowed. Codex waste is handled by orphan reaping after
an app restart, and by a multiplexer for configurable servers (R5).

## R2: attributing MCP processes to sessions

**Claude Code: per session.** Each session is its own `claude-code/<version>/…`
process, and its MCP servers are its direct children. Process tree plus start
time is enough.

**Codex desktop: per app only.** Facts:

- Every Codex MCP server is a direct child of the app's app-server process.
- Its environment has no thread, session or conversation variable. Checked
  with `ps eww` on a codegraph child.
- The app-server protocol has `thread/loaded/list`, `thread/closed`,
  `thread/unsubscribe` and `mcpServerStatus/list`. The desktop app talks to its
  app-server over stdio, so another program cannot ask it.
- A separate app-server daemon (0.160.1) listens on
  `~/.codex/app-server-control/app-server-control.sock`. A read-only
  `initialize` plus `thread/loaded/list` through `codex app-server proxy` gave
  no response within 12 s. Not established whether that socket expects a
  different framing or serves only daemon-owned threads.

**Decision.** Ownership for Codex stops at the app. `status` groups Codex
children into pools by start time, since each thread starts its full server
set at once, and labels such pools as inferred. Per-thread attribution through
the daemon socket remains a follow-up, not a blocker.

## R3: machine-wide build admission

Experiment: two `cargo build` runs of this crate, each with a fresh target
directory, started together under `nice -n 10`. A sampler counted `rustc`
processes every 100 ms.

| Setup | Peak concurrent rustc | Wall time | Tokens after |
| --- | --- | --- | --- |
| No shared jobserver | 22 | 12.3 s | — |
| FIFO jobserver, 2 tokens, `CARGO_MAKEFLAGS=-j --jobserver-auth=fifo:PATH` | 4 | 17.6 s | 2 of 2 |
| Same, 4 tokens, whole cargo process group killed with SIGKILL mid-build | — | — | 0 of 4 |

Findings:

- Cargo reads `CARGO_MAKEFLAGS` and joins the FIFO jobserver. The peak equals
  the tokens plus one implicit token per `cargo` process.
- Tokens held by a killed build are lost. The pool owner must restore the
  count. Policy: when no process that inherited the pool is alive, drain the
  FIFO and refill it to N. Between such points, losses only lower concurrency;
  they never raise it.
- Use `CARGO_MAKEFLAGS`, not `MAKEFLAGS`. The system `make` is GNU Make 3.81,
  which predates FIFO jobservers, and the repositories' Makefiles run under it.
- Not yet verified: getting the variable into every agent-spawned shell.
  Candidates are Claude Code's settings `env`, Codex's
  `shell_environment_policy`, `launchctl setenv` for GUI apps, and shell
  profiles. Stage 5 starts with this check.

## R4: true memory without root

Experiment: Python `ctypes` calling `proc_pid_rusage(pid, RUSAGE_INFO_V2)` on
every PID.

| Result | Value |
| --- | --- |
| Readable | 384 of 384 processes of the current user; 0 of 157 of other users |
| Scan time | 0.8 ms for 541 PIDs |
| Sum over the user's processes | footprint 20.36 GB, RSS 7.57 GB |
| codegraph, 53 processes | footprint 3.38 GB, RSS 0.51 GB |
| gopls, 4 processes | footprint 0.89 GB, RSS 0.04 GB |

`top -l 1 -stats pid,mem` also reports footprint without root, in 0.48 s, as
rounded text.

Crates:

| Crate | Footprint | Notes |
| --- | --- | --- |
| `libproc` 0.14.11 | Yes: `pid_rusage` returns `ri_phys_footprint` | Safe API; `bindgen` build dependency needs libclang, present with the Xcode tools |
| `sysinfo` 0.39.6 | No: `Process::memory` is `pti_resident_size` (RSS) | Rejected as the cost source |
| `darwin-libproc` 0.2.0 | — | Last release 2020 |

**Decision.** Use `libproc` for footprint, keeping `unsafe_code = "forbid"`.
Fall back to parsing `top` only if the dependency proves unacceptable at
implementation time.

## R5: multiplexer for configurable servers

Not executed: [mcp-mux](https://github.com/thebtf/mcp-mux) was not installed.
It is a third-party binary with one star, and installing it is the owner's
decision.

Estimate from the current process table, keying shared servers by working
directory as mcp-mux does by default:

| Server | Processes | Distinct working directories | After sharing |
| --- | --- | --- | --- |
| codegraph (`serve --mcp`, scoped to the project in its cwd) | 27 | 11 | 11 |
| railway (CLI-linked project depends on cwd) | 18 | 12 | 12 |
| gopls (workspace-scoped) | 5 | 5 | 5 |
| Total | 50 | — | 28 (−44 %) |

Before the restart, Codex alone held 178 codegraph processes over far fewer
projects, so the reduction under the leak is much larger. The cwd of those
processes was not recorded. App-bundled Codex servers (`node_repl`, computer
use) are not configured by the user and cannot be routed through a
multiplexer.

**Decision.** `status` reports duplicate groups by command and cwd, and names
the multiplexer as the remedy for configurable servers. For Claude sessions,
idle reclamation (R1) is an alternative that needs no extra component.

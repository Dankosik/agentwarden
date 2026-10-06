# agentwarden: specification

Status: definition, accepted for research gates. Date: 2026-10-06.
No product code exists yet; the `stats` example from the template is still in
place.

## Problem

A macOS workstation that runs several AI coding agents in parallel (Codex
desktop, Claude Code desktop and CLI) degrades over a working day until builds
crawl. The cause is not one runaway program but accumulation that no single
tool owns:

1. **Per-session MCP servers.** Every agent session starts its own copy of every
   configured stdio MCP server and keeps it for the session's lifetime. The
   desktop apps keep many sessions loaded, so copies multiply.
2. **Concurrent builds.** Each `cargo` invocation uses every core. Several agents
   building at once oversubscribe CPU and memory and push the machine into swap.
3. **Build caches.** Each worktree owns a `target/` of 10–20 GB that outlives the
   work it served.

### Evidence from the owner's machine (2026-10-06)

Apple Silicon, 10 cores, 16 GB RAM.

| Observation | Value |
| --- | --- |
| Swap used | 23.3 GB of 24.5 GB |
| Resident memory of the largest group (`node`) by `ps` | 2.9 GB, while swap held 23 GB |
| codegraph MCP processes | 202: 178 children of the live Codex app server, 13 of Claude Code, 8 orphans (PPID 1) |
| All tool helpers (codegraph, gopls, railway, node_repl, computer-use) | ~630 processes, ~20 orphans |
| Helper age | almost all older than one hour; 46 codegraph processes older than one day |
| One worktree `target/` | 19 GB |

Two consequences shape this specification:

- **Orphan reaping recovers about 3 % of the waste here.** Almost every leaked
  process still has a live parent.
- **RSS misreports the cost.** Swapped and compressed pages do not count toward
  RSS, so a tool that ranks by RSS points at the wrong processes.

The leaks are known upstream defects: [openai/codex#30408](https://github.com/openai/codex/issues/30408),
[openai/codex#43971](https://github.com/openai/codex/issues/43971),
[anthropics/claude-code#83689](https://github.com/anthropics/claude-code/issues/83689),
[anthropics/claude-code#99831](https://github.com/anthropics/claude-code/issues/99831).
agentwarden manages their effect; it does not depend on their fixes and stays
useful after them, because builds and caches remain.

## Existing tools

| Tool | Detection | Trigger | Gap for this problem |
| --- | --- | --- | --- |
| [cc-reaper](https://github.com/theQuert/cc-reaper) | PPID 1, RSS/FD thresholds per session | Claude hooks, 30 s daemon, launchd | Orphans and Claude first; RSS-based |
| [zclean](https://dev.to/thestack_ai/i-built-a-zombie-process-killer-because-claude-code-ate-14gb-of-my-ram-1deg) | PPID 1 + AI-tool signatures | SessionEnd hook, hourly | Orphans only |
| [claude-gc](https://github.com/kojott/claude-gc) | No TTY + name patterns + age | cron 15 min | Orphans only; Claude only |
| [mcp-reap](https://github.com/Caarlosgg/mcp-reap) | Missing or recycled parent | Manual | Orphans only; no schedule |
| [macos-orphan-process-cleanup](https://github.com/humzatariq-dev/macos-orphan-process-cleanup) | Name pattern + age | launchd/cron | One pattern; no attribution |
| [dev-hygiene-reaper](https://github.com/Niftory/dev-hygiene-reaper) | Idle trees, age, swap threshold; caches, worktrees | launchd 60 s / 5 min | Node/web stack; bash; no Codex awareness |
| [mcp-mux](https://github.com/thebtf/mcp-mux), [callmux](https://github.com/edimuj/callmux) | Share one upstream server across sessions | Always on (proxy) | Fixes configurable servers only; app-bundled servers (node_repl, computer use) stay per session |

Shared safety practice, which agentwarden adopts: dry-run by default, SIGTERM
then SIGKILL, PID revalidation by start time before signalling, an allowlist,
current-user processes only, an action log.

Shared blind spots, which define agentwarden's scope: no one attributes cost to
the owning session or worktree, no one measures true memory footprint, no one
handles live-but-idle processes, and no one coordinates builds across agents.

## Outcome

One user-level tool that answers "what is consuming this machine, on whose
behalf" and keeps the answer from getting worse, without breaking an active
agent session.

### Goals

1. **Attribution.** Every process is assigned to an owner: an agent app, a
   session where observable, a worktree, or "unowned". Cost is reported as
   physical footprint including compressed and swapped memory, not RSS.
2. **Reclamation by rule.** Reclaim orphans like existing tools, plus classes of
   live-parent processes that the research gates prove safe to stop.
3. **Build admission.** Cap concurrent compilation machine-wide, across every
   worktree and repository, so parallel agents share cores instead of thrashing.
4. **Cache reclamation.** Find build caches of finished or idle worktrees and
   reclaim them by rule.
5. **Evidence.** Every reclamation records what was stopped or deleted, why, and
   the memory or disk recovered.

### Non-goals

- Fixing Codex or Claude internals, or replacing their MCP configuration.
- Re-implementing an MCP multiplexer; agentwarden detects where one applies and
  reports it.
- A GUI, root privileges, or managing other users' processes.
- Linux and Windows in the first release. Platform calls stay behind one module
  so a later port does not touch policy.

## Behavior

Command names are provisional; the parser in `src/cli.rs` will own the final
grammar.

| Command | Effect | Writes |
| --- | --- | --- |
| `agentwarden status` | Owner tree with footprint, age, CPU idleness, duplicate groups; text or JSON | Nothing |
| `agentwarden reclaim` | Plans stops for matching rules and prints the plan | Nothing |
| `agentwarden reclaim --apply` | Executes the plan, appends to the action log | Signals, log |
| `agentwarden disk` | Lists build caches by worktree, size, last use, branch state; `--apply` deletes per rule | Deletions with `--apply` |
| `agentwarden watch` | Runs rules on an interval and on memory-pressure thresholds; installable as a LaunchAgent | Signals, deletions, log |
| `agentwarden builds` | Owns the machine-wide build token pool and prints its state | Token pool |

Exit status distinguishes success, a plan with nothing to do, partial
application, and refusal.

### Safety contract

- Dry run is the default for every effect; `--apply` or an explicit rule opt-in
  is required.
- Before a signal, the PID is revalidated by start time and command line; a
  changed process is skipped.
- Processes owned by another user, system processes, the agent app binaries
  themselves, and anything on the allowlist are never signalled.
- A live-parent process is stopped only by a rule class proven safe in gate R1.
- A cache is deleted only for a worktree with no running process inside it and
  no uncommitted changes; the default never deletes a checkout, only `target/`.
- Every effect appends one structured record to the action log.

## Accepted decisions

| Decision | Reason |
| --- | --- |
| Built from `rust-cli-template`, synchronous, single binary | The work is polling and signalling; no async runtime is needed |
| macOS first | All evidence and the owner's machine are macOS; footprint and launchd are platform-specific |
| Footprint, not RSS, as the cost metric | RSS hides swapped memory, measured above |
| Configurable MCP servers are routed through an existing multiplexer, not reimplemented | mcp-mux already handles sharing modes and idle shutdown |
| `unsafe_code = "forbid"` stays | Native calls come through a maintained crate or a system command |

## Research gates

Each gate must close with recorded evidence before the dependent capability is
built.

| Gate | Question | Blocks |
| --- | --- | --- |
| R1 | When an idle session's MCP server is stopped, does Codex or Claude restart it on the next tool call, fail that tool for the session, or fail the session? Per app and per server kind (configured stdio, app-bundled) | Live-parent reclamation |
| R2 | Can session ownership be observed for Codex app-server children (process tree, `~/.codex` state, app-server protocol)? Without it, ownership stops at the app | Per-session attribution and idle detection for Codex |
| R3 | Does a machine-wide FIFO jobserver (`--jobserver-auth=fifo:PATH` in `MAKEFLAGS`/`CARGO_MAKEFLAGS`) bound `cargo` and `rustc` parallelism across independent invocations? How are tokens recovered after a crash, and how does the environment reach agent-spawned shells? | Build admission |
| R4 | Which crate or system interface reads `phys_footprint` for same-user processes without root and without `unsafe` in this crate (`libproc` crate, `sysinfo`, `/usr/bin/footprint`)? What does it cost per scan of ~1,000 processes? | Footprint metric |
| R5 | Does running configured servers (codegraph, railway, gopls) through mcp-mux remove their duplicates under both apps, and which of them need per-project isolation? | Advice in `status`, scope of R1 |

## Delivery stages

1. **Observe.** `status` with attribution and footprint (R4; R2 as far as it
   closes). Done when its owner counts match a manual `ps` analysis on the
   owner's machine.
2. **Reclaim orphans.** `reclaim` with orphan rules and the safety contract;
   parity with existing tools.
3. **Watch.** `watch` as a LaunchAgent with interval and swap-pressure triggers.
4. **Live-parent rules.** Only the rule classes R1 proved safe.
5. **Build admission.** `builds` after an R3 prototype shows bounded parallelism.
6. **Disk.** `disk` for worktree caches.

## Completion evidence

- Each stage: tests for rule matching and the safety contract against recorded
  process tables, plus the built command's status and output.
- Stage 1: a recorded comparison with the manual analysis above.
- Stages 2–4: before and after swap use and process counts on the owner's
  machine over one working day, from the action log.
- Stage 5: concurrent builds in two worktrees never exceed the token count, with
  wall time recorded for both runs.

## Open questions for the owner

- Thresholds for automatic action in `watch` (swap level, idle age) once R1
  closes; defaults will be proposed from the stage 1 data.
- Whether `disk` may ever delete a worktree checkout, or only its `target/`.

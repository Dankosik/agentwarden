# agentwarden: specification

Status: definition; research gates closed (see [research](research.md)).
Date: 2026-10-06. Scope narrowed the same day to memory held by agent helper
processes; build admission and build-cache reclamation moved out (see
[Out of scope](#out-of-scope)). No product code exists yet; the `stats` example
from the template is still in place.

## Problem

A macOS workstation that runs several AI coding agents in parallel (Codex
desktop, Claude Code desktop and CLI) runs out of memory over a working day
until everything crawls. Every agent session starts its own copy of every
configured stdio MCP server and keeps it for the session's lifetime. The
desktop apps keep many sessions loaded, and some leak copies after sessions
end, so copies multiply into swap.

### Evidence from the owner's machine (2026-10-06)

Apple Silicon, 10 cores, 16 GB RAM.

| Observation | Value |
| --- | --- |
| Swap used | 23.3 GB of 24.5 GB |
| Resident memory of the largest group (`node`) by `ps` | 2.9 GB, while swap held 23 GB |
| codegraph MCP processes | 202: 178 children of the live Codex app server, 13 of Claude Code, 8 orphans (PPID 1) |
| All tool helpers (codegraph, gopls, railway, node_repl, computer-use) | ~630 processes, ~20 orphans |
| Helper age | almost all older than one hour; 46 codegraph processes older than one day |
| After the owner restarted the Codex app | swap 5.1 GB; 19 helpers left orphaned |

Two consequences shape this specification:

- **Orphan reaping recovers about 3 % of the waste here.** Almost every leaked
  process still has a live parent.
- **RSS misreports the cost.** Swapped and compressed pages do not count toward
  RSS; measured footprint was 2.7 times the RSS sum (R4).

The leaks are known upstream defects: [openai/codex#30408](https://github.com/openai/codex/issues/30408),
[openai/codex#43971](https://github.com/openai/codex/issues/43971),
[anthropics/claude-code#83689](https://github.com/anthropics/claude-code/issues/83689),
[anthropics/claude-code#99831](https://github.com/anthropics/claude-code/issues/99831).
agentwarden manages their effect and does not depend on their fixes; idle
per-session servers cost memory even without a leak.

## Existing tools

| Tool | Detection | Trigger | Gap for this problem |
| --- | --- | --- | --- |
| [cc-reaper](https://github.com/theQuert/cc-reaper) | PPID 1, RSS/FD thresholds per session | Claude hooks, 30 s daemon, launchd | Orphans and Claude first; RSS-based |
| [zclean](https://dev.to/thestack_ai/i-built-a-zombie-process-killer-because-claude-code-ate-14gb-of-my-ram-1deg) | PPID 1 + AI-tool signatures | SessionEnd hook, hourly | Orphans only |
| [claude-gc](https://github.com/kojott/claude-gc) | No TTY + name patterns + age | cron 15 min | Orphans only; Claude only |
| [mcp-reap](https://github.com/Caarlosgg/mcp-reap) | Missing or recycled parent | Manual | Orphans only; no schedule |
| [macos-orphan-process-cleanup](https://github.com/humzatariq-dev/macos-orphan-process-cleanup) | Name pattern + age | launchd/cron | One pattern; no attribution |
| [dev-hygiene-reaper](https://github.com/Niftory/dev-hygiene-reaper) | Idle trees, age, swap threshold; caches, worktrees | launchd 60 s / 5 min | Node/web stack; bash; thresholds set by hand |
| [mcp-mux](https://github.com/thebtf/mcp-mux), [callmux](https://github.com/edimuj/callmux) | Share one upstream server across sessions | Always on (proxy) | Configurable servers only; needs per-server setup |

Shared safety practice, which agentwarden adopts: SIGTERM then SIGKILL, PID
revalidation by start time before signalling, current-user processes only, an
action log.

Shared blind spots, which define agentwarden's scope: no one attributes cost to
the owning session, no one measures true memory footprint, no one reclaims
helpers of live but idle sessions, and every tool expects a human to tune it.

## Outcome

A tool an agent installs with one command, after which the machine stops
filling with idle agent helpers. No human configures, approves, or tunes
anything, and no active agent session breaks.

### Goals

1. **Zero configuration.** It ships with no configuration file. Only
   reclamations proven safe run, and they run automatically.
2. **Adaptive pressure.** How aggressively it reclaims follows the machine's
   memory pressure, not numbers a person sets: with free memory it does
   nothing; as swap grows it reclaims sooner.
3. **Language- and tool-neutral rules.** Rules use process ownership and
   session activity, never knowledge of which tool suits which language.
4. **Attribution.** Every process is assigned to an owner: an agent app, a
   session where observable, or "unowned". Cost is physical footprint, not RSS.
5. **Agent-first interface.** An agent can install it, check it, and explain
   what it did from structured output, without reading source.
6. **Evidence.** Every reclamation records what was stopped, why, and the
   memory recovered.

### Non-goals

- Fixing Codex or Claude internals, or editing their MCP configuration.
- Re-implementing an MCP multiplexer.
- A GUI, root privileges, or managing other users' processes.
- Linux and Windows in the first release. Platform calls stay behind one module
  so a later port does not touch policy.

### Out of scope

Moved out on 2026-10-06 by the owner's decision:

- **Build admission** (a machine-wide compiler limit). The owner rejects a
  fixed, explicitly set limit. The R3 measurements stay in
  [research](research.md) for any later tool.
- **Build-cache reclamation** (`target/` of finished worktrees). It is Rust- and
  git-specific and belongs in a separate Rust utility. The finished-worktree
  test drafted for it: no process has its cwd or an open file inside the
  worktree; `git status` is clean with nothing unpushed; the branch is merged
  into the default branch or deleted on the remote; it is not the main
  checkout; no source file changed for N days. All must hold.

## Rules

Each rule is on by default and needs no input. A rule fires only when the
process passes the [safety contract](#safety-contract).

| Rule | Target | When | Why it is safe |
| --- | --- | --- | --- |
| Orphan | A helper process whose agent parent has exited (PPID 1, or a recycled parent PID) and whose command line belongs to an agent tool's helper | Older than a short grace period | No session can use it again |
| Idle Claude session | Every MCP server child of a Claude Code session whose own CPU time has not grown for the idle window | Idle window chosen by pressure (below) | Claude Code restarts a stopped server on the next tool call (R1, observed twice) |
| Codex leak | Codex app-server MCP children | Never stopped | Codex reports a stopped server as "not connected" in that thread (R1); see [open question](#open-questions-for-the-owner) |

**Idle is measured on the session, not on the server.** Some servers spend CPU
on their own work, such as watching files, while no one calls them. A session
that has run no turn is idle whatever its servers do. A REPL-style server
(state kept in its process) loses that state when an idle session's servers are
stopped; the session gets a fresh one on its next call.

### Pressure levels

| Level | Signal (macOS) | Idle window |
| --- | --- | --- |
| Normal | Kernel memory pressure normal and swap not growing | Idle-session rule does not run |
| Warning | Kernel pressure warn, or swap grew during the last interval | 60 minutes |
| Critical | Kernel pressure critical, or swap above 90 % of its current size | 15 minutes |

The orphan rule runs at every level. The windows are defaults in code, not
configuration; changing them is a product change.

## Behavior

Command names are provisional; the parser in `src/cli.rs` will own the final
grammar.

| Command | Effect | Writes |
| --- | --- | --- |
| `agentwarden install` | Installs and starts the per-user LaunchAgent that runs `watch`; repeat runs are no-ops; `--uninstall` removes it | LaunchAgent plist |
| `agentwarden watch` | The loop the LaunchAgent runs: every 60 s, sample, apply rules, log | Signals, log |
| `agentwarden status` | Owners, footprint, age, idleness, pressure level, recent actions; `--json` for agents | Nothing |
| `agentwarden reclaim` | One pass of the rules now; `--dry-run` prints the plan only | Signals, log |

Exit status distinguishes success, nothing to do, partial application, and
refusal.

### Agent-first interface

- The repository's README starts with the one command an agent runs to install
  and verify: build or download, then `agentwarden install`, then
  `agentwarden status --json`.
- `status --json` has a stable, documented schema: pressure level, owner tree
  with footprint, idle durations, and the last actions with their rule and
  recovered memory.
- Diagnostics say what was refused and why, in a form an agent can act on.
- There are no prompts and no configuration file.

### Safety contract

- Before a signal, the PID is revalidated by start time and command line; a
  changed process is skipped.
- Only the current user's processes are signalled. System processes, the agent
  applications themselves, terminals, editors, and any process with a TTY are
  never signalled.
- Stopping is SIGTERM, then SIGKILL after a grace period, applied to the
  server's process group when it owns one.
- Every stop appends one structured record to the action log.

## Accepted decisions

| Decision | Reason |
| --- | --- |
| Built from `rust-cli-template`, synchronous, single binary | The work is polling and signalling; no async runtime is needed |
| macOS first | All evidence and the owner's machine are macOS; footprint, pressure and launchd are platform-specific |
| Zero configuration; pressure-adaptive defaults in code | The owner requires that no person configures it; agents install it |
| Footprint, not RSS, as the cost metric | RSS hides swapped memory (R4) |
| Footprint is read with the `libproc` crate (`proc_pid_rusage`) | Reads all same-user processes without root in under 1 ms; `sysinfo` reports RSS (R4) |
| `unsafe_code = "forbid"` stays | Native calls come through a maintained crate or a system command |
| Idleness is a session property, not a server property | Rules stay tool- and language-neutral; a server can be busy with its own work while unused |
| Claude Code session servers may be stopped when the session is idle | Claude Code restarts a stopped server on the next call (R1) |
| Codex app-server children are never stopped | Codex reports a stopped server "not connected" until an MCP refresh (R1); threads are not observable from outside (R2) |
| No build admission, no cache reclamation | Owner decision; see [Out of scope](#out-of-scope) |

## Research gates

Evidence and method for each gate are in [research](research.md).

| Gate | Question | Result |
| --- | --- | --- |
| R1 | What happens when an idle session's MCP server is stopped? | Claude Code 2.1.286 restarts it on the next tool call (run twice). Codex 0.160.0 fails calls with "not connected" until an MCP refresh (source) |
| R2 | Can Codex MCP children be attributed to threads? | Not from outside the app. No thread marker in the process or its environment; the desktop app-server speaks stdio to the app |
| R3 | Does a machine-wide FIFO jobserver bound concurrent builds? | Yes, through `CARGO_MAKEFLAGS`; now out of scope |
| R4 | How to read true memory without root or `unsafe`? | `proc_pid_rusage` via the `libproc` crate, 0.8 ms for 541 processes |
| R5 | Does a multiplexer remove duplicate configurable servers? | Estimated 50 → 28 processes; not needed for the zero-configuration design |

Remaining checks belong to implementation, not to new gates:

- Measure a Claude Code session's CPU time while it waits, to set the "not
  grown" tolerance.
- Confirm on each stage that the Claude Code version in use still restarts
  stopped servers; a release that stops doing so disables the idle rule.

## Delivery stages

1. **Observe.** `status` with owners, footprint, idleness, and pressure. Done
   when its counts match a manual `ps` analysis on the owner's machine.
2. **Orphans.** The orphan rule with the safety contract and the action log.
3. **Install and watch.** `install` and the LaunchAgent loop with pressure
   levels.
4. **Idle Claude sessions.** The idle-session rule.

## Completion evidence

- Each stage: tests for ownership, rule matching, and the safety contract
  against recorded process tables, plus the built command's status and output.
- Stage 1: a recorded comparison with the manual analysis above.
- Stages 2–4: swap use and helper counts on the owner's machine over one
  working day with agentwarden installed, from the action log, compared with a
  day without it.
- Stage 4: an idle session's next tool call succeeds after its servers were
  stopped.

## Open questions for the owner

- **Codex leak.** Codex holds most of the leaked memory and cannot be fixed
  safely in place. Options: (a) only report it in `status`; (b) restart the
  Codex app automatically when its whole process tree has been idle for a long
  period, for example overnight. Saved threads survive a restart, but an open
  window closes. The default is (a) until the owner chooses.

# agentwarden: specification

Status: active specification; implemented on macOS (see [research](research.md)).
Date: 2026-10-06 (expanded 2026-11-26 for dynamic multi-agent discovery,
Antigravity and clones, OpenCode, nested subagents, and terminal Claude CLI).
Scope is focused on memory held by agent helper processes; build admission and
build-cache reclamation remain [out of scope](#out-of-scope).

## Problem

A macOS workstation that runs several AI coding agents in parallel — Claude Code
(desktop and CLI), Codex (ChatGPT/Codex desktop apps, clones, CLI, `codex exec`,
and nested subagents), Antigravity (standard or renamed/proxy-wrapped clones
such as `Antigravity US.app`, and `agy` CLI), OpenCode (desktop app, CLI, and
detached background service), and other MCP-capable IDEs/CLIs (Cursor, Windsurf,
Zed, Gemini CLI, Cline) — runs out of memory over a working day until everything
crawls. Every agent session starts its own copy of configured stdio MCP servers
and runtime helpers and keeps them for the session's lifetime. Desktop apps keep
many sessions loaded, and some leak copies or sidecars after sessions or apps
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

Desktop agent apps (including arbitrarily renamed or proxy-wrapped clones such
as `Antigravity US.app`, `Codex.app`, or custom bundles) are discovered
dynamically by walking up from live bundled runtimes to their enclosing `.app`
bundle rather than hardcoding bundle paths. Any other running IDE or CLI
(Cursor, Windsurf, Zed, Gemini CLI, Cline, Claude Desktop direct MCPs, or future
tools) that parents configured MCP servers is also dynamically discovered as
`other` (or `claude` for Claude Desktop) so its MCP helpers are observed and
recorded in `known_helpers` for orphan reaping after exit, while conservatively
never stopping live helpers or restarting tools whose restart semantics are
unproven.

| Rule | Target | When | Why it is safe |
| --- | --- | --- | --- |
| `orphan` | A helper process whose agent parent has exited (PPID 1), with its descendants (excluding VCS/system utilities such as `git`, `ssh`, `gpg`): one previously seen serving a live agent (`known_helpers`), one whose command line matches a distinctive configured MCP server, an orphaned Antigravity `sidecar_sdk`, an orphaned desktop-bundled Antigravity `language_server` with `--host_bridge_url=`, or a childless `opencode-pty` when no OpenCode app/CLI/service is alive | Older than 120 s, and only while launchd's job list is readable | No live session can use it again |
| `idle-claude-session` | Every MCP server child tree of a Claude Code session (desktop or terminal CLI) whose own CPU time has not grown beyond timer noise for the idle window, that started no process in it, and that runs no shell, nested external agent, or non-helper terminal command. Under a TTY-attached `claude` CLI session, only direct children matching configured MCP pool signatures on the exact inherited controlling TTY (and their non-shell, non-agent, same-TTY workers) qualify | Idle window chosen by pressure (below) | Claude Code restarts a stopped stdio MCP server on the next tool call (R1, observed twice); terminal `claude` CLI spawns stdio MCP servers over pipes/sockets while inheriting the parent TTY |
| `idle-codex-restart` | Any desktop app hosting a bundled Codex runtime (`ChatGPT.app`, `Codex.app`, or clones), quit gracefully and relaunched via its exact bundle | All [idle desktop restart conditions](#idle-codex-restart) hold | Codex reports individually killed MCP servers as "not connected" (R1); saved threads persist on disk and restarting frees every leaked pool (swap 23.3 → 5.1 GB observed) |
| `idle-antigravity-restart` | Any desktop app hosting Antigravity's `language_server` (`Antigravity.app`, `Antigravity US.app`, or arbitrarily renamed clones), quit gracefully (plus graceful SIGTERM to `language_server` if it lingers) and relaunched via its exact bundle | All [idle desktop restart conditions](#idle-codex-restart) hold, tracking each instance's `--app_data_dir` (`~/.gemini/<name>`, default `antigravity`) independently | Conversations persist on disk; restarting frees leaked renderers, `language_server`, sidecars, and MCP pools |
| `idle-opencode-restart` | An idle OpenCode desktop app (and its detached `opencode-cli serve --service` only when no external CLI client is open), quit gracefully and relaunched via its exact bundle | All [idle desktop restart conditions](#idle-codex-restart) hold, using real SQLite session/message timestamps (`opencode.db`) + `prompt-history.jsonl` | Sessions persist in SQLite; SIGTERM-only shutdown preserves WAL integrity |
| `idle-opencode-service` | A headless `opencode-cli serve --service` daemon when no OpenCode desktop app or CLI client is running (SIGTERM only, never SIGKILL) | Idle 60 min under Warning / 15 min under Critical (or 30 min idle + user away 30 min + ≥ 1 GB under Normal), with no running commands/shells/TTY/nested agents | OpenCode restarts the service on demand; SIGTERM-only preserves the SQLite database |
| Live Codex / Antigravity / OpenCode / `other` MCP helpers one by one | Never | Never stopped while their parent is alive | Those agents do not reliably lazy-restart killed stdio MCP servers (or have unproven restart semantics) |

**Idle is measured on the session, not on the server.** Some servers spend CPU
on their own work, such as watching files, while no one calls them. A session
that has run no turn is idle whatever its servers do. A REPL-style server
(state kept in its process) loses that state when an idle session's servers are
stopped; the session gets a fresh one on its next call. Idle time is counted
only across passes about a minute apart: after a gap, such as the Mac
sleeping, every session's clock starts again.

### Idle Codex restart

Owner decision 2026-10-06 (extended to Antigravity/clones and OpenCode desktop
apps): agentwarden restarts idle desktop agent apps as a whole because their
leaked runtime and MCP pools cannot be reclaimed safely one by one. Defaults are
fixed in code:

| Condition | Default | Signal (no root needed, checked on the owner's machine) |
| --- | --- | --- |
| No agent turn activity | 30 minutes | Codex: no `~/.codex/sessions/**/rollout-*.jsonl` modified. Antigravity: no conversation/summary write in `~/.gemini/<--app_data_dir>` (default `antigravity`). OpenCode: no real session/message/part timestamp in `opencode.db` (via read-only `sqlite3`, ignoring background `kv` WAL ticks and `opencode.log`) or `prompt-history.jsonl` write. In all cases: nothing running under the app except MCP/runtime pool members — no shell, no TTY process, no `codex exec`, no nested external agent (`claude`, `codex`, `opencode`, `agy`), and no non-pool command |
| Nobody at the machine | 30 minutes | HID idle time from `IOHIDSystem` |
| Worth restarting | ≥ 1 GB, or ≥ 512 MB under Warning or Critical pressure | Footprint (R4), pressure level (Codex: runtimes subtree; Antigravity: app tree minus active commands; OpenCode: app tree + attached service minus active commands) |
| Not too often | At most once per 6 hours per bundle | Persisted per-bundle restart timestamps |

Restart sequence: ask the app to quit gracefully via SIGTERM; if it has not
exited within 60 seconds, abort and record the refusal (never force-kill the
app); for OpenCode (with no CLI client) or Antigravity, gracefully SIGTERM a
lingering companion service/`language_server` (never SIGKILL); relaunch the
exact `.app` bundle in the background without focus (`open -g -j -a <bundle>`);
the next pass reclaims helpers the quit left orphaned. If the app quits after
the 60 seconds, or the relaunch fails, later passes open that exact bundle again
once it is gone, for up to 10 minutes; after that a closed app is taken as the
user's choice.

MCP pool members are children of a runtime whose command line matches a
configured MCP server (including package-runner forms such as `npx`/`uvx`/
`npm exec`, discovered across Claude, Codex, Antigravity/Gemini + plugins,
OpenCode, Cursor, Windsurf, Zed, VS Code, and project `.mcp.json` configs) or whose executable
is inside the app bundle or its external runtime directory. Any other child is a
command: a shell that runs one command execs it, so a running `cargo test` is
the runtime's own child. Pools that an idle agent starts by itself
([openai/codex#43971](https://github.com/openai/codex/issues/43971)) therefore
do not count as activity.

### Pressure levels

| Level | Signal (macOS) | Idle window |
| --- | --- | --- |
| Normal | Kernel memory pressure normal and swap not growing | Idle-session rule does not run |
| Warning | Kernel pressure warn, or swap grew during the last interval | 60 minutes |
| Critical | Kernel pressure critical | 15 minutes |

How full swap is does not raise the level: macOS adds swap files as it needs
them, so allocated swap is nearly always nearly full.

The orphan rule runs at every level. The windows are defaults in code, not
configuration; changing them is a product change.

## Behavior

Command names are provisional; the parser in `src/cli.rs` will own the final
grammar.

| Command | Effect | Writes |
| --- | --- | --- |
| `agentwarden install` | Installs and starts the per-user LaunchAgent that runs `watch`; a repeat run restarts it on the binary it is run from, so an upgrade takes effect; `--uninstall` removes it | LaunchAgent plist |
| `agentwarden watch` | The loop the LaunchAgent runs: every 60 s, sample, apply rules, log | Signals, log |
| `agentwarden status` | Owners, footprint, age, idleness, pressure level, recent actions; `--format json` for agents | Nothing |
| `agentwarden reclaim` | One pass of the rules now; `--dry-run` prints the plan only | Signals, log |

Exit status: 0 success or nothing to do, 1 error or unsupported platform,
2 usage error, 3 `reclaim` with at least one failed action. Refused and skipped
actions are outcomes in the log, not failures.

### Agent-first interface

- The repository's README starts with the one command an agent runs to install
  or upgrade: `install.sh` downloads the release for the Mac's architecture,
  verifies its checksum, and runs `agentwarden install`; then
  `agentwarden status --format json`, whose `version` field shows what runs.
- `status --format json` has a stable schema, documented in the README: pressure level, owner tree
  with footprint, idle durations, and the last actions with their rule and
  recovered memory.
- Diagnostics say what was refused and why, in a form an agent can act on.
- There are no prompts and no configuration file.

### Safety contract

- Before a signal, the PID is revalidated by start time and owner; a changed
  process is skipped.
- Only the current user's processes are signalled, and agentwarden refuses to
  run as root. System processes, shells, terminals, editors, VCS/system
  utilities (`git`, `ssh`, `gpg`, etc.), and unrelated processes with a TTY are
  never signalled; a helper tree containing a shell, an agent, or a protected
  terminal process is left alone as a whole. Under `idle-claude-session`, an
  idle TTY-attached `claude` CLI session's configured MCP helpers (and their
  non-shell, non-agent worker descendants) carrying the exact controlling TTY
  inherited from that parent `claude` session are permitted; processes on any
  different TTY, direct non-helper TTY children, and any `orphan` candidate
  with a TTY remain strictly protected.
- Agent hosts and desktop applications are never signalled by `Stop` rules; the
  only exceptions are provably orphaned desktop-bundled Antigravity
  `language_server` processes (`ppid == 1`, `--host_bridge_url=`, non-launchd,
  owning app exited or replaced), graceful SIGTERM-only shutdown in
  `idle-opencode-service`, and graceful SIGTERM quit in
  [idle desktop restart](#idle-codex-restart).
- `Stop` sends SIGTERM, then SIGKILL after a 5 s grace period, to each process
  of the tree, children first. `StopGraceful` and desktop app restarts send
  SIGTERM only and never SIGKILL. Zombies are skipped: they have exited already.
- In `reclaim` and `watch`, a `/usr/bin/top` failure or timeout degrades
  non-fatally so `orphan`, `idle-claude-session`, pressure-idle
  `idle-opencode-service`, and session observation still run while
  footprint-gated restarts conservatively skip. A `/bin/ps` failure always aborts
  the pass.
- Every action appends one structured record to the action log.

## Accepted decisions

| Decision | Reason |
| --- | --- |
| Built from `rust-cli-template`, synchronous, single binary | The work is polling and signalling; no async runtime is needed |
| macOS first | All evidence and the owner's machine are macOS; footprint, pressure and launchd are platform-specific |
| Zero configuration; pressure-adaptive defaults in code | The owner requires that no person configures it; agents install it |
| Footprint, not RSS, as the cost metric | RSS hides swapped memory (R4) |
| Footprint is read from `top -l 1 -stats pid,mem` | Its MEM column equals `phys_footprint` (matched on six processes); `libproc`, chosen in R4, was dropped at implementation because its `bindgen` build dependency needs BSD-3-Clause and ISC licenses outside `deny.toml` and a libclang build |
| `unsafe_code = "forbid"` stays | Native calls come through a maintained crate or a system command |
| Idleness is a session property, not a server property | Rules stay tool- and language-neutral; a server can be busy with its own work while unused |
| Claude Code session servers may be stopped when the session is idle | Claude Code restarts a stopped server on the next call (R1) |
| Codex app-server children are never stopped one by one; the whole app is restarted when Codex and the user are idle | Codex reports a stopped server "not connected" until an MCP refresh (R1); threads are not observable from outside (R2); owner chose restart over report-only |
| No build admission, no cache reclamation | Owner decision; see [Out of scope](#out-of-scope) |

## Research gates

Evidence and method for each gate are in [research](research.md).

| Gate | Question | Result |
| --- | --- | --- |
| R1 | What happens when an idle session's MCP server is stopped? | Claude Code 2.1.286 restarts it on the next tool call (run twice). Codex 0.160.0 fails calls with "not connected" until an MCP refresh (source) |
| R2 | Can Codex MCP children be attributed to threads? | Not from outside the app. No thread marker in the process or its environment; the desktop app-server speaks stdio to the app |
| R3 | Does a machine-wide FIFO jobserver bound concurrent builds? | Yes, through `CARGO_MAKEFLAGS`; now out of scope |
| R4 | How to read true memory without root or `unsafe`? | `proc_pid_rusage` works (0.8 ms for 541 processes); implemented with `top`, which reports the same value |
| R5 | Does a multiplexer remove duplicate configurable servers? | Estimated 50 → 28 processes; not needed for the zero-configuration design |

Remaining checks belong to implementation, not to new gates:

- Done 2026-10-06: a waiting Claude Code session spends 0.2–0.45 s of CPU per
  minute; the idle tolerance is 0.5 s per minute (T11 in [research](research.md)).
- Confirm on each stage that the Claude Code version in use still restarts
  stopped servers; a release that stops doing so disables the idle rule.
- Observe an idle Codex for one hour: which rollout writes and processes
  appear, to confirm the activity signal does not fire on its own.
- Find a graceful quit that needs no one-time permission prompt (AppleScript
  `quit` triggers macOS Automation consent; SIGTERM to the app is the
  candidate) and confirm threads are listed again after relaunch.

## Delivery stages

1. **Observe.** `status` with owners, footprint, idleness, and pressure. Done
   when its counts match a manual `ps` analysis on the owner's machine.
2. **Orphans.** The orphan rule with the safety contract and the action log.
3. **Install and watch.** `install` and the LaunchAgent loop with pressure
   levels.
4. **Idle Claude sessions.** The idle-session rule.
5. **Idle Codex restart.** The restart rule and its sequence.

## Completion evidence

- Each stage: tests for ownership, rule matching, and the safety contract
  against recorded process tables, plus the built command's status and output.
- Stage 1: a recorded comparison with the manual analysis above.
- Stages 2–4: swap use and helper counts on the owner's machine over one
  working day with agentwarden installed, from the action log, compared with a
  day without it.
- Stage 4: an idle session's next tool call succeeds after its servers were
  stopped.
- Stage 5: one recorded restart on the owner's machine: conditions met, app
  quit gracefully and relaunched, Codex threads still listed, helpers and swap
  before and after.

## Resolved owner decisions

- 2026-10-06: the Codex leak is handled by [idle Codex restart](#idle-codex-restart)
  rather than report-only, with the defaults above.

Plan: [plan](plan.md).

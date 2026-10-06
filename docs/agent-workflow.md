# Working with agents

[AGENTS.md](../AGENTS.md) owns repository rules. This is optional navigation, not
another mandatory workflow. Small tasks can go from the request and affected
implementation directly to a checked result.

## Find the relevant context

| Current decision | Read when needed |
| --- | --- |
| Add or replace a command | [First command](first-command.md), affected parser/dispatch/tests |
| Understand ownership or public behavior | [Architecture](architecture.md) and affected callers |
| Reuse a mechanism or select a dependency | Matching [library-guide](library-guide.md) row and selected API |
| Measure a resource claim | [Performance](performance.md); a code-only audit can report bounds and hypotheses |
| Packaging or authorized release | [Releasing](releasing.md); preparation does not authorize publication |
| Share decisions across sessions or owners | [Task notes](../specs/README.md), only when persistence is useful |
| Review a change or audit a defined scope | [Agent review](agent-review.md) |
| Change instructions or the skill snapshot | Pin procedure below, [evaluation](agent-evaluation.md), and [design rationale](agent-instruction-design.md) |

Read only the matching branch. Research catalogs explain past choices; current
Cargo inputs and the selected API establish current behavior and compatibility.

## Select a technical skill

Descriptions in `.agents/skills/` are the activation surface. This map is a fallback
when the host does not expose them. Load a specialist for a real decision, not as
a mandatory stage. Useful combinations are allowed without loading every neighbor.

| Decision | Skill |
| --- | --- |
| Implement requested behavior | [rust-implement](../.agents/skills/rust-implement/SKILL.md) |
| Representation, borrowing, traits, transformations | [rust-idiomatic](../.agents/skills/rust-idiomatic/SKILL.md) |
| Responsibilities and useful module boundaries | [rust-design](../.agents/skills/rust-design/SKILL.md) |
| Grammar, configuration precedence, terminal/output interface | [rust-cli-interface](../.agents/skills/rust-cli-interface/SKILL.md) |
| Error distinctions, diagnostics, partial success, status | [rust-errors](../.agents/skills/rust-errors/SKILL.md) |
| Byte flow, buffering, record boundaries, fallible I/O | [rust-io](../.agents/skills/rust-io/SKILL.md) |
| File identity, traversal, replacement, preservation | [rust-filesystem](../.agents/skills/rust-filesystem/SKILL.md) |
| Child arguments, pipes, termination, reaping | [rust-processes](../.agents/skills/rust-processes/SKILL.md) |
| Admission, task ownership, cancellation, joining | [rust-concurrency](../.agents/skills/rust-concurrency/SKILL.md) |
| Retained owners, growing buffers, memory bounds | [rust-memory](../.agents/skills/rust-memory/SKILL.md) |
| Workload timing, profiling, benchmark comparisons | [rust-performance](../.agents/skills/rust-performance/SKILL.md) |
| An uncertain defect or causal explanation | [rust-debugging](../.agents/skills/rust-debugging/SKILL.md) |
| Function/parser/I/O-double behavior | [rust-testing](../.agents/skills/rust-testing/SKILL.md) |
| Executable wiring, status, real pipes or terminals | [rust-cli-testing](../.agents/skills/rust-cli-testing/SKILL.md) |
| Cargo resolution, features, toolchain, build profile | [rust-build](../.agents/skills/rust-build/SKILL.md) |
| Artifact compatibility and delivery | [rust-distribution](../.agents/skills/rust-distribution/SKILL.md) |

Repository contracts constrain reusable advice. Keep template-specific policy in
maintained instructions rather than editing vendored skills. Explicitly revising
one project decision does not reopen unrelated choices.

## Implementation and diagnosis

For a command change, identify input, success output, expected errors, status,
and file/process effects on the affected path. A refactor preserves those
contracts. Use small behavior-oriented slices and independent expected values.
For a defect fix, reproduce the failure before the repair when feasible; record
why a pre-fix reproduction is unavailable instead of fabricating a red test.

Choose the boundary and checks defined in AGENTS.md and the Makefile. A parser
call establishes grammar, not executable wiring or status. A Read/Write double
establishes local error propagation, not OS pipe behavior. Review and diagnosis
can finish without edits; an agreed logical resource bound needs no profiling
prerequisite. Repair introduced failures within scope, without routine approval
loops. A handoff alone is not a reason to rerun valid checks.

## Larger work and delegation

Use `specs/<topic>/` when shared decisions must survive sessions. Record the
outcome, accepted choices, open questions, dependencies, and completion evidence.
Do not create a spec merely to edit a command. Respect a research/design-only task.

Delegate independent work only when tools are available and coordination is
worthwhile. Each worker receives the task, exact revision/snapshot, relevant
files, settled decisions, writable scope, constraints, available tools, and the
expected evidence. Keep writers on disjoint files; sequence shared edits and
build resources. Read-only reviewers can share a checkout, but must report if
it changes under them. Without a subagent facility, use sequential self-review
and identify it as such rather than simulating independent agents.

Workers return a compact receipt: inspected revision and files, changes or
findings with anchors and evidence, actual commands/results, uncertainties, and
blockers. The coordinator verifies evidence against the integrated state and
resolves conflicts using contracts and code, not majority agreement. Reuse
unaffected checks and rerun only those invalidated by integration.

For iterative review, predeclare a finite budget appropriate to risk. Stop when
the selected scope is accounted for, candidates are resolved or explicitly
unverified, and introduced failures have been repaired and checked; if the budget
is exhausted first, report remaining blockers. Repetition until reviewers agree
is not evidence of perfection. The [review guide](agent-review.md) defines how to
falsify claims and retain both requirements and contract coverage.

A handoff records outcome, decisions, files, commands/results, and next action or
blocker. Distinguish local verification, CI, behavioral evaluation, and publication.

## Update the pinned skills deliberately

[skills-source.json](../.agents/skills-source.json) owns the upstream URL, immutable
commit, and SHA-256 inventory. Skills are available offline; startup must not fetch
a moving branch. Update from a reviewed local checkout with a full commit:

```sh
python3 scripts/sync_skills.py --source ../rust-cli-skills --revision FULL_COMMIT_SHA --check
python3 scripts/sync_skills.py --source ../rust-cli-skills --revision FULL_COMMIT_SHA --apply
make template-check
```

Preview exits 1 for drift, 0 for no change, and 2 for invalid input; it writes
nothing. Apply reads committed blobs, refuses locally changed skills or inventory,
and updates contents and pin together without changing application code. Review
activation and scope changes as behavior changes, not cosmetic edits.

The inventory contains SKILL.md only. A per-skill upstream LICENSE is omitted
only when its exact bytes are already preserved in the root LICENSE. Different
or missing notices require explicit reconciliation. Unknown resources,
non-regular files, and orphan license directories are rejected. Keep
[third-party notices](../THIRD_PARTY_NOTICES.md) accurate.

Skill-only updates need template checks, not an invented Rust workload. Add other
checks only for affected code/build/initialization inputs. CI gates remain intact.
Structural tests do not establish model behavior; use selected evaluation cases
when assessing an instruction change and retain NOT RUN for unavailable trials.

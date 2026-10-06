# Rust CLI template instruction audit

Audited template base: `6939e56ca673e919b7a750a0aadd812eefc77c33`.
Upstream skill revision adopted: `aa14d4147af6bb5b58533389cd2ba52455459790`,
the merged [rust-cli-skills PR #1](https://github.com/Dankosik/rust-cli-skills/pull/1).
This is a static instruction audit plus a focused synchronization compatibility
fix. It is not an executed model comparison or a performance benchmark.

## Basis and scope

[OpenAI's article](https://developers.openai.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra)
recommends selective skill activation, contextual document reading, revisiting
rigid instructions, and clear completion boundaries. Here that means a smaller
project-specific entry point, not another orchestrator or removal of meaningful
Rust and CLI checks. Shared instructions may be used by different hosts/models;
no model-specific speedup is presumed.

Reviewed AGENTS.md, Claude/Copilot pointers, README and consumer README,
contribution/workflow/architecture/first-command/library/performance/release docs,
task notes, all 16 vendored skill bodies, their provenance manifest, and the
synchronization/integrity checks that constrain updates. Cargo and the Makefile/CI
were inspected as authorities, not rewritten as part of this audit. The dated
library research and catalog remain reference evidence, not new instructions.

## Findings and changes

### Preserve what already works

The old root already respects research-only and named-phase scopes, owns work
through repair, allows direct small changes and disjoint delegation, and rejects
unrequested publication. Do not claim those protections were missing. Keep
CLAUDE.md and the Copilot file as short pointers to one shared policy. Keep
specs optional and the application synchronous unless a task needs otherwise.

### Reduce the always-read layer

The original AGENTS.md contains a 16-row skill catalog and repeats generic Rust
advice and version details. Move the full map into the optional workflow guide;
route by the actual decision and load only chosen skill bodies. Point to Cargo,
the toolchain file, and the Makefile instead of inventing another version or
command authority. Preserve the explicitly forbidden unsafe code, intentional
predeclared toolbox, CLI contracts, ownership, and bounded-resource goals.

The library guide remains valuable. Make its matching row contextual to new
mechanics or a dependency decision; a known std/project API does not need a new
survey. Do not read the entire 78-crate research catalog for ordinary work.
README and the initialized consumer README use the same guidance.

### Separate local validation from unrelated gates

Keep make check for applicable Rust changes and existing CI requirements. Expose
make template-check for documentation, skills, and maintenance, make verify for
both surfaces, and consumer smoke for identity/initialization. Reuse equivalent
checks on the same revision/environment; do not run check, verify, and all their
component commands as independent obligations.

Use the actual parser/configuration merge for local grammar/precedence claims,
the built binary for process wiring and status, and real pipes/terminals for OS
behavior. Preserve byte distinctions, output failures, cleanup, and assertions
on effects. Unavailable stronger evidence remains unverified. No automatic
benchmark, new specification, platform matrix, or approval phase is introduced.

### Adopt upstream improvements without a local fork

The template vendors the old `3ec323b33654173caff2c760b99d70c8923a6576` snapshot.
The upstream instruction PR is now merged. Copy its 16 SKILL.md blobs exactly and
update the immutable revision and SHA-256 map together. The update narrows overlap,
separates review/diagnosis from editing, and scopes evidence to the changed claim.
It retains Rust-specific byte/error, file identity, subprocess, task-termination,
MSRV, and feature-contract safeguards.

Do not copy the skill-pack release metadata or bump this application's version.
The source manifest owns the current pin; third-party notices link to it instead
of carrying another stale commit. The original template-design source is labeled
historical. No moving branch is fetched by an agent's startup.

### Repair the documented synchronization path

The old source_files function accepts only skills/name/SKILL.md. Newer upstream
packs also contain per-skill LICENSE files, so the documented update rejects the
new source before it can adopt any instruction fixes.

Keep the current local SKILL.md-only inventory. Accept only a regular per-skill
LICENSE whose bytes exactly equal the root LICENSE already retained by this
consumer. Different/missing notices, orphan notice directories, unknown resources,
symlinks, oversized source files, and local edits still fail closed. Read committed
blobs rather than a dirty checkout. Preserve preview, immutable source selection,
application-file isolation, and rollback after write failures. No silent copying
of executable resources or license changes is introduced.

Eight regression cases supplement the existing seven sync tests. These are real
Python tests of the maintenance behavior, not assertions that prompts improved.
Full CI and behavioral results must be recorded separately from local checks.

## File-level decisions

| Surface | Disposition |
| --- | --- |
| AGENTS.md | Project contracts, contextual routing, completion and validation map |
| docs/agent-workflow.md | Optional skill/document map, delegation, handoffs, provenance updates |
| CONTRIBUTING.md | Same check boundaries and evidence expectations for people and agents |
| README.md and .template/README.md | Contextual library lookup and instruction-only validation |
| .agents/skills-source.json | Adopt merged immutable upstream revision with exact hashes |
| .agents/skills/*/SKILL.md | All 16 are exact upstream blobs, not template-specific rewrites |
| scripts/sync_skills.py and tests | Licensed-pack compatibility with existing protections retained |
| THIRD_PARTY_NOTICES.md | Retained license/attribution; current revision delegated to source manifest |
| docs/template-design.md | Preserve historical source; distinguish current vendoring pin |
| docs/agent-evaluation.md | Twelve natural-request cases on the existing template; NOT RUN |
| CLAUDE.md and Copilot instructions | Keep short shared-policy pointers unchanged |
| Architecture, first-command, library and performance guides | Keep useful contextual technical guidance, not compulsory preflight |
| specs/README.md and PR template | Keep optional notes and actual-evidence reporting unchanged |
| Cargo, toolchain, dependencies, application code, Makefile, CI and release workflow | No changes or weakened gates |

## Evidence limits

The skill snapshot and manifest can be checked deterministically. Sync fixtures
verify code behavior, and CI verifies its configured application/template surfaces.
Neither establishes fewer model errors or faster generated programs. Use the
[template-specific comparison cases](../agent-evaluation.md) to evaluate root-only
and root-plus-skill changes while holding application inputs constant. The sync
code correction must be evaluated separately from instruction effectiveness.

No release, tag, package upload, or marketplace update is part of this PR.

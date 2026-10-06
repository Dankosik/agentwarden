---
name: rust-implement
description: "Implement requested Rust CLI behavior within the project's existing contracts and technical choices."
---

# Rust Implement

**Execution.** When the intended behavior is clear, implement it directly. Treat supplied requirements as constraints. Preserve settled technical decisions outside the explicitly requested change; do not reopen unrelated choices.

Read the affected code and callers, then extend the existing path. Preserve the project's Rust edition, minimum supported Rust version, target platforms, CLI contract, and selected dependencies.

**Reuse.** For a proposed helper, check relevant existing code, standard-library operations, and declared crates for matching semantics; avoid an exhaustive library survey. Use matching APIs directly. Custom mechanics need a concrete semantic or operational gap; wrappers should add domain meaning or adaptation. A parser, runtime, or utility crate should answer a current requirement.

**Clarity.** Write for the next reader: intention-revealing names, cohesive responsibilities, explicit ownership, and visible effects and failure paths. Keep changes local and idiomatic to Rust. Apply SOLID, DRY, and YAGNI as heuristics: centralize shared knowledge, preserve distinct rules, and add only structure justified by current requirements. Prefer the simplest implementation that remains easy to read and change.

Keep data moving without unnecessary copies or whole-input materialization when input can grow; bounded data can justify simple materialization. Account for affected buffers, queues, and retained results. Preserve the existing execution model; for new work, start synchronously when sufficient. Honor an agreed concurrency or resource requirement without turning every implementation into a performance investigation.

Resolve routine details autonomously. If a concrete contradiction prevents implementation, identify it and continue independent work; ask only for information that changes the required outcome.

Carry the change through existing formatting, compilation, and focused tests appropriate to the changed packages and targets. Preserve required project checks. Within the environment's permissions, fix failures introduced by the change and rerun affected checks rather than stopping for review after the first patch. Reuse applicable results for the same revision and environment.

Finish when the requested outcome and required checks are satisfied, or state the concrete blocker and any unavailable verification. Do not invent unrelated cleanup, profiling, platform matrices, or new test infrastructure as completion gates. Do not claim a process, OS, safety, or performance property from a weaker check.

---
name: rust-testing
description: "Behavior. Use when writing or improving focused Rust CLI unit tests, deterministic fixtures, property tests, fuzz targets, or I/O test doubles."
---

# Rust Testing

**Behavior first.** Identify the observable promise a change could break. Choose fixture values and relationships that distinguish correct behavior from a plausible defect; derive expectations independently of the implementation. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

Use existing tests and commands; inspect features and the Rust baseline when API selection or test discovery depends on them. Start with ordinary values and direct calls. Cursor and small Read or Write implementations can expose meaningful I/O behavior without a general mocking framework. Replace collaborators beyond the property being tested; avoid traits created solely to mock internal choreography.

Preserve relevant distinctions through the assertion. Lossy text conversion, trimming, sorting, decoding, or helpers must not make an incorrect result appear correct. Verify byte boundaries, aliases, or ordering when they are part of the contract. A round trip alone can miss the same defect shared by encoder and decoder.

Make failures identify the rule. Group cases that share behavior and separate unrelated obligations. For fallible I/O, inject relevant short operations or a failure after partial progress, including flush when completion depends on it.

Control time, scheduling, and shared state. Bound waits, join workers, and propagate their failures. Use isolated child environments for process-global settings rather than racing mutations across tests.

Use property tests or fuzzing when the requested input space or invariant warrants them; keep exploratory runs bounded and retain discovered failures as regressions. Challenge whether the defect could survive the test or a harmless refactor could break it.

For review, assess whether the tests distinguish the relevant defect without editing. For implementation, run the relevant tests, confirm discovery, ignored cases, and affected feature coverage, and preserve required project checks. Reuse applicable evidence for the same revision and environment. Do not add a full feature matrix, fuzz campaign, Miri run, or process harness merely because the skill mentions one. State any unavailable check; passing tests describe exercised behavior, not untested process, platform, or soundness guarantees.

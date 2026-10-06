---
name: rust-cli-testing
description: "Use for Rust CLI executable wiring, exit status, real streams, filesystem effects, pipes, signals, terminal, or packaged-artifact tests."
---

# Rust CLI Testing

**Prove the mechanism.** Choose the smallest boundary that includes what makes the command's promise true. A direct parser test can establish grammar, but not executable wiring, process status, or terminal behavior. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

When the property crosses the process boundary, run the actual built binary through Cargo's integration-test support or the established command-test helper; pure parsing or transformation can remain in focused unit tests. Reuse assert_cmd or equivalent when already suitable. Set arguments, stdin, environment, and working directory explicitly; isolate fixtures in owned temporary directories and keep their guards alive until children finish.

Assert stdout, stderr, status, and file effects where they matter. Preserve relevant byte and value distinctions; normalizing paths, line endings, colors, or JSON must not conceal the behavior under test. Snapshot intended interfaces without accepting every new snapshot as correct.

Choose scenarios that expose a plausible failure: a conflicting option, explicit configuration override, non-UTF-8 path on a supporting platform, or error after partial work. Keep destructive and external effects inside controlled fixtures.

Piped capture does not exercise a terminal. Use real pipes for backpressure and downstream closure, and a PTY when terminal detection or interaction is the claim. Coordinate at observable boundaries rather than relying on sleeps. A timeout must also stop and reap the child; an assertion failure must not leak processes.

Bound captured output and fixture size so the harness does not exhaust memory while testing memory behavior. Test platform-specific semantics on the relevant platform; cross-compilation alone is insufficient.

For review, identify the missing mechanism and appropriate check without editing. For implementation, confirm selected tests ran and cleanup completed. Report the command and boundary exercised, including unavailable platform, terminal, or release-artifact evidence. Reuse existing harnesses; do not invent cross-platform or PTY infrastructure as a completion gate for a property that does not require it. Never replace a missing mechanism with a weaker test and claim the stronger result.

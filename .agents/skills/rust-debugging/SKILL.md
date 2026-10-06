---
name: rust-debugging
description: "Causality. Use for an uncertain Rust CLI defect, panic, hang, incorrect output, resource leak, or platform-dependent failure."
---

# Rust Debugging

**Causality.** Find the first observable divergence between the intended command and the real execution path. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

Choose the smallest useful signal for the reported symptom. Inspect actual arguments, bytes, environment, target, features, or profile when they distinguish plausible causes; compiler-only failures need no process reproduction. Separate compiler diagnostics, process startup failures, domain errors, and crashes. Preserve raw stdout, stderr, and status when their differences explain the symptom.

Read the available error chain or backtrace and trace relevant callers. For ownership errors, identify the actual owner and required lifetime before adding clone, Arc, or unsafe. For optimized-only failures, compare cfg, assertions, overflow behavior, and timing rather than assuming an optimizer bug.

Choose the next observation to distinguish plausible causes. Follow file identity, buffer contents, short I/O operations, child streams, locks, and task completion across their real boundaries. A retry, panic catch, or discarded error can hide the mechanism without repairing it.

Use the appropriate debugger, profile, or syscall trace when runtime evidence is needed. Miri can detect some undefined behavior in supported executions, but it does not certify arbitrary OS interaction or prove overall soundness. Keep sensitive input and credentials out of diagnostic captures.

Control scheduling, fixtures, and dependencies enough to expose intermittent behavior; change one causal variable at a time.

For diagnosis, finish with the supported explanation or next discriminating observation; do not edit unless fixing is requested. For a fix, change the causal owner, replay the original failure, and check relevant neighboring paths. Retain a regression check, remove temporary instrumentation, and report actual verification and remaining uncertainty without expanding to unrelated diagnostics.

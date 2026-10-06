---
name: rust-memory
description: "Use to analyze or reduce Rust CLI allocation churn, retained data, growing buffers, or peak-memory use."
---

# Rust Memory

**Retention.** Account for what stays live, how large it can become, and which owner releases it. Separate allocation count, live heap, peak resident memory, stack, and mapped pages. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

Trace the affected owners as input size or concurrency grows; include parser buffers, collections, queues, worker state, output reordering, or captured subprocess data where they contribute. Streaming one stage does not bound a later collector. Name the term that grows before selecting a smaller representation.

Remove demonstrated redundant copies and intermediate collections. Borrow where lifetimes stay simple, move owned data when possible, and reuse buffers in repeated work. A tiny borrowed view or shared reference can retain a large owner; copying a small surviving value may reduce total retention.

Choose capacity from credible bounds or observed distributions. Vec::clear retains capacity; repeated shrinking can replace retention with allocation churn. Account for oversized-record recovery and long-lived buffers. Inline collections enlarge every containing value, and large stack arrays can exhaust worker stacks.

Measure before introducing interning, arenas, custom allocators, alternative hashers, or unsafe storage. Preserve collision resistance where input is adversarial. Memory mapping changes paging and lifetime behavior; file-backed maps require safety conditions against external modification and are not a universal zero-memory read.

Use allocation evidence to locate churn and resident-memory evidence for the process budget. Account for allocator retention, page cache conditions, child processes, and measurement overhead. A falling allocation count alone does not establish lower peak memory.

For an audit or diagnosis, identify supported owners and growth terms, separate hypotheses from observed evidence, and propose the next useful measurement without editing. A code-derived retention bound is not a measured RSS result. For a requested reduction or bound, implement the relevant ownership change, preserve output and failures, and check the bound at its real mechanism. Compare representative inputs when claiming a measured memory improvement; report unavailable measurements without inventing them or treating every task as an allocator investigation.

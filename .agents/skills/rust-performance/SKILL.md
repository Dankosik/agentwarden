---
name: rust-performance
description: "Use to assess or measure Rust CLI startup, runtime, throughput, CPU, or I/O concerns, or verify an optimization."
---

# Rust Performance

**Evidence.** Identify the requested result: an audit, measurement, or optimization. Define the relevant workload and metric; establish a comparable baseline when measuring a change. Separate startup, time to first result, total runtime, throughput, memory, and binary size. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

For whole-command measurements, measure an optimized executable directly; cargo run adds orchestration and possible build work. Keep toolchain, target, features, profile, input, and environment comparable. Define the relevant warm or cold cache condition and control output destinations; terminal rendering or a slow pipe can dominate computation.

Choose evidence that separates plausible causes: CPU samples for computation, allocation profiles for churn, syscall or I/O evidence for repeated operations and waiting. Account for profiler overhead. Inspect complexity, repeated parsing, formatting, copies, and unnecessary initialization before changing low-level mechanics.

When whole-command speed is the claim, compare repeated samples with an existing suitable tool such as hyperfine. Use a microbenchmark for a specific operation, with realistic inputs and observable results; it cannot establish whole-command improvement. Select the comparison and sample plan before measuring, and retain variance rather than rerunning until a favorable sample appears.

Treat LTO, codegen units, optimization level, allocator changes, SIMD, and PGO as measured alternatives. Smaller code and maximum optimization settings do not guarantee faster execution. Preserve the target CPU baseline; native CPU tuning is unsuitable for a generally distributed binary unless that compatibility limit is deliberate.

For an audit without runtime evidence, report code-supported properties separately from bottleneck hypotheses and propose discriminating observations without editing. For benchmarking, report the comparison without requiring a code change. For an authorized optimization, verify output, errors, ordering, and resource bounds alongside speed; report before-and-after evidence and its limits. If evidence is unavailable or inconclusive, state that and prefer the simpler correct implementation rather than inventing a speedup or adding a new profiling platform.

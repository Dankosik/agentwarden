---
name: rust-distribution
description: "Use when preparing or reviewing Rust CLI packages, prebuilt artifacts, or installers, or performing an authorized release."
---

# Rust Distribution

**Compatibility.** Identify the affected delivery contract and which users can install and run the artifact: operating system, architecture, CPU baseline, system libraries, and installation method. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

Preserve the project's release tooling and supported target set. A source package and a prebuilt binary have different prerequisites. Cargo installation depends on a suitable toolchain and build dependencies; a downloaded executable depends on its actual linkage and platform baseline.

When producing artifacts, build from the intended source revision and dependency resolution. Inspect executable naming, permissions, linked libraries, included licenses, and required runtime files. Cross-compiling successfully does not demonstrate that the binary starts on the oldest supported system. CPU-native optimization can silently narrow compatibility.

Keep profile decisions measurable. Stripping symbols changes diagnostic options and file size; it does not establish lower runtime memory. Static linking, especially with native dependencies, is a target-specific property rather than a universal portability switch. Test the produced artifact with its selected features and runtime assumptions.

Include help, completions, or manuals when they are part of the distribution contract, deriving them from the same command definition where supported. Keep archives and installation paths predictable, and make removal or replacement respect user-owned configuration and data.

Reuse the existing release mechanism for checksums, signatures, and provenance when required; distinguish artifact integrity from trusted origin. Keep registry credentials, signing identity, and publication authority owner-controlled.

For review, explain the packaging or compatibility issue without editing. For release preparation, implement the requested packaging change and validate the affected artifact or installation path in a controlled staging location. Preparation is not authorization to publish, sign, upload, or change user-level installations. Before an authorized publication, verify installation and a meaningful command using the final artifact in the relevant environment. Report exercised targets and untested compatibility explicitly; do not turn a narrow packaging change into an unrequested full target matrix.

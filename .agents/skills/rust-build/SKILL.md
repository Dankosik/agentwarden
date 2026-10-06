---
name: rust-build
description: "Resolution. Use for Rust CLI Cargo configuration, toolchains, dependency features, compiler or linker failures, and requested build-profile changes."
---

# Rust Build

**Resolution.** Explain what Cargo actually selects before changing its declarations. Identify the affected Cargo command and inspect the layer that can explain it: resolution/features, compiler compatibility, linking, profile, or packaging. Read workspace configuration and environment as needed, not as an exhaustive preflight. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

Separate the compiler running the build, language edition, minimum supported Rust version, and executable target. Declaring rust-version does not prove dependencies or source compile on that version; exercise the baseline when compatibility is affected or required, and never substitute a newer host build as proof. A host build cannot establish target linker or system-library compatibility.

Trace dependencies through the resolved graph and feature activation. Declare directly used crates in the consuming package. Preserve the application's Cargo.lock and use locked resolution where reproducibility is required. Inspect transitive defaults: disabling a feature on one edge does not prevent another dependency from enabling it.

Choose only dependencies and features the present CLI needs. Review their maintenance, licensing, and vulnerability evidence when changing them, with scope proportional to the change. Keep build scripts and procedural macros in the build-time trust analysis. Do not fabricate lockfile checksums or bypass integrity checks to fix downloads.

For profile or feature changes, inspect workspace-root settings and the supported combinations affected. Optimize for the requested metric; strip, LTO, codegen units, and panic strategy have different effects. Panic abort changes cleanup behavior and cannot be treated as a free size setting.

For analysis, explain the failing or proposed build layer without editing. For requested changes, use the existing formatting, compiler, Clippy, test, or artifact command that establishes the affected claim, preserving required checks and lint policy. Check relevant default and feature configurations; all-features success is not proof that the default path works, and unsupported combinations need not be invented. Reuse valid results and report unavailable targets or toolchains. Explain the change and observed result without treating compilation as runtime verification.

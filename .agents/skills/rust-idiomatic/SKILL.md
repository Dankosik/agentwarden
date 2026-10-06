---
name: rust-idiomatic
description: "Use for Rust representation, borrowing, trait, or transformation decisions where caller-visible semantics or readability need attention."
---

# Rust Idiomatic

**Contracts.** Make ownership and caller expectations visible in ordinary Rust. Identify absence, mutation, ordering, errors, and resource lifetime before changing representation. Honor supplied requirements and preserve settled choices outside the requested change; resolve only what the task leaves open.

Use the supported edition, Rust version, and project conventions. Borrow through slices, string slices, or paths when ownership is unnecessary; move owned values when responsibility transfers. Let lifetimes describe actual relationships. Before cloning or introducing shared ownership to satisfy the borrow checker, inspect the data flow and scope of the borrow.

Choose enums for meaningful alternatives and newtypes for distinctions that prevent real misuse. Use Option and Result to preserve absence and failure. Add standard conversion and comparison traits when their semantics fit; keep equality and hashing consistent. Cloning an Arc shares the same allocation rather than making independent data.

Use iterators for readable transformations and loops for clearer stateful control flow. Lazy iterators perform work only when consumed; collecting changes memory use. Keep required effects and early errors explicit. Preserve ordering, numeric overflow behavior, and UTF-8 boundaries; paths and arbitrary bytes are not necessarily text.

Prefer safe standard-library and established crate operations. Keep unsafe code narrowly justified by a concrete need, with the invariants its callers must uphold; successful compilation does not establish soundness.

For review, explain the contract issue and smallest justified change without editing files. For implementation, finish with formatted code and focused checks for the behavior or type contract affected. Avoid unrelated modernization or abstractions that obscure the operation; neither a clone nor a shared owner is wrong merely by its spelling.

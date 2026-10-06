# Third-party notices

The vendored Rust skills in `.agents/skills/` originate from
[Dankosik/rust-cli-skills](https://github.com/Dankosik/rust-cli-skills),
copyright (c) 2026 Dankosik, under the MIT license reproduced in
[LICENSE](LICENSE). [skills-source.json](.agents/skills-source.json) is the
single source for their current immutable upstream revision and content hashes.
Identical per-skill MIT notices are retained by that root license; a notice with
different content must be preserved explicitly before adopting it.

The template adapts the runnable starting point, initialization, contract,
validation, and agent-workflow ideas of
[Dankosik/go-service-template-rest](https://github.com/Dankosik/go-service-template-rest),
commit `ef6a3bc9983431de065a421e6a3fced36ef886ba`, also licensed under MIT.
The Rust runtime and maintenance tools are newly written for CLI behavior.

Cargo dependencies retain their own licenses. `Cargo.lock` identifies the
resolved packages; `deny.toml` defines the project's dependency license and
advisory policy. This notice does not replace the licenses of dependencies.

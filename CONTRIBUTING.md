# Contributing

Issues and pull requests are welcome. For a bug, include the command, relevant
input, expected and observed output, exit status, operating system, and tool
version. Remove secrets and personal data from reproductions. For an improvement,
explain the user need and the behavior that would change.

Use the toolchain declared by `rust-toolchain.toml`. Start with the README's
quickstart; consult the [first-command guide](docs/first-command.md) when adding
or replacing a command. Coding agents follow [AGENTS.md](AGENTS.md).

Keep changes focused and preserve existing CLI contracts unless explicitly
revising them. Reuse the standard library and existing dependencies; the
predeclared toolbox is intentional. Consult the matching
[library-guide](docs/library-guide.md) entry for a new technical mechanism or
dependency decision, not as an exhaustive survey before every edit.

Include regression coverage that distinguishes changed behavior from a plausible
defect. A direct call to the real parser or configuration merge can cover its
local contract. Exercise the built executable when process wiring, streams,
status, or post-exit effects change, and real OS mechanisms when their behavior
is claimed. Preserve meaningful bytes and effects through assertions.

## Choose applicable validation

| Changed surface | Checks |
| --- | --- |
| Rust code or build inputs | Focused checks during implementation; applicable `make check` gates before a code PR |
| Documentation, instructions, pinned skills, Python maintenance | `make template-check` for integrity, local links, and maintenance tests |
| Both Rust and template maintenance | `make verify`, which combines `check` and `template-check` |
| Identity or initialization | Also `make template-smoke`, which initializes and tests a disposable consumer |
| Dependencies or release packaging | Relevant policy and artifact checks from [releasing](docs/releasing.md), within the authorized scope |

The [Makefile](Makefile) defines the actual commands. `make check` runs formatting,
Clippy, all-target tests, and doctests; each underlying Cargo command can also run
individually. Reuse equivalent successful results for the same revision and
environment. Explain checks that could not run, preserve required CI gates, and
never substitute weaker evidence for the claimed mechanism. Documentation-only
edits do not require unrelated runtime tests or new infrastructure.

For skill updates, follow the pinned procedure in
[agent workflow](docs/agent-workflow.md). Preserve source provenance and license
notices; do not independently fork vendored instructions to change project policy.
[Behavioral evaluation](docs/agent-evaluation.md) distinguishes instruction quality
from structural validity and contains scenarios on this template, not claimed
model results.

Describe the problem, resulting behavior, and actual verification in the PR.
For measured performance claims, include the workload and comparable release
measurements. Dependency changes should explain the needed capability and relevant
feature, license, platform, and maintenance implications. Neither a green PR nor
available credentials authorize release publication.

Report suspected vulnerabilities through [SECURITY.md](SECURITY.md), not public
issue details. Contributions use the repository's [MIT license](LICENSE).

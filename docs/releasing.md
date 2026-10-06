# Releases

agentwarden ships native archives for macOS on Apple Silicon and Intel. A `v`
tag starts the release workflow. It runs the same required CI as a pull
request, validates the tag and repository against Cargo metadata, builds on
each native runner, and tests the executable after extracting it from its final
archive, and attests each archive's build provenance. A single final job
creates a draft GitHub Release with both archives, `SHA256SUMS`,
[`install.sh`](../install.sh) and the tagged version's
[changelog](../CHANGELOG.md) section, then publishes it. The repository has
immutable releases on: once published, a release's assets and tag cannot
change. The README installs with the `install.sh` of the latest release, so
the script and the binary always come from the same version.

To verify packaging before a release, open the **Release** workflow in GitHub
Actions and choose **Run workflow** on a branch. This runs CI, builds and tests
both archives at the selected commit, and uploads them as workflow artifacts.
Branch dispatches do not validate a version tag or create a GitHub Release.

The workflow uses the repository's `GITHUB_TOKEN`; no personal token, crates.io
credentials, or signing service is required. Creating a tag is an intentional
publication action. Nothing publishes from branch commits, branch dispatches,
or pull requests. `publish = false` prevents accidental crates.io publication.

## Versions

Versions follow [Semantic Versioning](https://semver.org/). The public
interface is the commands and flags, the `--format json` fields, exit codes,
file locations, and the LaunchAgent label. Before 1.0, a minor version may
change that interface and its changelog entry says how; a patch version never
does. A rule change that stops more processes, or stops them sooner, is at
least a minor version.

## Making a release

1. Move the `Unreleased` entries in `CHANGELOG.md` under a new
   `## [X.Y.Z] - YYYY-MM-DD` heading and update the links at the bottom.
2. Set `package.version` in `Cargo.toml` to `X.Y.Z` and run
   `cargo check --locked` after `cargo update -p agentwarden` so `Cargo.lock`
   follows.
3. Run `make verify` and `python3 scripts/release.py notes` to preview the
   release page. Merge through a pull request with required CI green.
4. Tag the merge commit on `main` and push the tag:
   `git tag vX.Y.Z && git push origin vX.Y.Z`. A tag with a suffix, such as
   `v0.2.0-rc.1`, publishes a prerelease that `install.sh` does not pick as
   the latest.
5. After the release is published, run `install.sh` on a Mac and confirm
   `agentwarden status --format json` reports the new `version`.

The tag must identify the exact checked-out commit, that commit must be on
`origin/main`, and its version must equal Cargo's version. The publication step refuses to overwrite an existing release.
If a run fails before publication, repair the cause and rerun it. If a release
already exists or publication partly succeeded, inspect its assets and the run
before deciding whether to finish that release or issue a new version. Do not
move a tag that users may already have downloaded.

## Local packaging

On a Mac with Rust and Python 3.9+, replace the target below with the `host`
reported by `rustc -vV`:

```sh
cargo build --release --locked --target aarch64-apple-darwin
python3 scripts/release.py package --target aarch64-apple-darwin --dist dist
```

Packaging derives the binary name and version from `cargo metadata`. It
includes the binary, `README.md`, and `LICENSE`, plus `NOTICE`,
`THIRD_PARTY_NOTICES`, and `THIRD_PARTY_NOTICES.md` when present. The smoke
test runs `--version` and `completions bash`, which need no system access.
Packaging never cross-compiles or claims to test a foreign executable.
`checksums` validates the full inventory before writing `SHA256SUMS`.

## Supported artifacts

| Target | Build runner | Archive |
| --- | --- | --- |
| `aarch64-apple-darwin` | macOS 15 Apple Silicon | `.tar.gz` |
| `x86_64-apple-darwin` | macOS 15 Intel | `.tar.gz` |

Linux and Windows build and pass CI, but agentwarden refuses to run there, so
no archives are published for them. The binaries are not signed or notarized.
`install.sh` downloads with `curl`, which does not mark files as quarantined,
so Gatekeeper does not block them; an archive downloaded with a browser needs
`xattr -d com.apple.quarantine agentwarden` before its first run.

Verify a manually downloaded archive with
`shasum -a 256 --check --ignore-missing SHA256SUMS`; `install.sh` does this
itself. Checksums detect download corruption. Authenticity comes from the
build provenance attestation: `gh attestation verify <archive> --repo
Dankosik/agentwarden` checks that this repository's release workflow built
it. Releases before 0.1.2 have no attestation.

When changing a target, update the release workflow matrix, `TARGETS` in
`scripts/release.py`, `install.sh`, this table, and the corresponding
verification evidence. Keep the aggregate `required` CI check as the branch
protection target.

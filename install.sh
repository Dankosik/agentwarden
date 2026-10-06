#!/bin/sh
# Install or upgrade agentwarden from GitHub Releases and start its LaunchAgent.
#
#   curl -fsSL https://raw.githubusercontent.com/Dankosik/agentwarden/main/install.sh | sh
#
# AGENTWARDEN_VERSION selects a release such as v0.1.0 (default: the latest).
# AGENTWARDEN_BIN_DIR selects where the binary goes (default: ~/.local/bin).
# Running it again upgrades in place and restarts the background agent.
set -eu

repo="Dankosik/agentwarden"
bin_dir="${AGENTWARDEN_BIN_DIR:-$HOME/.local/bin}"

say() { printf 'agentwarden: %s\n' "$1"; }
fail() {
	printf 'agentwarden: %s\n' "$1" >&2
	exit 1
}
fetch() { curl --proto '=https' --tlsv1.2 -fsSL "$@"; }

[ "$(uname -s)" = Darwin ] || fail "only macOS is supported"
# A shell under Rosetta reports x86_64 on Apple Silicon; ask the hardware.
if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
	target=aarch64-apple-darwin
else
	target=x86_64-apple-darwin
fi

version="${AGENTWARDEN_VERSION:-}"
if [ -z "$version" ]; then
	latest=$(fetch -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
		fail "cannot reach GitHub"
	version="${latest##*/}"
fi
case "$version" in
v[0-9]*) ;;
*) fail "no published release found" ;;
esac

name="agentwarden-${version#v}-$target"
base="https://github.com/$repo/releases/download/$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM

say "downloading $version for $target"
fetch -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || fail "cannot download $name.tar.gz"
fetch -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "cannot download SHA256SUMS"
(
	cd "$tmp"
	awk -v file="$name.tar.gz" '$2 == file' SHA256SUMS | shasum -a 256 -c -s -
) || fail "checksum mismatch for $name.tar.gz"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$bin_dir"
# Replace by rename so a running copy keeps its file until it restarts.
cp "$tmp/$name/agentwarden" "$bin_dir/.agentwarden.new"
chmod 755 "$bin_dir/.agentwarden.new"
mv -f "$bin_dir/.agentwarden.new" "$bin_dir/agentwarden"
say "installed $("$bin_dir/agentwarden" --version) to $bin_dir"

"$bin_dir/agentwarden" install

found=$(command -v agentwarden 2>/dev/null || true)
if [ -z "$found" ]; then
	say "add $bin_dir to PATH to run agentwarden from a shell; the background agent already runs"
elif [ "$found" != "$bin_dir/agentwarden" ]; then
	say "$found comes first in PATH; remove it (for a cargo install: cargo uninstall agentwarden)"
fi
say "check it with: $bin_dir/agentwarden status"

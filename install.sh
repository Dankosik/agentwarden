#!/bin/sh
# Install or upgrade agentwarden from GitHub Releases and start its LaunchAgent.
#
#   curl -fsSL https://github.com/Dankosik/agentwarden/releases/latest/download/install.sh | sh
#
# AGENTWARDEN_VERSION selects a release such as v0.1.0 (default: the latest).
# AGENTWARDEN_BIN_DIR selects where the binary goes (default: ~/.local/bin).
# When that directory is not on PATH, one line adding it goes to the shell's
# startup file, as rustup and uv do; AGENTWARDEN_NO_MODIFY_PATH=1 skips that.
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

# Put a directory on PATH for new shells of the user's login shell.
add_to_path() {
	dir=$1
	if [ -n "${AGENTWARDEN_NO_MODIFY_PATH:-}" ]; then
		say "add $dir to PATH to run agentwarden from a shell; the background agent already runs"
		return 0
	fi
	case "$dir" in
	"$HOME"/*) shown="\$HOME/${dir#"$HOME"/}" ;;
	*) shown=$dir ;;
	esac
	case "${SHELL:-}" in
	*/fish)
		profile="$HOME/.config/fish/conf.d/agentwarden.fish"
		line="fish_add_path \"$shown\""
		;;
	*/zsh)
		profile="${ZDOTDIR:-$HOME}/.zshrc"
		line="export PATH=\"$shown:\$PATH\""
		;;
	*/bash)
		profile="$HOME/.bash_profile"
		line="export PATH=\"$shown:\$PATH\""
		;;
	*)
		profile="$HOME/.profile"
		line="export PATH=\"$shown:\$PATH\""
		;;
	esac
	if ! { [ -f "$profile" ] && grep -qxF "$line" "$profile"; }; then
		mkdir -p "$(dirname "$profile")"
		printf '\n# Added by the agentwarden installer\n%s\n' "$line" >>"$profile"
	fi
	say "added $dir to PATH in $profile; open a new terminal to run agentwarden"
}

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

case ":$PATH:" in
*":$bin_dir:"*)
	found=$(command -v agentwarden 2>/dev/null || true)
	if [ "$found" != "$bin_dir/agentwarden" ]; then
		say "$found comes first in PATH; remove it (for a cargo install: cargo uninstall agentwarden)"
	fi
	;;
*) add_to_path "$bin_dir" ;;
esac
say "check it with: $bin_dir/agentwarden status"

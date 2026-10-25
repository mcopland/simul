#!/usr/bin/env bash
# Downloads the prebuilt simul binary matching VERSION into bin/simul and
# verifies its sha256. Prints nothing to stdout; errors go to stderr.

set -euo pipefail

PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_URL="${SIMUL_RELEASE_BASE_URL:-https://github.com/mcopland/simul/releases/download}"

die() {
	echo "simul install: $*" >&2
	exit 1
}

target_triple() {
	local os=$1 arch=$2
	case "$os/$arch" in
	Darwin/arm64 | Darwin/aarch64) echo aarch64-apple-darwin ;;
	Darwin/x86_64) echo x86_64-apple-darwin ;;
	Linux/arm64 | Linux/aarch64) echo aarch64-unknown-linux-musl ;;
	Linux/x86_64 | Linux/amd64) echo x86_64-unknown-linux-musl ;;
	*) return 1 ;;
	esac
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | awk '{print $1}'
	else
		die "cannot verify $(basename "$1"): neither sha256sum nor shasum is installed"
	fi
}

fetch() {
	curl -fsSL --connect-timeout 10 --max-time 120 --retry 2 -o "$1" "$2"
}

main() {
	local version os arch triple bin archive url tmp staged expected actual
	version=$(tr -d '[:space:]' <"$PLUGIN_DIR/VERSION") ||
		die "failed to read $PLUGIN_DIR/VERSION"
	os=${SIMUL_OS:-$(uname -s)}
	arch=${SIMUL_ARCH:-$(uname -m)}
	triple=$(target_triple "$os" "$arch") ||
		die "no prebuilt binary for $os/$arch; build from source with cargo build --release"

	bin="$PLUGIN_DIR/bin/simul"
	if [ -x "$bin" ] && [ "$("$bin" --version 2>/dev/null)" = "simul $version" ]; then
		return 0
	fi

	archive="simul-$triple.tar.gz"
	url="$BASE_URL/v$version/$archive"
	tmp=$(mktemp -d "${TMPDIR:-/tmp}/simul.XXXXXX") ||
		die "failed to create a temp dir under ${TMPDIR:-/tmp}"
	# shellcheck disable=SC2064 # expand $tmp now; it is local to main
	trap "rm -rf '$tmp'" EXIT

	fetch "$tmp/$archive" "$url" ||
		die "failed to download $url (release v$version)"
	fetch "$tmp/$archive.sha256" "$url.sha256" ||
		die "failed to download $url.sha256 (release v$version)"

	expected=$(awk '{print $1; exit}' "$tmp/$archive.sha256")
	actual=$(sha256_of "$tmp/$archive")
	[ -n "$expected" ] && [ "$expected" = "$actual" ] ||
		die "checksum mismatch for $archive: expected $expected, got $actual"

	tar -xzf "$tmp/$archive" -C "$tmp" simul ||
		die "failed to extract simul from $archive"

	# Stage under a unique name in bin/, then rename: the swap is atomic, two
	# tmux servers installing at once can't interleave writes, and the path
	# that installed hooks point at stays the same.
	mkdir -p "$PLUGIN_DIR/bin" ||
		die "failed to create $PLUGIN_DIR/bin"
	staged=$(mktemp "$PLUGIN_DIR/bin/.simul.XXXXXX" 2>/dev/null) ||
		die "failed to write to $PLUGIN_DIR/bin"
	# shellcheck disable=SC2064 # expand both paths now; they are local to main
	trap "rm -rf '$tmp' '$staged'" EXIT
	{ cp "$tmp/simul" "$staged" && chmod 755 "$staged"; } ||
		die "failed to copy simul into $staged"
	mv -f "$staged" "$bin" ||
		die "failed to install $bin"
}

main "$@"

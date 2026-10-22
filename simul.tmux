#!/usr/bin/env bash
# TPM entry point: checks tmux, finds the simul binary, and starts the daemon.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

find_binary() {
	local candidate
	for candidate in "$CURRENT_DIR/bin/simul" "$CURRENT_DIR/target/release/simul"; do
		if [ -x "$candidate" ]; then
			echo "$candidate"
			return 0
		fi
	done
	command -v simul
}

# Accepts "tmux 3.3a", "tmux next-3.4"; unversioned builds ("tmux master") pass.
tmux_version_ok() {
	local version major minor
	version=$(tmux -V | awk '{print $2}' | sed 's/^[^0-9]*//')
	[ -z "$version" ] && return 0
	major=${version%%.*}
	minor=${version#*.}
	minor=${minor%%[!0-9]*}
	[ "$major" -gt 3 ] || { [ "$major" -eq 3 ] && [ "${minor:-0}" -ge 3 ]; }
}

main() {
	if ! tmux_version_ok; then
		tmux display-message "simul: needs tmux 3.3 or newer (found $(tmux -V))"
		return 0
	fi
	local bin socket
	if ! bin=$(find_binary); then
		tmux display-message "simul: binary not found; run: cargo build --release --manifest-path '$CURRENT_DIR/Cargo.toml'"
		return 0
	fi
	socket=$(tmux display-message -p '#{socket_path}')
	tmux run-shell -b "'$bin' daemon --tmux-socket '$socket' >/dev/null 2>&1"
}

main

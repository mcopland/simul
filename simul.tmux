#!/usr/bin/env bash
# TPM entry point: checks tmux, installs the prebuilt simul binary matching VERSION,
# starts the daemon, and binds @simul-key (default prefix + a) to toggle the sidebar.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

find_binary() {
	local candidate
	for candidate in "$CURRENT_DIR/target/release/simul" "$CURRENT_DIR/bin/simul"; do
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
	local bin socket override install_err=""
	# @simul-bin points at another build, such as a dev checkout's, for testing; unset it and
	# reload tmux to go back to the release.
	override=$(tmux show-option -gqv @simul-bin)
	override=${override/#\~/$HOME}
	if [ -n "$override" ]; then
		if [ ! -x "$override" ]; then
			tmux display-message "simul: @simul-bin is not an executable: $override"
			return 0
		fi
		bin=$override
	# A local cargo build means a dev checkout; leave it alone.
	elif [ ! -x "$CURRENT_DIR/target/release/simul" ]; then
		# Only a failed install has an error worth showing; on success, stderr is
		# just noise such as tar warnings.
		if ! install_err=$("$CURRENT_DIR/scripts/install-binary.sh" 2>&1 >/dev/null); then
			install_err=${install_err##*$'\n'}
		else
			install_err=""
		fi
	fi
	if [ -z "$bin" ] && ! bin=$(find_binary); then
		tmux display-message "simul: binary not found${install_err:+ ($install_err)}; run: cargo build --release --manifest-path '$CURRENT_DIR/Cargo.toml'"
		return 0
	fi
	if [ -n "$install_err" ]; then
		tmux display-message "$install_err; using $bin"
	fi
	socket=$(tmux display-message -p '#{socket_path}')
	# --replace hands over from a daemon of another build, such as the one before an update.
	tmux run-shell -b "'$bin' daemon --replace --tmux-socket '$socket' >/dev/null 2>&1"

	local key old
	key=$(tmux show-option -gqv @simul-key)
	key=${key:-a}
	old=$(tmux show-option -gqv @simul-bound-key)
	if [ -n "$old" ] && [ "$old" != "$key" ]; then
		# Only while the old key still toggles simul; the user may have rebound it since.
		case "$(tmux list-keys -T prefix "$old" 2>/dev/null)" in
		*simul*" toggle"*) tmux unbind-key -T prefix "$old" ;;
		esac
	fi
	tmux bind-key "$key" run-shell -b "'$bin' toggle"
	tmux set-option -g @simul-bound-key "$key"
}

main

#!/usr/bin/env bash
# Measures what shown sidebars cost on an isolated tmux server: 3 sessions with 13 windows,
# one attached client that switches windows every 10 seconds.
# Usage: bench-sidebar.sh <simul-bin> <lazy: true|false|none> [secs] [label]
# Prints one tab-separated row: label, sidebars, sidebar/daemon/tmux/total CPU seconds over
# the run, and the sidebars' summed RSS in MB. "none" leaves lazy out of the config, for
# builds that predate it.

set -euo pipefail

usage="usage: bench-sidebar.sh <simul-bin> <lazy: true|false|none> [secs] [label]"
bin=${1:?$usage}
lazy=${2:?$usage}
secs=${3:-120}
label=${4:-"$(basename "$(dirname "$(dirname "$(dirname "$bin")")")")/lazy=$lazy"}

[ -x "$bin" ] || {
	echo "bench-sidebar: not an executable: $bin" >&2
	exit 1
}
case "$lazy" in
true | false | none) ;;
*)
	echo "bench-sidebar: lazy must be true, false, or none, not $lazy" >&2
	exit 1
	;;
esac
# Sidebars are found by the binary's path in ps, as the daemon resolves it: absolute, with
# symlinks such as macOS's /tmp resolved.
bin=$(cd "$(dirname "$bin")" && pwd -P)/$(basename "$bin")

# Short, since tmux and simul socket paths have a length limit.
dir=$(mktemp -d /tmp/sb.XXXXXX)
chmod 700 "$dir"
sock=$dir/t.sock
daemon_pid=""
client_pid=""
cleanup() {
	{
		[ -n "$client_pid" ] && kill "$client_pid"
		tmux -S "$sock" kill-server
		[ -n "$daemon_pid" ] && kill "$daemon_pid"
		wait
	} 2>/dev/null
	rm -rf "$dir"
}
trap cleanup EXIT

mkdir -p "$dir/config/simul" "$dir/claude" "$dir/kiro"
{
	printf '[notify]\nenabled = false\n'
	[ "$lazy" = none ] || printf '[sidebar]\nlazy = %s\n' "$lazy"
} >"$dir/config/simul/config.toml"

export SIMUL_SOCKET=$dir/s.sock
export XDG_CONFIG_HOME=$dir/config
export CLAUDE_CONFIG_DIR=$dir/claude
export KIRO_HOME=$dir/kiro
export TMUX="$sock,0,0"
unset TMUX_PANE

t() { tmux -S "$sock" "$@"; }

t -f /dev/null new-session -d -s s1 -x 200 -y 50 -c "$dir" sh
t new-session -d -s s2 -x 200 -y 50 -c "$dir" sh
t new-session -d -s s3 -x 200 -y 50 -c "$dir" sh
for s in s1 s2 s3; do
	n=3
	[ "$s" = s1 ] && n=4
	for _ in $(seq "$n"); do
		t new-window -d -t "$s:" -c "$dir" sh
	done
done
t send-keys -t s2:1 'while sleep 3; do date; done' Enter

# A real client in a pty, so the daemon sees a shown window; control clients don't count.
python3 -I -c '
import os, pty, sys, fcntl, termios, struct
pid, fd = pty.fork()
if pid == 0:
    os.execvp(sys.argv[1], sys.argv[1:])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
try:
    while os.read(fd, 65536):
        pass
except OSError:
    pass
' tmux -S "$sock" attach -t s1 >/dev/null 2>&1 &
client_pid=$!

"$bin" daemon --tmux-socket "$sock" </dev/null >"$dir/daemon.out" 2>&1 &
daemon_pid=$!

wait_for() {
	local what=$1
	shift
	for _ in $(seq 100); do
		"$@" >/dev/null 2>&1 && return 0
		sleep 0.1
	done
	echo "bench-sidebar: timed out waiting for $what" >&2
	exit 1
}
wait_for "the attached client" sh -c "tmux -S '$sock' list-clients -F '#{client_flags}' | grep -qv control-mode"
wait_for "the daemon socket" test -S "$SIMUL_SOCKET"
"$bin" toggle >/dev/null
sleep 5

server_pid=$(t display-message -p '#{pid}')

sidebar_pids() { pgrep -f "^$bin sidebar\$" | tr '\n' ' ' || true; }

# CPU seconds summed over a space-separated pid list; ps prints [[dd-]hh:]mm:ss.ss.
cpu() {
	local pids
	pids=$(echo "$1" | xargs | tr ' ' ',')
	[ -z "$pids" ] && {
		echo 0
		return
	}
	ps -o time= -p "$pids" | awk '
		{ n = split($1, f, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + f[i]; total += s }
		END { printf "%.2f\n", total }'
}

# Sidebars that start during the run (lazy) count from zero, so sample them at the end only;
# the ones present at the start are subtracted.
start_sidebars=$(sidebar_pids)
sidebar0=$(cpu "$start_sidebars")
daemon0=$(cpu "$daemon_pid")
server0=$(cpu "$server_pid")

elapsed=0
i=0
while [ "$elapsed" -lt "$secs" ]; do
	step=$((secs - elapsed < 10 ? secs - elapsed : 10))
	sleep "$step"
	elapsed=$((elapsed + step))
	i=$((i + 1))
	t select-window -t "s1:$((i % 4))"
done

end_sidebars=$(sidebar_pids)
sidebar1=$(cpu "$end_sidebars")
daemon1=$(cpu "$daemon_pid")
server1=$(cpu "$server_pid")
count=$(echo "$end_sidebars" | wc -w | tr -d ' ')
rss_mb=0
if [ "$count" -gt 0 ]; then
	rss_mb=$(ps -o rss= -p "$(echo "$end_sidebars" | xargs | tr ' ' ',')" | awk '{ t += $1 } END { printf "%.1f\n", t / 1024 }')
fi

awk -v l="$label" -v c="$count" -v s0="$sidebar0" -v s1="$sidebar1" -v d0="$daemon0" \
	-v d1="$daemon1" -v t0="$server0" -v t1="$server1" -v r="$rss_mb" 'BEGIN {
	s = s1 - s0; d = d1 - d0; t = t1 - t0
	printf "%s\t%d\t%.2f\t%.2f\t%.2f\t%.2f\t%s\n", l, c, s, d, t, s + d + t, r
}'

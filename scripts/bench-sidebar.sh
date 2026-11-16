#!/usr/bin/env bash
# Measures what shown sidebars cost on an isolated tmux server: 3 sessions with 13 windows,
# one attached client that switches windows every 10 seconds, and one Claude pane that sends
# a tool hook every 0.25 seconds. Sessions s1 and s2 sit in a git repo, s3 outside one.
# Usage: bench-sidebar.sh <simul-bin> [secs] [label]
# Prints one tab-separated row: label, sidebars, sidebar/daemon/tmux/total CPU seconds over
# the run, the CPU seconds of the daemon's child processes (tmux queries, git), and the
# sidebars' summed memory in MB: physical footprint where macOS's `footprint` exists, else
# RSS, which counts the shared binary once per sidebar. The children column covers the
# daemon's whole life, from its start to SIGTERM, as it logs it at exit; the rest cover the
# run alone. Hook processes and the shims tmux starts are not counted.

set -euo pipefail

usage="usage: bench-sidebar.sh <simul-bin> [secs] [label]"
bin=${1:?$usage}
secs=${2:-120}
label=${3:-$bin}

[ -x "$bin" ] || {
	echo "bench-sidebar: not an executable: $bin" >&2
	exit 1
}
# Sidebars are found by the binary's path in ps, as the daemon resolves it: absolute, with
# symlinks such as macOS's /tmp resolved.
bin=$(cd "$(dirname "$bin")" && pwd -P)/$(basename "$bin")

# Short, since tmux and simul socket paths have a length limit.
dir=$(mktemp -d /tmp/sb.XXXXXX)
chmod 700 "$dir"
sock=$dir/t.sock
daemon_pid=""
client_pid=""
hooks_pid=""
cleanup() {
	{
		[ -n "$hooks_pid" ] && kill "$hooks_pid"
		[ -n "$client_pid" ] && kill "$client_pid"
		tmux -S "$sock" kill-server
		[ -n "$daemon_pid" ] && kill "$daemon_pid"
		wait
	} 2>/dev/null
	rm -rf "$dir"
}
trap cleanup EXIT

mkdir -p "$dir/config/simul" "$dir/claude" "$dir/kiro" "$dir/repo" "$dir/hooks"
printf '[notify]\nenabled = false\n' >"$dir/config/simul/config.toml"
repo=$dir/repo
git -C "$repo" init -q
# A throwaway repo: the user's global git hooks have no business here.
git -C "$repo" -c core.hooksPath=/dev/null -c user.name=bench -c user.email=bench@localhost \
	commit -q --allow-empty -m init
touch "$repo/untracked"

# Claude's hook payloads, pointed at the repo and at an empty transcript.
fixtures="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/fixtures/claude/hooks"
: >"$dir/transcript.jsonl"
python3 -I - "$fixtures" "$dir/hooks" "$repo" "$dir/transcript.jsonl" <<'EOF'
import json, sys
src, dst, cwd, transcript = sys.argv[1:]
for event in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse"]:
    with open(f"{src}/{event}.json") as f:
        payload = json.load(f)
    payload["cwd"] = cwd
    payload["transcript_path"] = transcript
    with open(f"{dst}/{event}.json", "w") as f:
        json.dump(payload, f)
EOF

export SIMUL_SOCKET=$dir/s.sock
export XDG_CONFIG_HOME=$dir/config
export CLAUDE_CONFIG_DIR=$dir/claude
export KIRO_HOME=$dir/kiro
export TMUX="$sock,0,0"
unset TMUX_PANE

t() { tmux -S "$sock" "$@"; }

t -f /dev/null new-session -d -s s1 -x 200 -y 50 -c "$repo" sh
t new-session -d -s s2 -x 200 -y 50 -c "$repo" sh
t new-session -d -s s3 -x 200 -y 50 -c "$dir" sh
for s in s1 s2 s3; do
	n=3
	[ "$s" = s1 ] && n=4
	cwd=$repo
	[ "$s" = s3 ] && cwd=$dir
	for _ in $(seq "$n"); do
		t new-window -d -t "$s:" -c "$cwd" sh
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

agent_pane=$(t display-message -p -t s1:0 '#{pane_id}')
hook() { TMUX_PANE=$agent_pane "$bin" hook claude "$1" <"$dir/hooks/$1.json"; }
hook SessionStart
hook UserPromptSubmit
while :; do
	hook PreToolUse
	sleep 0.25
	hook PostToolUse
	sleep 0.25
done &
hooks_pid=$!
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

# A sidebar that starts during the run counts from zero; only those present at the start
# have their starting CPU subtracted.
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
# MB summed over a space-separated pid list.
mem() {
	local pid
	[ -z "$1" ] && {
		echo 0
		return
	}
	if command -v footprint >/dev/null; then
		for pid in $1; do
			footprint -p "$pid"
		done | awk '
			$1 == "phys_footprint:" { v = $2; if ($3 == "MB") v *= 1024; else if ($3 == "B") v /= 1024; t += v }
			END { printf "%.1f\n", t / 1024 }'
	else
		ps -o rss= -p "$(echo "$1" | xargs | tr ' ' ',')" | awk '{ t += $1 } END { printf "%.1f\n", t / 1024 }'
	fi
}
mem_mb=$(mem "$end_sidebars")

kill "$hooks_pid"
wait "$hooks_pid" 2>/dev/null || true
hooks_pid=""
# The daemon logs its own and its children's CPU as it exits.
kill "$daemon_pid"
wait "$daemon_pid" || true
daemon_pid=""
children=$(awk '/daemon cpu: own / { v = $NF } END { sub(/s$/, "", v); print v }' "$dir/s.log")
[ -n "$children" ] || {
	echo "bench-sidebar: no 'daemon cpu:' line in $dir/s.log; the daemon may predate it" >&2
	exit 1
}

awk -v l="$label" -v c="$count" -v s0="$sidebar0" -v s1="$sidebar1" -v d0="$daemon0" \
	-v d1="$daemon1" -v t0="$server0" -v t1="$server1" -v k="$children" -v r="$mem_mb" 'BEGIN {
	s = s1 - s0; d = d1 - d0; t = t1 - t0
	printf "%s\t%d\t%.2f\t%.2f\t%.2f\t%.2f\t%.2f\t%s\n", l, c, s, d, t, s + d + t, k, r
}'

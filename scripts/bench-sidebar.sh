#!/usr/bin/env bash
# Measures what shown sidebars cost on an isolated tmux server: 3 sessions with 13 windows,
# one attached client that switches windows every 10 seconds, and one Claude pane that sends
# a tool hook every 0.25 seconds. Sessions s1 and s2 sit in a git repo, s3 outside one.
# Usage: bench-sidebar.sh [-i] [-w windows] [-a agents] [-r secs] <simul-bin> [secs] [label]
# -i measures idle: each Claude pane starts its session and then sends no more hooks, and the
# client stays on one window.
# -w sets the number of windows (default 13), spread over the 3 sessions. -a sets the number
# of Claude panes (default 1), one in each of the first windows, each with its own git repo
# and hook loop. -r sets the seconds per tool call, a PreToolUse and a PostToolUse, in each
# loop (default 0.5). Tool calls alternate between Write and Read, so work done only after
# a write shows up.
# -H sends tool hooks as Claude's http hooks do, over [hooks] transport = "http": one python3
# sender POSTs them on a kept-alive connection, and its CPU counts as hook CPU, standing in
# for Claude's side. Session start and prompt hooks stay commands.
# Each agent gets a Claude session file (sessions/<n>.json) with its status, as Claude Code
# writes, so the daemon reads no screens. -S leaves them out: busy panes' screens are then
# read every second, as for an agent that writes none.
# Prints one tab-separated row: label, sidebars, sidebar/daemon/tmux/total CPU seconds over
# the run, the CPU seconds of the daemon's child processes (tmux queries, git), and the
# sidebars' summed memory in MB: physical footprint where macOS's `footprint` exists, else
# RSS, which counts the shared binary once per sidebar. The children column covers the
# daemon's whole life, from its start to SIGTERM, as it logs it at exit; the rest cover the
# run alone. Two more columns give the CPU seconds of the tool hook processes, over the
# hook loops' whole lives, and how many hooks the loops sent, both summed over the loops. The
# shims tmux starts are not counted.

set -euo pipefail

usage="usage: bench-sidebar.sh [-i] [-H] [-S] [-w windows] [-a agents] [-r secs] <simul-bin> [secs] [label]"
idle=false
windows=13
agents=1
rate=0.5
http=false
scrape=false
while getopts "iHSw:a:r:" opt; do
	case "$opt" in
	i) idle=true ;;
	H) http=true ;;
	S) scrape=true ;;
	w) windows=$OPTARG ;;
	a) agents=$OPTARG ;;
	r) rate=$OPTARG ;;
	*)
		echo "$usage" >&2
		exit 1
		;;
	esac
done
shift $((OPTIND - 1))
bin=${1:?$usage}
secs=${2:-120}
label=${3:-$bin}

case "$windows$agents" in
*[!0-9]*)
	echo "bench-sidebar: -w and -a take whole numbers: $windows $agents" >&2
	exit 1
	;;
esac
[ "$windows" -ge 3 ] && [ "$agents" -ge 1 ] && [ "$agents" -le "$windows" ] || {
	echo "bench-sidebar: need 3 <= windows and 1 <= agents <= windows, not $windows and $agents" >&2
	exit 1
}
# Half a tool call between hooks; awk also rejects a rate that is not a positive number.
half=$(awk -v r="$rate" 'BEGIN { if (r + 0 <= 0) exit 1; printf "%.3f\n", r / 2 }') || {
	echo "bench-sidebar: -r takes a positive number of seconds, not $rate" >&2
	exit 1
}

[ -f "$bin" ] && [ -x "$bin" ] || {
	echo "bench-sidebar: not an executable file: $bin" >&2
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
hooks_pids=()
sender_pid=""
# Stops the hook loops and waits for them. Killing one would leave a hook in flight, which
# starts a new daemon if it finds the old one gone.
stop_hooks() {
	[ "${#hooks_pids[@]}" -gt 0 ] || return 0
	: >"$dir/stop-hooks"
	local pid
	for pid in "${hooks_pids[@]}"; do
		wait "$pid" || true
	done
	hooks_pids=()
}
cleanup() {
	{
		stop_hooks
		[ -n "$client_pid" ] && kill "$client_pid"
		[ -n "$sender_pid" ] && kill "$sender_pid"
		tmux -S "$sock" kill-server
		[ -n "$daemon_pid" ] && kill "$daemon_pid"
		wait
	} 2>/dev/null
	rm -rf "$dir"
}
trap cleanup EXIT

mkdir -p "$dir/config/simul" "$dir/claude" "$dir/kiro" "$dir/repo" "$dir/hooks"
printf '[notify]\nenabled = false\n' >"$dir/config/simul/config.toml"
if $http; then
	# A port free a moment ago; the daemon retries if something takes it first.
	port=$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
	printf '[hooks]\ntransport = "http"\nhttp_port = %s\n' "$port" >>"$dir/config/simul/config.toml"
fi
repo=$dir/repo
git -C "$repo" init -q
# A throwaway repo: the user's global git hooks have no business here.
git -C "$repo" -c core.hooksPath=/dev/null -c user.name=bench -c user.email=bench@localhost \
	commit -q --allow-empty -m init
touch "$repo/untracked"

# Each agent after the first works in a repo of its own, so each has its own git status to read.
agent_repo() {
	if [ "$1" -eq 1 ]; then
		echo "$repo"
		return
	fi
	local r=$dir/repos/a$1
	mkdir -p "$r"
	git -C "$r" init -q
	git -C "$r" -c core.hooksPath=/dev/null -c user.name=bench -c user.email=bench@localhost \
		commit -q --allow-empty -m init
	touch "$r/untracked"
	echo "$r"
}

# Claude's hook payloads for each agent, pointed at its repo and at an empty transcript.
fixtures="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/fixtures/claude/hooks"
: >"$dir/transcript.jsonl"
for a in $(seq "$agents"); do
	mkdir -p "$dir/hooks/a$a"
	python3 -I - "$fixtures" "$dir/hooks/a$a" "$(agent_repo "$a")" "$dir/transcript.jsonl" "$a" <<'EOF'
import json, sys
src, dst, cwd, transcript, n = sys.argv[1:]
for event in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse"]:
    with open(f"{src}/{event}.json") as f:
        payload = json.load(f)
    payload["cwd"] = cwd
    payload["transcript_path"] = transcript
    payload["session_id"] = f"bench-{n}"
    with open(f"{dst}/{event}.json", "w") as f:
        json.dump(payload, f)
    if event.endswith("ToolUse"):
        payload["tool_name"] = "Read"
        payload["tool_input"] = {"file_path": f"{cwd}/untracked"}
        payload.pop("tool_response", None)
        with open(f"{dst}/{event}-Read.json", "w") as f:
            json.dump(payload, f)
EOF
done

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
# Each session already has one window; s1, then s2, take the remainder of an uneven split.
per=$((windows / 3))
extra=$((windows % 3))
s1_windows=$((per + (extra >= 1)))
s2_windows=$((per + (extra >= 2)))
for s in s1 s2 s3; do
	n=$((per - 1))
	[ "$s" = s1 ] && n=$((per - 1 + (extra >= 1)))
	[ "$s" = s2 ] && n=$((per - 1 + (extra >= 2)))
	cwd=$repo
	[ "$s" = s3 ] && cwd=$dir
	# Not `seq`: macOS's `seq 1 0` counts down, so a session wanting none would get two.
	for ((k = 0; k < n; k++)); do
		t new-window -d -t "$s:" -c "$cwd" sh
	done
done
# The agents' panes, one per window in s1, s2, s3 order, read before sidebars join them,
# with where each is as Claude's session file names it: <session>:<window id>.<pane id>.
agent_panes=()
agent_places=()
while read -r p place; do
	agent_panes+=("$p")
	agent_places+=("$place")
done < <(t list-panes -a -F '#{pane_id} #{session_name}:#{window_id}.#{pane_id}' | head -n "$agents")
if ! $scrape; then
	mkdir -p "$dir/claude/sessions"
	status=busy
	$idle && status=idle
	now_ms=$(python3 -I -c 'import time; print(int(time.time() * 1000))')
	for a in $(seq "$agents"); do
		# This script's pid stands in for Claude's: alive for the whole run.
		printf '{"pid":%d,"sessionId":"bench-%d","tmux":"%s","status":"%s","statusUpdatedAt":%s}\n' \
			$$ "$a" "${agent_places[$a - 1]}" "$status" "$now_ms" >"$dir/claude/sessions/$a.json"
	done
fi
# Output in a window nobody views: s2:1, as before -w, unless s2 has only one window.
t send-keys -t "s2:$((s2_windows > 1 ? 1 : 0))" 'while sleep 3; do date; done' Enter

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

if $http; then
	wait_for "the HTTP hook port" python3 -I -c 'import socket, sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1)' "$port"
	mkfifo "$dir/hookq"
	# Reads "<pane> <event> <payload file>" lines until the queue closes.
	python3 -I -c '
import http.client, sys
port, tmux = int(sys.argv[1]), sys.argv[2]
conn = http.client.HTTPConnection("127.0.0.1", port)
for line in sys.stdin:
    pane, event, path = line.split()
    with open(path, "rb") as f:
        body = f.read()
    headers = {"X-Simul-Pane": pane, "X-Simul-Tmux": tmux, "Content-Type": "application/json"}
    for attempt in range(2):
        try:
            conn.request("POST", f"/simul/hook/claude/{event}", body, headers)
            resp = conn.getresponse()
            resp.read()
            if resp.status != 200:
                print(f"bench-sidebar: HTTP hook {event} got {resp.status}", file=sys.stderr)
            break
        except (OSError, http.client.HTTPException) as e:
            conn.close()
            if attempt:
                print(f"bench-sidebar: HTTP hook {event} failed: {e}", file=sys.stderr)
' "$port" "$TMUX" <"$dir/hookq" &
	sender_pid=$!
	# Held open so the sender sees one stream, not an end after each writer.
	exec 3>"$dir/hookq"
fi

# Sends agent $1's tool hook $2, with the payload variant $3 if given: queued for the HTTP
# sender with -H, else as `hook` does. printf is a builtin, so queueing starts no process.
tool_hook() {
	if $http; then
		printf '%s %s %s\n' "${agent_panes[$1 - 1]}" "$2" "$dir/hooks/a$1/$2${3:+-$3}.json" >&3
	else
		hook "$@"
	fi
}
# Sends agent $1's hook $2, with the payload variant $3 if given. A failed hook is reported,
# and the load goes on.
hook() {
	TMUX_PANE=${agent_panes[$1 - 1]} "$bin" hook claude "$2" <"$dir/hooks/a$1/$2${3:+-$3}.json" ||
		echo "bench-sidebar: agent $1 hook $2 failed" >&2
}
for a in $(seq "$agents"); do
	hook "$a" SessionStart
done
if ! $idle; then
	for a in $(seq "$agents"); do
		hook "$a" UserPromptSubmit
		(
			calls=0
			tool=""
			while [ ! -e "$dir/stop-hooks" ]; do
				tool_hook "$a" PreToolUse "$tool"
				sleep "$half"
				tool_hook "$a" PostToolUse "$tool"
				sleep "$half"
				calls=$((calls + 2))
				# Empty is the fixture's Write.
				if [ -z "$tool" ]; then tool=Read; else tool=""; fi
			done
			# The second line of `times` is the CPU of this subshell's children: the hooks, plus
			# the sleeps, which cost next to nothing. Not piped: a pipeline would run `times` in
			# a fresh subshell with no children.
			times >"$dir/times-$a"
			awk -v n="$calls" '
				NR == 2 {
					t = 0
					for (i = 1; i <= 2; i++) { split($i, f, /[ms]/); t += f[1] * 60 + f[2] }
					printf "%.2f\t%d\n", t, n
				}' "$dir/times-$a" >"$dir/hook-cpu-$a"
		) &
		hooks_pids+=($!)
	done
fi
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
	$idle || t select-window -t "s1:$((i % (s1_windows < 4 ? s1_windows : 4)))"
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

stop_hooks
sender_cpu=0
if [ -n "$sender_pid" ]; then
	sender_cpu=$(cpu "$sender_pid")
	exec 3>&-
	wait "$sender_pid" || true
	sender_pid=""
fi
# Idle runs start no loops, so there may be no files; awk then reads /dev/null alone.
hook_files=(/dev/null)
for f in "$dir"/hook-cpu-*; do
	[ -e "$f" ] && hook_files+=("$f")
done
hook_cpu=$(awk -F '\t' -v s="$sender_cpu" '{ t += $1; n += $2 } END { printf "%.2f\t%d\n", t + s, n }' "${hook_files[@]}")
# The daemon logs its own and its children's CPU as it exits.
kill "$daemon_pid"
wait "$daemon_pid" || true
daemon_pid=""
children=$(awk '/daemon cpu: own / { v = $NF } END { sub(/s$/, "", v); print v }' "$dir/s.log")
[ -n "$children" ] || {
	echo "bench-sidebar: no 'daemon cpu:' line in $dir/s.log; the daemon may predate it" >&2
	exit 1
}

awk -v l="$label" -v c="$count" -v h="$hook_cpu" -v s0="$sidebar0" -v s1="$sidebar1" -v d0="$daemon0" \
	-v d1="$daemon1" -v t0="$server0" -v t1="$server1" -v k="$children" -v r="$mem_mb" 'BEGIN {
	s = s1 - s0; d = d1 - d0; t = t1 - t0
	printf "%s\t%d\t%.2f\t%.2f\t%.2f\t%.2f\t%.2f\t%s\t%s\n", l, c, s, d, t, s + d + t, k, r, h
}'

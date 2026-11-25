#!/usr/bin/env bash
# Runs bench-sidebar.sh for each build, interleaved, and prints each column's median.
# Usage: bench-all.sh [-i] [-H] [-n runs] [-s secs] [-w windows] [-a agents] [-r secs] <label=simul-bin>...
#   -i, -H, -w, -a and -r go to bench-sidebar.sh, which describes them
#   e.g. bench-all.sh before=/tmp/old/target/release/simul after=target/release/simul

set -euo pipefail

usage="usage: bench-all.sh [-i] [-H] [-n runs] [-s secs] [-w windows] [-a agents] [-r secs] <label=simul-bin>..."
runs=3
secs=120
# Options for bench-sidebar.sh, passed through as given.
pass=()
while getopts "iHn:s:w:a:r:" opt; do
	case "$opt" in
	i | H) pass+=("-$opt") ;;
	w | a | r) pass+=("-$opt" "$OPTARG") ;;
	n) runs=$OPTARG ;;
	s) secs=$OPTARG ;;
	*)
		echo "$usage" >&2
		exit 1
		;;
	esac
done
shift $((OPTIND - 1))
[ $# -gt 0 ] || {
	echo "$usage" >&2
	exit 1
}
for build in "$@"; do
	case "$build" in
	?*=?*) ;;
	*)
		echo "bench-all: expected label=simul-bin, not $build" >&2
		exit 1
		;;
	esac
done
bench="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/bench-sidebar.sh"

rows=$(mktemp)
trap 'rm -f "$rows"' EXIT

for i in $(seq "$runs"); do
	# Interleaved, so drift in machine load spreads over every build.
	for build in "$@"; do
		"$bench" ${pass[@]+"${pass[@]}"} "${build#*=}" "$secs" "${build%%=*}" >>"$rows"
	done
	echo "bench-all: run $i of $runs done" >&2
done

echo "raw rows:"
cat "$rows"
echo
printf 'median of %s runs, %ss each\n' "$runs" "$secs"
printf '%-10s %8s %12s %11s %9s %10s %13s %12s %9s %10s\n' build sidebars "sidebar cpu" "daemon cpu" "tmux cpu" "total cpu" "children cpu" "sidebar mem" "hook cpu" "hook calls"
for build in "$@"; do
	awk -F '\t' -v v="${build%%=*}" '
		$1 == v { n++; for (c = 2; c <= 10; c++) col[c, n] = $c + 0 }
		function median(c,   i, j, t, a) {
			for (i = 1; i <= n; i++) a[i] = col[c, i]
			for (i = 2; i <= n; i++) for (j = i; j > 1 && a[j - 1] > a[j]; j--) { t = a[j]; a[j] = a[j - 1]; a[j - 1] = t }
			return n % 2 ? a[(n + 1) / 2] : (a[n / 2] + a[n / 2 + 1]) / 2
		}
		END {
			printf "%-10s %8d %11.2fs %10.2fs %8.2fs %9.2fs %12.2fs %9.1f MB %8.2fs %10d\n", v, median(2), median(3), median(4), median(5), median(6), median(7), median(8), median(9), median(10)
		}' "$rows"
done

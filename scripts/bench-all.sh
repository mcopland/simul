#!/usr/bin/env bash
# Runs bench-sidebar.sh for four sidebar variants and prints each column's median.
# Usage: bench-all.sh <base-bin> <lazy-bin> <both-bin> [runs] [secs]
#   base-bin: a build before either change ("current")
#   lazy-bin: a build with the lazy option but the old redraw loop ("3 only")
#   both-bin: a build with both changes; run with lazy off ("1 only") and on ("1 + 3")

set -euo pipefail

usage="usage: bench-all.sh <base-bin> <lazy-bin> <both-bin> [runs] [secs]"
base=${1:?$usage}
lazy=${2:?$usage}
both=${3:?$usage}
runs=${4:-3}
secs=${5:-120}
bench="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/bench-sidebar.sh"

rows=$(mktemp)
trap 'rm -f "$rows"' EXIT

for i in $(seq "$runs"); do
	# Interleaved, so drift in machine load spreads over every variant.
	{
		"$bench" "$base" none "$secs" current
		"$bench" "$both" false "$secs" "1 only"
		"$bench" "$lazy" true "$secs" "3 only"
		"$bench" "$both" true "$secs" "1 + 3"
	} >>"$rows"
	echo "bench-all: run $i of $runs done" >&2
done

echo "raw rows:"
cat "$rows"
echo
printf 'median of %s runs, %ss each\n' "$runs" "$secs"
printf '%-8s %8s %12s %11s %9s %10s %12s\n' variant sidebars "sidebar cpu" "daemon cpu" "tmux cpu" "total cpu" "sidebar rss"
for v in current "1 only" "3 only" "1 + 3"; do
	awk -F '\t' -v v="$v" '
		$1 == v { n++; for (c = 2; c <= 7; c++) col[c, n] = $c + 0 }
		function median(c,   i, j, t, a) {
			for (i = 1; i <= n; i++) a[i] = col[c, i]
			for (i = 2; i <= n; i++) for (j = i; j > 1 && a[j - 1] > a[j]; j--) { t = a[j]; a[j] = a[j - 1]; a[j - 1] = t }
			return n % 2 ? a[(n + 1) / 2] : (a[n / 2] + a[n / 2 + 1]) / 2
		}
		END {
			printf "%-8s %8d %11.2fs %10.2fs %8.2fs %9.2fs %9.1f MB\n", v, median(2), median(3), median(4), median(5), median(6), median(7)
		}' "$rows"
done

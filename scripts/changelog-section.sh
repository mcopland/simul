#!/usr/bin/env bash
# Prints the body of a version's section in CHANGELOG.md, for release notes.
# Usage: changelog-section.sh <version> [changelog]

set -euo pipefail

version=${1:?usage: changelog-section.sh <version> [changelog]}
file=${2:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/CHANGELOG.md"}

[ -r "$file" ] || {
	echo "changelog-section: cannot read $file" >&2
	exit 1
}

# The section runs from its "## [<version>]" heading to the next heading or the link
# references at the end; blank lines at either end are dropped.
body=$(awk -v want="## [$version]" '
	index($0, want) == 1 && (length($0) == length(want) || substr($0, length(want) + 1, 1) == " ") { on = 1; next }
	on && (/^## \[/ || /^\[[^]]*\]: /) { exit }
	on { print }
' "$file" | sed -e '/./,$!d')

if [ -z "$body" ]; then
	echo "changelog-section: no section for $version in $file; add a \"## [$version]\" heading" >&2
	exit 1
fi
printf '%s\n' "$body"

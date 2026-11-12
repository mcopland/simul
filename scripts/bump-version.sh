#!/usr/bin/env bash
# Sets the release version in VERSION, Cargo.toml, and Cargo.lock, moves the Unreleased notes
# in CHANGELOG.md under the new version, and commits. It does not tag or push.
# Usage: scripts/bump-version.sh <version>

set -euo pipefail

die() {
	echo "bump-version: $*" >&2
	exit 1
}

new=${1:-}
[[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
	die "usage: bump-version.sh <version>, such as 0.11.0 (got '${new}')"

cd "$(dirname "${BASH_SOURCE[0]}")/.."
[ -z "$(git status --porcelain)" ] ||
	die "the working tree has uncommitted changes; commit or stash them first"

old=$(tr -d '[:space:]' <VERSION)
[ "$new" != "$old" ] || die "VERSION is already $new"

# Text between "## [Unreleased]" and the next heading, minus blank lines.
notes=$(awk '/^## \[Unreleased\]/ { on = 1; next } on && /^(## \[|\[[^]]*\]: )/ { exit } on && NF' CHANGELOG.md)
[ -n "$notes" ] ||
	die "CHANGELOG.md has no notes under ## [Unreleased]; add them before releasing $new"

# Overwrites file $1 with stdin. Reading all of stdin first lets a pipeline read the same
# file, and writing in place keeps the file's mode.
replace() {
	local text
	text=$(cat)
	printf '%s\n' "$text" >"$1"
}

printf '%s\n' "$new" >VERSION

# Only the package version: the first version key, inside [package].
awk -v v="$new" '
	/^\[/ { in_package = ($0 == "[package]") }
	in_package && !done && /^version = "/ { print "version = \"" v "\""; done = 1; next }
	{ print }
' Cargo.toml | replace Cargo.toml
grep -qx "version = \"$new\"" Cargo.toml || die "failed to set the version in Cargo.toml"

cargo update --workspace --offline --quiet ||
	die "cargo update failed to record $new in Cargo.lock"

today=$(date +%Y-%m-%d)
repo=https://github.com/mcopland/simul
awk -v new="$new" -v old="$old" -v today="$today" -v repo="$repo" '
	/^## \[Unreleased\]/ { print; print ""; print "## [" new "] - " today; next }
	/^\[Unreleased\]: / {
		print "[Unreleased]: " repo "/compare/v" new "...HEAD"
		print "[" new "]: " repo "/compare/v" old "...v" new
		next
	}
	{ print }
' CHANGELOG.md | replace CHANGELOG.md

git add VERSION Cargo.toml Cargo.lock CHANGELOG.md
git commit --quiet -m "chore(release): bump version to $new"
echo "committed $old -> $new; tag it with: git tag v$new && git push origin main v$new"

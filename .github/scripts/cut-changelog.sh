#!/usr/bin/env bash
# Retitle `## Unreleased` in CHANGELOG.md to `## vN - YYYY-MM-DD`, add a new
# empty `## Unreleased` above it, and commit. Read the notes before you push.
#
# Usage: .github/scripts/cut-changelog.sh vN
set -euo pipefail

tag=${1:?usage: cut-changelog.sh vN}
[[ "$tag" =~ ^v[0-9]+$ ]] || { echo "error: expected vN, got '$tag'" >&2; exit 1; }

cd "$(git rev-parse --show-toplevel)"
grep -q '^## Unreleased$' CHANGELOG.md || { echo "error: no '## Unreleased' in CHANGELOG.md" >&2; exit 1; }
grep -q "^## $tag " CHANGELOG.md && { echo "error: '## $tag' already exists" >&2; exit 1; }

heading="## $tag - $(date -u +%F)"
awk -v h="$heading" '!done && $0 == "## Unreleased" { print; print ""; print h; done = 1; next } { print }' \
  CHANGELOG.md > CHANGELOG.md.tmp
mv CHANGELOG.md.tmp CHANGELOG.md

git commit -q -m "doc(changelog): $tag release notes" -- CHANGELOG.md
git show --stat HEAD

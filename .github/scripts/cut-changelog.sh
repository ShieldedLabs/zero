#!/usr/bin/env bash
# Retitle `## Unreleased` in CHANGELOG.md to `## vN - YYYY-MM-DD`, add a new
# empty `## Unreleased` above it, and commit. Read the notes before you push.
#
# Usage: .github/scripts/cut-changelog.sh vN
set -euo pipefail

tag=${1:?usage: cut-changelog.sh vN}
[[ "$tag" =~ ^v[0-9]+$ ]] || { echo "error: expected vN, got '$tag'" >&2; exit 1; }

cd "$(git rev-parse --show-toplevel)"
if [ -n "$(git status --porcelain --untracked-files=no -- . ':!CHANGELOG.md')" ]; then
  echo "error: commit the halt-height edits first, other tracked files are dirty" >&2
  git status --short --untracked-files=no >&2
  exit 1
fi
grep -q '^## Unreleased$' CHANGELOG.md || { echo "error: no '## Unreleased' in CHANGELOG.md" >&2; exit 1; }
grep -Eq "^## $tag( |$)" CHANGELOG.md && { echo "error: '## $tag' already exists" >&2; exit 1; }

heading="## $tag - $(date -u +%F)"
awk -v h="$heading" '!done && $0 == "## Unreleased" { print; print ""; print h; done = 1; next } { print }' \
  CHANGELOG.md > CHANGELOG.md.tmp
mv CHANGELOG.md.tmp CHANGELOG.md

git commit -q -m "doc(changelog): $tag release notes" -- CHANGELOG.md
git log -1 --oneline
awk -v h="$heading" '$0 == h { on = 1 } on && /^## / && $0 != h { exit } on' CHANGELOG.md

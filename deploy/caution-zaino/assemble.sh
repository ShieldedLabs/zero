#!/usr/bin/env bash
# Assemble the Caution build-context repo: the zaino source (subtree split of
# the zaino/ prefix, preserving upstream history and our [zero] carries) plus
# the overlay (Containerfile, caution.hcl, baked config profiles) committed on
# top. The result is a small standalone repo whose ROOT is what Caution
# docker-builds; its main branch is rebuilt on every run, so pushes force.
#
# Usage: deploy/caution-zaino/assemble.sh [dest-dir]
#        default dest: ../zaino-caution next to the zero checkout

set -euo pipefail

ZERO_ROOT=$(git rev-parse --show-toplevel)
OVERLAY="$ZERO_ROOT/deploy/caution-zaino/overlay"
DEST=${1:-"$(dirname "$ZERO_ROOT")/zaino-caution"}
SHA=$(git -C "$ZERO_ROOT" rev-parse --short HEAD)
SPLIT_BRANCH="caution-zaino-split"

if ! git -C "$ZERO_ROOT" diff --quiet -- zaino deploy/caution-zaino; then
  echo "WARN: uncommitted changes under zaino/ or deploy/caution-zaino/; the split uses HEAD only" >&2
fi

echo "==> splitting zaino/ prefix from zero@$SHA (first run can take a minute)"
git -C "$ZERO_ROOT" branch -D "$SPLIT_BRANCH" >/dev/null 2>&1 || true
# --ignore-joins: our subtree imports are squashed, so the recorded upstream
# split hashes only resolve in clones that happen to have the upstream remote
# fetched. Ignoring joins splits purely from the path-filtered zero history
# (squash snapshots plus [zero] carries), which works on any fresh clone.
git -C "$ZERO_ROOT" subtree split --prefix=zaino --ignore-joins HEAD -b "$SPLIT_BRANCH" >/dev/null

mkdir -p "$DEST"
if [ ! -d "$DEST/.git" ]; then
  git -C "$DEST" init -q -b main
fi
git -C "$DEST" fetch -q "$ZERO_ROOT" "$SPLIT_BRANCH"
git -C "$DEST" checkout -q -B main FETCH_HEAD

cp -R "$OVERLAY/." "$DEST/"
git -C "$DEST" add -A
if git -C "$DEST" diff --cached --quiet; then
  echo "==> overlay unchanged; nothing to commit"
else
  git -C "$DEST" -c user.name="zero-assemble" -c user.email="eng@shieldedlabs.net" \
    commit -q -m "caution build context from zero@$SHA"
fi

echo "==> assembled: $DEST"
echo "verify: docker build -f Containerfile $DEST"
echo "push:   git -C $DEST remote add caution <caution-remote>   (first time)"
echo "        git -C $DEST push caution main --force"

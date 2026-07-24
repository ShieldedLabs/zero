#!/usr/bin/env bash
# Assemble the combined Caution build context: zebra/ and zaino/ as subfolders
# (Anton's requested layout) plus the overlay (Containerfile, run-both.sh,
# configs). git archive is used rather than subtree split because this context
# is a throwaway build input, not a repo whose history we push; the whole tree
# comes from the zero HEAD so provenance is the recorded SHA.
#
# Usage: deploy/caution-zaino/combined/assemble-combined.sh [dest-dir]

set -euo pipefail

ZERO_ROOT=$(git rev-parse --show-toplevel)
HERE="$ZERO_ROOT/deploy/caution-zaino/combined"
DEST=${1:-"$(dirname "$ZERO_ROOT")/z3-enclave"}
SHA=$(git -C "$ZERO_ROOT" rev-parse --short HEAD)

echo "==> assembling combined context from zero@$SHA into $DEST"
rm -rf "$DEST"
mkdir -p "$DEST/zebra" "$DEST/zaino" "$DEST/orchard" "$DEST/config"

git -C "$ZERO_ROOT" archive HEAD:zebra | tar -x -C "$DEST/zebra"
git -C "$ZERO_ROOT" archive HEAD:zaino | tar -x -C "$DEST/zaino"
# zebra carries a [zero] patch orchard = { path = "../orchard" }, so the
# vendored orchard must sit beside zebra/ in the context (Containerfile copies
# it to /home/orchard where the patch path resolves).
git -C "$ZERO_ROOT" archive HEAD:orchard | tar -x -C "$DEST/orchard"

cp "$HERE/Containerfile" "$DEST/Containerfile"
cp "$HERE/run-both.sh" "$DEST/run-both.sh"
cp "$HERE/caution.hcl" "$DEST/caution.hcl"
cp "$HERE/zainod-colocated.toml" "$DEST/config/zainod-colocated.toml"
# The zebra-side config contract becomes the baked zebrad.toml.
cp "$HERE/zebrad-contract.toml" "$DEST/config/zebrad.toml"

echo "==> assembled: $DEST"
echo "verify (x86): docker build -f Containerfile $DEST"

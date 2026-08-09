#!/bin/sh
# Assemble the zero-indexer-hub build context: a partial mirror of the zero repo
# containing the hub plus exactly the parts of zebra/ and zaino/ its path
# dependencies need. Sibling of zeronym/shim/deploy/assemble.sh, and now the same
# shape: the hub broadcasts through an indexer's CompactTxStreamer, so it carries
# zaino-proto just as the shim does.
#
# Everything comes out of `git archive HEAD`, never the working tree. Three
# reasons, all of them load-bearing:
#   1. Provenance: the context is exactly the recorded SHA, so "rebuild from
#      source" has an unambiguous meaning. That includes the Containerfile,
#      which IS the build; see the recipe-provenance block near the bottom.
#   2. The vendored zebra/ subtree CANNOT be dirtied, because the working copy
#      is never touched. Vendored trees are read-only for us.
#   3. Determinism: git archive stamps every entry with the commit timestamp,
#      so file mtimes in the context are identical on every machine.
#
# No network access is required.
#
# PORTABILITY: this is POSIX sh, deliberately. Callers invoke it as
# `sh assemble.sh`, which bypasses any shebang, and on Debian and Ubuntu /bin/sh
# is dash. dash has no `-o pipefail`, so this uses `set -eu` and no pipelines:
# each `git archive` writes a tar file whose exit status `set -e` actually
# checks. Do not reintroduce a pipeline here without pipefail, and do not
# reintroduce pipefail.
#
# Usage: sh zeronym/hub/deploy/assemble.sh [dest-dir]

set -eu

# Context file modes must not depend on who runs the script (see the shim's
# assemble.sh for the full argument): git archive records the committed mode but
# `tar -x` masks it with the invoking user's umask. Belt and braces: umask here,
# `-p` on every extraction below.
umask 022

ZERO_ROOT=$(git rev-parse --show-toplevel)
HERE="$ZERO_ROOT/zeronym/hub/deploy"
DEST=${1:-"$(dirname "$ZERO_ROOT")/zero-indexer-hub-ctx"}
SHA=$(git -C "$ZERO_ROOT" rev-parse --short HEAD)

echo "==> assembling zero-indexer-hub context from zero@$SHA into $DEST"
rm -rf "$DEST"
mkdir -p "$DEST"

STAGE=$(mktemp -d)
# shellcheck disable=SC2064
trap "rm -rf '$STAGE'" EXIT INT TERM

# The crate. Its manifest path-depends on ../../zebra/zebra-chain and
# ../../zaino/packages/zaino-proto, so the context keeps the repo's own layout and the Containerfile reconstructs it under
# /usr/src/app. No manifest is edited, here or in the image.
#
# This also carries deploy/ itself, which is how the build gets a commit-pinned
# copy of its own Containerfile.
git -C "$ZERO_ROOT" archive HEAD -o "$STAGE/hub.tar" zeronym/hub
tar -xpf "$STAGE/hub.tar" -C "$DEST"

# zebra: the workspace root manifest plus two member directories. Not the tree.
#
#   Cargo.toml   REQUIRED. zebra-chain inherits roughly fifty dependency entries
#                plus authors/license/edition/rust-version and [lints] from
#                [workspace.*]. Without it, cargo cannot parse zebra-chain's
#                manifest.
#   zebra-chain  the actual dependency, the vendored Zcash parser.
#   zebra-test   REQUIRED, AND NEVER COMPILED. It is an optional dependency of
#                zebra-chain that only appears under dev-dependencies, but cargo
#                must still load its manifest to RESOLVE the graph. Dropping it
#                fails at resolution, not compile time. It costs context bytes
#                and zero build time. Do not optimise it away.
#
# The other members listed in zebra/Cargo.toml are not needed: cargo only reads
# the workspace root for inheritance here, and does not require absent members
# to exist.
#
# Keep this list, and the zaino one below, in sync with the `paths:` filter of
# .github/workflows/zeronym-hub-reproduce.yml. A subtree pull that touches one
# of these directories changes the binary and therefore the recorded hash, so
# the reproduce job has to fire on it.
git -C "$ZERO_ROOT" archive HEAD -o "$STAGE/zebra.tar" \
	zebra/Cargo.toml \
	zebra/zebra-chain \
	zebra/zebra-test
tar -xpf "$STAGE/zebra.tar" -C "$DEST"

# zaino: same shape. zaino-proto inherits authors/repository/homepage/edition/
# license and its tonic and prost versions from [workspace.*], so the root
# manifest is required; the other members are not.
#
# zaino-proto/proto/{compact_formats,service}.proto are git-tracked SYMLINKS into
# ../lightwallet-protocol/walletrpc/. git archive reproduces both the links and
# their targets, so copying the crate directory is sufficient.
git -C "$ZERO_ROOT" archive HEAD -o "$STAGE/zaino.tar" \
	zaino/Cargo.toml \
	zaino/packages/zaino-proto
tar -xpf "$STAGE/zaino.tar" -C "$DEST"

# Deliberately NOT in the context: orchard/. zebra/Cargo.toml carries a [zero]
# patch `orchard = { path = "../orchard" }`, but that patch belongs to zebra's
# workspace and does not apply to the hub's own; the hub resolves orchard from
# crates.io per its committed Cargo.lock, and cargo does not complain about the
# dangling patch path when it merely loads zebra's manifest for inheritance.
#
# Also not needed: zebra/Cargo.lock or either tree's rust-toolchain.toml. The
# hub's own lockfile is authoritative, and the pinned pallet-rust digest is the
# toolchain.

############################################################
# Recipe provenance
############################################################
# The Containerfile is the entire definition of the build, so it is the one file
# in the context most worth tampering with, and therefore the one that must come
# from HEAD rather than from the working tree. It arrives via the `git archive
# HEAD zeronym/hub` above, at $DEST/zeronym/hub/deploy/Containerfile, and every
# caller builds with `-f` pointed at THAT path. There is deliberately no copy at
# the context root: a second, working-tree-sourced copy is exactly the hole this
# closes.
RECIPE="$DEST/zeronym/hub/deploy/Containerfile"

if [ -f "$RECIPE" ]; then
	# Committed. Warn if the working tree has drifted, because the build will
	# silently use HEAD's version and someone iterating on the recipe needs to
	# know why their edit had no effect.
	if ! cmp -s "$HERE/Containerfile" "$RECIPE"; then
		echo "note: your working-tree Containerfile differs from HEAD's."
		echo "      The build uses HEAD's copy, by design. Commit to test edits."
	fi
else
	# Not committed yet. Fall back to the working tree so the recipe can be
	# developed at all, but say so loudly: a hash produced this way is not
	# pinned to a commit and must not be published as one.
	echo
	echo "############################################################"
	echo "# WARNING: zeronym/hub/deploy/ is NOT COMMITTED at $SHA."
	echo "# Falling back to the working-tree copy of deploy/."
	echo "# The context is therefore NOT fully commit-pinned, and a hash"
	echo "# produced from it is not reproducible by a third party, who"
	echo "# would check out $SHA and find no deploy/ at all."
	echo "# Commit zeronym/hub/deploy/ before publishing any hash."
	echo "############################################################"
	echo
	mkdir -p "$DEST/zeronym/hub/deploy"
	cp "$HERE/Containerfile" "$RECIPE"
	# Mirror the rest of deploy/ too, so the context has the same shape it will
	# have once committed. None of it is compiled (cargo builds src/ and the
	# manifest; deploy/ is inert), which is what makes the recorded hash survive
	# committing this directory.
	for f in "$HERE"/*; do
		[ -f "$f" ] || continue
		cp "$f" "$DEST/zeronym/hub/deploy/"
	done
fi

echo "==> assembled: $DEST ($(du -sh "$DEST" | cut -f1))"
echo "recipe: $RECIPE"
echo "verify: docker build --platform linux/amd64 -f $RECIPE $DEST"

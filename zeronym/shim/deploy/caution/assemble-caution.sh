#!/bin/sh
# Assemble the Caution deploy repository for zero-indexer-shim.
#
# A Caution app is a git repository you push to; whatever is at its root is what
# gets built into an EIF. So this produces exactly that: the reproducible build
# context, plus caution.hcl and a Containerfile at the root where Caution looks
# for them.
#
# Everything comes from `git archive HEAD` by way of deploy/assemble.sh. Nothing
# is read from the working tree, which is what makes "the enclave runs the code
# at commit X" a checkable statement rather than a hope.
#
# POSIX sh with no pipelines, for the reason recorded in assemble.sh: /bin/sh is
# dash on Debian and Ubuntu, dash has no `-o pipefail`, and a pipeline without
# it hides the exit status of everything but the last command.
#
# Usage: sh zeronym/shim/deploy/caution/assemble-caution.sh [dest-dir]

set -eu

umask 022

ZERO_ROOT=$(git rev-parse --show-toplevel)
HERE="$ZERO_ROOT/zeronym/shim/deploy/caution"
DEST=${1:-"$(dirname "$ZERO_ROOT")/zeronym-shim-enclave"}
SHA=$(git -C "$ZERO_ROOT" rev-parse HEAD)
SHORT=$(git -C "$ZERO_ROOT" rev-parse --short HEAD)

# Refuse to assemble from a dirty tree. The context comes from HEAD regardless,
# so a dirty tree does not corrupt the build; it corrupts the OPERATOR'S
# understanding of it, by making them think they deployed the edit they are
# looking at. Everything about this deploy is an argument that a specific commit
# is running inside the enclave, so silently deploying a different one is the
# one failure that would matter most.
if [ -n "$(git -C "$ZERO_ROOT" status --porcelain -- zeronym/shim)" ]; then
	echo "error: zeronym/shim has uncommitted changes." >&2
	echo "       This assembles from git archive HEAD, so those changes would" >&2
	echo "       NOT be deployed. Commit them first." >&2
	exit 1
fi

echo "==> assembling Caution deploy repo from zero@$SHORT into $DEST"

# The build context: the shim crate plus the parts of zebra/ and zaino/ its path
# dependencies need. Identical to what the reproducibility check builds, because
# it is the same script.
sh "$ZERO_ROOT/zeronym/shim/deploy/assemble.sh" "$DEST"

# Caution's build.containerfile is resolved from the repo root, so the recipe
# has to exist there. Copy it OUT OF THE ASSEMBLED CONTEXT, never from the
# working tree: the context copy came from `git archive HEAD`, so the root copy
# inherits that provenance. Copying from $HERE/../Containerfile instead would
# reintroduce exactly the hole assemble.sh closes, and would do it silently.
NESTED="$DEST/zeronym/shim/deploy/Containerfile"
test -f "$NESTED" || { echo "error: no Containerfile in the assembled context" >&2; exit 1; }
cp "$NESTED" "$DEST/Containerfile"

# Assert the two copies agree. They must, having just been copied, but this is
# the check that catches a future edit to this script that reaches for the
# working tree because it was nearer to hand.
cmp "$NESTED" "$DEST/Containerfile" || {
	echo "error: root Containerfile differs from the context copy" >&2
	exit 1
}

cp "$HERE/caution.hcl" "$DEST/caution.hcl"

# Record what this was built from, inside the repo that gets pushed. The whole
# deploy argues that a particular commit is running in the enclave; that claim
# should be legible from the deployed artifact itself, not only from a shell
# history somewhere.
EXPECTED=$(cat "$ZERO_ROOT/zeronym/shim/deploy/EXPECTED_SHA256" 2>/dev/null || echo "unrecorded")
cat > "$DEST/PROVENANCE" <<EOF
zero-indexer-shim Caution enclave
source repo:     github.com/ShieldedLabs/zero
source commit:   $SHA
expected binary: $EXPECTED

The binary inside this EIF should hash to the value above. Verify with:
  git clone https://github.com/ShieldedLabs/zero && cd zero
  git checkout $SHA
  sh zeronym/shim/deploy/reproduce.sh
EOF

# A git identity is not configured in a fresh temp repo, and Caution deploys are
# pushes, so the repo has to be able to commit. Use --local so nothing here
# touches the user's global config.
if [ ! -d "$DEST/.git" ]; then
	git -C "$DEST" init --quiet --initial-branch=main
	git -C "$DEST" config --local user.name "zero-deploy"
	git -C "$DEST" config --local user.email "deploy@shieldedlabs.invalid"
fi
git -C "$DEST" add -A
git -C "$DEST" commit --quiet -m "zero-indexer-shim enclave from zero@$SHORT" || true

echo "==> assembled: $DEST ($(du -sh "$DEST" | cut -f1))"
echo
echo "Next, from $DEST:"
echo "  caution login --username <name> --qr     # FIDO2; session expires often"
echo "  caution apps create --name zeronym-shim  # NEW app; do not reuse the z3 node's"
echo "  caution init <app-id>"
echo "  git remote add caution ssh://git@dashboard.caution.co:2222/<app-id>.git"
echo "  git push caution main"

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
# Usage:
#   sh .../assemble-caution.sh --name <enclave> --backend <ip:port> [dest-dir]
#
# One enclave fronts exactly one indexer, so each backend gets its own app and
# its own assembled repo. Both arguments are required rather than defaulted: a
# wrong backend produces an enclave that boots, serves, and quietly proxies for
# something nobody intended, which is worse than one that fails to start.

set -eu

umask 022

NAME=""
BACKEND=""
DEST=""
while [ $# -gt 0 ]; do
	case "$1" in
		--name)    NAME=$2; shift 2 ;;
		--backend) BACKEND=$2; shift 2 ;;
		-*) echo "unknown option: $1" >&2; exit 2 ;;
		*)  DEST=$1; shift ;;
	esac
done

[ -n "$NAME" ] || { echo "error: --name is required (e.g. zeronym-shim-zaino)" >&2; exit 2; }
[ -n "$BACKEND" ] || { echo "error: --backend is required (e.g. 66.42.124.202:8137)" >&2; exit 2; }

# ZIS_BACKEND parses as a Rust SocketAddr, so a hostname does not merely
# degrade, it fails to parse and the enclave never starts. Catch that here,
# where the error is readable, rather than inside an enclave with no console.
BACKEND_IP=${BACKEND%:*}
BACKEND_PORT=${BACKEND##*:}
echo "$BACKEND_IP" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' || {
	echo "error: --backend must be a literal IPv4 address and port, got '$BACKEND'." >&2
	echo "       ZIS_BACKEND is a SocketAddr; a hostname will not parse." >&2
	exit 2
}
echo "$BACKEND_PORT" | grep -qE '^[0-9]+$' || {
	echo "error: --backend port '$BACKEND_PORT' is not numeric" >&2; exit 2; }

ZERO_ROOT=$(git rev-parse --show-toplevel)
HERE="$ZERO_ROOT/zeronym/shim/deploy/caution"
DEST=${DEST:-"$(dirname "$ZERO_ROOT")/$NAME"}
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

# Render the enclave definition. The committed file is a template because the
# only things that vary between the zaino shim and the lightwalletd shim are the
# name and the backend, and hand-editing two near-identical copies is how the
# egress CIDR ends up disagreeing with ZIS_BACKEND: the enclave would then boot,
# fail every dial, and look like a shim bug rather than a firewall one.
sed \
	-e "s|__ENCLAVE_NAME__|$NAME|g" \
	-e "s|__BACKEND_ADDR__|$BACKEND|g" \
	-e "s|__BACKEND_CIDR__|$BACKEND_IP/32|g" \
	-e "s|__BACKEND_PORT__|$BACKEND_PORT|g" \
	"$HERE/caution.hcl.tmpl" > "$DEST/caution.hcl"

# No placeholder may survive. An unsubstituted token would be pushed as literal
# HCL and rejected by Caution's parser at build time, minutes later and with a
# message that does not mention this script.
if grep -q '__[A-Z_]*__' "$DEST/caution.hcl"; then
	echo "error: unsubstituted placeholder left in caution.hcl:" >&2
	grep -n '__[A-Z_]*__' "$DEST/caution.hcl" >&2
	exit 1
fi

# Record what this was built from, inside the repo that gets pushed. The whole
# deploy argues that a particular commit is running in the enclave; that claim
# should be legible from the deployed artifact itself, not only from a shell
# history somewhere.
EXPECTED=$(cat "$ZERO_ROOT/zeronym/shim/deploy/EXPECTED_SHA256" 2>/dev/null || echo "unrecorded")
cat > "$DEST/PROVENANCE" <<EOF
zero-indexer-shim Caution enclave ('$NAME')
source repo:     github.com/ShieldedLabs/zero
backend:         $BACKEND
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
echo "  caution apps create --name '"$NAME"'"
echo "  caution init <app-id>"
echo "  git remote add caution ssh://git@dashboard.caution.co:2222/<app-id>.git"
echo "  git push caution main"

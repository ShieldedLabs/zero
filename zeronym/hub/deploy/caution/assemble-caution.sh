#!/bin/sh
# Assemble the Caution deploy repository for zero-indexer-hub.
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
# dash on Debian and Ubuntu, dash has no `-o pipefail`, and a pipeline without it
# hides the exit status of everything but the last command.
#
# Usage:
#   sh .../assemble-caution.sh --name <enclave> \
#       --indexers <ip:port[,ip:port...]> --indexer-tls <indexer-cert-name> \
#       --tls-domain <hub-domain> [dest-dir]
#
# Unlike the shim (one enclave fronts one indexer), the hub broadcasts through a
# SET of endpoints, so --indexers takes a comma-separated list and one egress /32
# is emitted per endpoint. All four arguments are required: a hub with the wrong
# indexers broadcasts nowhere useful, a hub with no domain has no certificate for
# the shim to verify, and a hub without --indexer-tls sends every batch in the
# clear past its own enclave boundary.

set -eu

umask 022

NAME=""
INDEXERS=""
INDEXER_TLS=""
TLS_DOMAIN=""
TLS_EMAIL="security@shieldedlabs.com"
TLS_PRODUCTION="false"
DEBUG="false"
DEST=""
while [ $# -gt 0 ]; do
	case "$1" in
		--name)          NAME=$2; shift 2 ;;
		--indexers)         INDEXERS=$2; shift 2 ;;
		--indexer-tls)   INDEXER_TLS=$2; shift 2 ;;
		--tls-domain)    TLS_DOMAIN=$2; shift 2 ;;
		--tls-email)     TLS_EMAIL=$2; shift 2 ;;
		--production)    TLS_PRODUCTION="true"; shift ;;
		--debug)         DEBUG="true"; shift ;;
		-*) echo "unknown option: $1" >&2; exit 2 ;;
		*)  DEST=$1; shift ;;
	esac
done

[ -n "$NAME" ] || { echo "error: --name is required (e.g. zeronym-hub-1)" >&2; exit 2; }
[ -n "$INDEXERS" ] || { echo "error: --indexers is required (e.g. 1.2.3.4:8232,5.6.7.8:8232)" >&2; exit 2; }
[ -n "$TLS_DOMAIN" ] || { echo "error: --tls-domain is required (the name shims connect to)" >&2; exit 2; }

# Without TLS the enclave's parent host reads every batch in the clear moments
# before it is public, which removes most of the reason to run the hub in an
# enclave at all. Required rather than warned about.
[ -n "$INDEXER_TLS" ] || {
	echo "error: --indexer-tls is required (the DNS name the indexer's cert carries)." >&2
	echo "       Without it the hop is plaintext and the parent host reads every batch." >&2
	exit 2
}

# Production is opt-in and announced, because it spends one of five weekly
# duplicate-certificate issuances for this name and there is no way to get it
# back. Staging has no meaningful ceiling and is where a change should first
# prove itself.
if [ "$TLS_PRODUCTION" = "true" ]; then
	echo "==> Let's Encrypt PRODUCTION for $TLS_DOMAIN."
	echo "    This spends one of 5 weekly issuances. Record it in RESTARTS.md."
else
	echo "==> Let's Encrypt STAGING for $TLS_DOMAIN (certificates will not be trusted by clients)."
fi

ZERO_ROOT=$(git rev-parse --show-toplevel)
HERE="$ZERO_ROOT/zeronym/hub/deploy/caution"
DEST=${DEST:-"$(dirname "$ZERO_ROOT")/$NAME"}
SHA=$(git -C "$ZERO_ROOT" rev-parse HEAD)
SHORT=$(git -C "$ZERO_ROOT" rev-parse --short HEAD)

STAGE=$(mktemp -d)
# shellcheck disable=SC2064
trap "rm -rf '$STAGE'" EXIT INT TERM

# Validate every endpoint and build its egress block. ZIH_INDEXERS entries must be
# literal IPv4:port: the enclave has no DNS egress (no port 53), so a hostname
# would dial nothing, and the /32 egress rule needs a literal address anyway.
# Each block is emitted with the four-space indent of the network stanza so the
# rendered HCL is clean.
EGRESS="$STAGE/egress.txt"
: > "$EGRESS"
OLDIFS=$IFS
IFS=,
first=1
for endpoint in $INDEXERS; do
	IFS=$OLDIFS
	NODE_IP=${endpoint%:*}
	NODE_PORT=${endpoint##*:}
	echo "$NODE_IP" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' || {
		echo "error: --indexers entry '$endpoint' is not a literal IPv4 address and port." >&2
		echo "       The enclave dials IPs with no DNS; a hostname will not resolve." >&2
		exit 2
	}
	echo "$NODE_PORT" | grep -qE '^[0-9]+$' || {
		echo "error: --indexers entry '$endpoint' has a non-numeric port" >&2; exit 2; }
	[ "$first" = 1 ] || echo "" >> "$EGRESS"
	first=0
	cat >> "$EGRESS" <<EOF
    egress {
      cidr_ipv4   = "$NODE_IP/32"
      port        = $NODE_PORT
      ip_protocol = "tcp"
    }
EOF
	IFS=,
done
IFS=$OLDIFS

# The endpoint list, normalised (no trailing/leading spaces), for the ZIH_INDEXERS env.
INDEXERS_ENV=$INDEXERS

# Refuse to assemble from a dirty tree. The context comes from HEAD regardless,
# so a dirty tree does not corrupt the build; it corrupts the OPERATOR'S
# understanding of it, by making them think they deployed the edit they are
# looking at.
if [ -n "$(git -C "$ZERO_ROOT" status --porcelain -- zeronym/hub)" ]; then
	echo "error: zeronym/hub has uncommitted changes." >&2
	echo "       This assembles from git archive HEAD, so those changes would" >&2
	echo "       NOT be deployed. Commit them first." >&2
	exit 1
fi

echo "==> assembling Caution deploy repo from zero@$SHORT into $DEST"

# The build context: the hub crate plus the parts of zebra/ its path dependency
# needs. Identical to what the reproducibility check builds, because it is the
# same script.
sh "$ZERO_ROOT/zeronym/hub/deploy/assemble.sh" "$DEST"

# Caution's build.containerfile is resolved from the repo root, so the recipe has
# to exist there. Copy it OUT OF THE ASSEMBLED CONTEXT, never from the working
# tree: the context copy came from `git archive HEAD`, so the root copy inherits
# that provenance.
NESTED="$DEST/zeronym/hub/deploy/Containerfile"
test -f "$NESTED" || { echo "error: no Containerfile in the assembled context" >&2; exit 1; }
cp "$NESTED" "$DEST/Containerfile"
cmp "$NESTED" "$DEST/Containerfile" || {
	echo "error: root Containerfile differs from the context copy" >&2
	exit 1
}

# Render the enclave definition. Two markers carry multi-line content (the
# per-node egress blocks and the optional node-auth env), injected with awk from
# the files built above so no metacharacter has to survive sed. The scalar
# fields go through sed afterwards.
RENDERED="$STAGE/caution.hcl"
awk -v egress="$EGRESS" '
	/__EGRESS_BLOCKS__/ { while ((getline l < egress) > 0) print l; next }
	{ print }
' "$HERE/caution.hcl.tmpl" > "$RENDERED"

sed \
	-e "s|__ENCLAVE_NAME__|$NAME|g" \
	-e "s|__INDEXERS__|$INDEXERS_ENV|g" \
	-e "s|__INDEXER_TLS__|$INDEXER_TLS|g" \
	-e "s|__TLS_DOMAIN__|$TLS_DOMAIN|g" \
	-e "s|__TLS_EMAIL__|$TLS_EMAIL|g" \
	-e "s|__TLS_PRODUCTION__|$TLS_PRODUCTION|g" \
	"$RENDERED" > "$DEST/caution.hcl"

# --debug: flip the enclave into debug mode. DIAGNOSTIC only: debug mode disables
# attestation, so nothing it runs is provable, and this is the enclave trusted
# with plaintext migrations. Use it on a throwaway host to read the enclave
# console (SSH opens on the parent in debug mode), never for real traffic. The
# hub BINARY is identical to the attested build, so a failure reproduced here is
# the same failure.
if [ "$DEBUG" = "true" ]; then
	sed -i.bak -e 's|^    enabled  = false|    enabled  = true|' "$DEST/caution.hcl"
	rm -f "$DEST/caution.hcl.bak"
	echo "==> DEBUG build: attestation OFF, SSH console ON. Diagnostic only."
fi

# No placeholder may survive. An unsubstituted token would be pushed as literal
# HCL and rejected by Caution's parser at build time, minutes later and with a
# message that does not mention this script.
if grep -q '__[A-Z_]*__' "$DEST/caution.hcl"; then
	echo "error: unsubstituted placeholder left in caution.hcl:" >&2
	grep -n '__[A-Z_]*__' "$DEST/caution.hcl" >&2
	exit 1
fi

# Record what this was built from, inside the repo that gets pushed.
EXPECTED=$(cat "$ZERO_ROOT/zeronym/hub/deploy/EXPECTED_SHA256" 2>/dev/null || echo "unrecorded")
cat > "$DEST/PROVENANCE" <<EOF
zero-indexer-hub Caution enclave ('$NAME')
source repo:     github.com/ShieldedLabs/zero
serves:          $TLS_DOMAIN (TLS terminated in-enclave, ACME)
broadcasts via:  $INDEXERS_ENV verified as $INDEXER_TLS
acme directory:  $([ "$TLS_PRODUCTION" = "true" ] && echo "letsencrypt PRODUCTION" || echo "letsencrypt staging")
source commit:   $SHA
expected binary: $EXPECTED

The binary inside this EIF should hash to the value above. Verify with:
  git clone https://github.com/ShieldedLabs/zero && cd zero
  git checkout $SHA
  sh zeronym/hub/deploy/reproduce.sh
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
git -C "$DEST" commit --quiet -m "zero-indexer-hub enclave from zero@$SHORT" || true

echo "==> assembled: $DEST ($(du -sh "$DEST" | cut -f1))"
echo
echo "Next, from $DEST:"
echo "  caution login --username <name> --qr     # FIDO2; session expires often"
echo "  caution apps create --name '"$NAME"'"
echo "  caution init <app-id>"
echo "  git remote add caution ssh://git@dashboard.caution.co:2222/<app-id>.git"
echo "  git push caution main"

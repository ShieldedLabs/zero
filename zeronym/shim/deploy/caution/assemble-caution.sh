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
#   sh .../assemble-caution.sh --name <enclave> --backend <ip:port> \
#       --backend-tls <cert-name> --tls-domain <wallet-facing-domain> \
#       [--hub <ip:port> --hub-tls <hub-cert-name>] [dest-dir]
#
# --hub turns diversion ON. Without it the shim is forward-only: it classifies
# and logs, and still hands every migration to the operator's indexer.
#
# One enclave fronts exactly one indexer, so each backend gets its own app and
# its own assembled repo. Both arguments are required rather than defaulted: a
# wrong backend produces an enclave that boots, serves, and quietly proxies for
# something nobody intended, which is worse than one that fails to start.

set -eu

umask 022

NAME=""
BACKEND=""
BACKEND_TLS=""
HUB=""
HUB_TLS=""
TLS_DOMAIN=""
TLS_EMAIL="security@shieldedlabs.com"
TLS_PRODUCTION="false"
DEBUG="false"
DEST=""
while [ $# -gt 0 ]; do
	case "$1" in
		--name)        NAME=$2; shift 2 ;;
		--backend)     BACKEND=$2; shift 2 ;;
		--backend-tls) BACKEND_TLS=$2; shift 2 ;;
		--hub)         HUB=$2; shift 2 ;;
		--hub-tls)     HUB_TLS=$2; shift 2 ;;
		--tls-domain)  TLS_DOMAIN=$2; shift 2 ;;
		--tls-email)   TLS_EMAIL=$2; shift 2 ;;
		--production)  TLS_PRODUCTION="true"; shift ;;
		--debug)       DEBUG="true"; shift ;;
		-*) echo "unknown option: $1" >&2; exit 2 ;;
		*)  DEST=$1; shift ;;
	esac
done

[ -n "$NAME" ] || { echo "error: --name is required (e.g. zeronym-shim-zaino)" >&2; exit 2; }
[ -n "$BACKEND" ] || { echo "error: --backend is required (e.g. 66.42.124.202:443)" >&2; exit 2; }
[ -n "$BACKEND_TLS" ] || { echo "error: --backend-tls is required (the DNS name the backend's cert carries)" >&2; exit 2; }
[ -n "$TLS_DOMAIN" ] || { echo "error: --tls-domain is required (the name wallets connect to)" >&2; exit 2; }

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

# --hub turns DIVERSION ON. Without it the shim is forward-only: it classifies
# and logs but hands every migration to the operator's indexer exactly as the
# proof of concept did, which is no privacy at all. That is the default on
# purpose, so an operator who deploys this without having been given a hub
# address gets working, honest, unchanged behaviour rather than a shim that
# fails every migration.
STAGE=$(mktemp -d)
# shellcheck disable=SC2064
trap "rm -rf '$STAGE'" EXIT INT TERM
HUB_EGRESS="$STAGE/hub_egress.txt"
HUB_ENV="$STAGE/hub_env.txt"
: > "$HUB_EGRESS"
: > "$HUB_ENV"

if [ -n "$HUB" ]; then
	# ZIS_HUB parses as a Rust SocketAddr and the enclave resolves no DNS (there
	# is no port 53 egress), so a hostname does not degrade, it fails to parse
	# and the enclave never starts. Catch it here where the error is readable.
	HUB_IP=${HUB%:*}
	HUB_PORT=${HUB##*:}
	echo "$HUB_IP" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' || {
		echo "error: --hub must be a literal IPv4 address and port, got '$HUB'." >&2
		echo "       ZIS_HUB is a SocketAddr; a hostname will not parse." >&2
		exit 2
	}
	echo "$HUB_PORT" | grep -qE '^[0-9]+$' || {
		echo "error: --hub port '$HUB_PORT' is not numeric" >&2; exit 2; }

	# A SECOND /32, and no wider. The shim now holds migrations in the clear on
	# their way out, so the set of places this enclave can reach is the set of
	# places a migration could go. Two destinations, two ports, nothing else.
	cat > "$HUB_EGRESS" <<EOF

    # The hub. Migrations go here instead of to the operator's indexer, so this
    # is the one additional destination the enclave may reach. Same reasoning as
    # the backend rule above: a literal /32 and a single port, no DNS.
    egress {
      cidr_ipv4   = "$HUB_IP/32"
      port        = $HUB_PORT
      ip_protocol = "tcp"
    }
EOF

	if [ -n "$HUB_TLS" ]; then
		cat > "$HUB_ENV" <<EOF

      # Divert Orchard-touching transactions to the hub at this literal address,
      # authenticated as the name below. With ZIS_HUB set the shim stops handing
      # migrations to the operator's indexer at all.
      ZIS_HUB     = "$HUB"
      ZIS_HUB_TLS = "$HUB_TLS"
EOF
	else
		# Allowed, and warned about loudly. The hop carries a migration in the
		# clear, and the whole point of the hub being attested is undone if
		# anything between the two enclaves can read or alter what crosses.
		cat > "$HUB_ENV" <<EOF

      # Divert Orchard-touching transactions to the hub at this literal address.
      # NO ZIS_HUB_TLS: this hop is PLAINTEXT. Only correct on a trusted network
      # path; set --hub-tls for any real deployment.
      ZIS_HUB = "$HUB"
EOF
		echo "==> WARNING: --hub without --hub-tls. The shim-to-hub hop will be PLAINTEXT."
		echo "    A migration crosses it in the clear. Use --hub-tls for a real deployment."
	fi
	echo "==> DIVERSION ON: Orchard-touching transactions go to $HUB, not to the operator's indexer."
else
	echo "==> forward-only: no --hub, so migrations are forwarded to the operator's indexer (no privacy)."
fi

if [ -n "$HUB_TLS" ] && [ -z "$HUB" ]; then
	echo "error: --hub-tls without --hub. Nothing would be diverted." >&2
	exit 2
fi

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
# The two hub markers carry multi-line content and are injected with awk from
# the files built above, so nothing has to survive sed quoting. Both are empty
# in the forward-only case, and an empty file removes the marker line entirely.
RENDERED="$STAGE/caution.hcl"
awk -v egress="$HUB_EGRESS" -v env="$HUB_ENV" '
	/__HUB_EGRESS__/ { while ((getline l < egress) > 0) print l; next }
	/__HUB_ENV__/    { while ((getline l < env) > 0) print l; next }
	{ print }
' "$HERE/caution.hcl.tmpl" > "$RENDERED"

sed \
	-e "s|__ENCLAVE_NAME__|$NAME|g" \
	-e "s|__BACKEND_ADDR__|$BACKEND|g" \
	-e "s|__BACKEND_CIDR__|$BACKEND_IP/32|g" \
	-e "s|__BACKEND_PORT__|$BACKEND_PORT|g" \
	-e "s|__BACKEND_TLS_NAME__|$BACKEND_TLS|g" \
	-e "s|__TLS_DOMAIN__|$TLS_DOMAIN|g" \
	-e "s|__TLS_EMAIL__|$TLS_EMAIL|g" \
	-e "s|__TLS_PRODUCTION__|$TLS_PRODUCTION|g" \
	"$RENDERED" > "$DEST/caution.hcl"

# --debug: flip the enclave into debug mode and turn on per-request shim logging.
# This is a DIAGNOSTIC build, not a shippable one, for two reasons stated in the
# template: debug mode disables attestation (so nothing it runs is provable), and
# RUST_LOG=zis::proxy=debug logs the gRPC method each caller invokes, which is the
# exact metadata the shim exists to deny an operator. Use it on a throwaway host
# to read the enclave console (SSH opens on the parent in debug mode), never for
# real traffic. The shim BINARY is identical to the attested build, so a failure
# reproduced here is the same failure.
if [ "$DEBUG" = "true" ]; then
	sed -i.bak \
		-e 's|^      # RUST_LOG = "zis::proxy=debug,info"|      RUST_LOG = "zis::proxy=debug,info"|' \
		-e 's|^    enabled  = false|    enabled  = true|' \
		"$DEST/caution.hcl"
	rm -f "$DEST/caution.hcl.bak"
	echo "==> DEBUG build: attestation OFF, SSH console ON, per-request logging ON. Diagnostic only."
fi

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
serves:          $TLS_DOMAIN (TLS terminated in-enclave, ACME)
backend:         $BACKEND verified as $BACKEND_TLS
diversion:       $([ -n "$HUB" ] && echo "ON -> hub $HUB${HUB_TLS:+ verified as $HUB_TLS}" || echo "OFF (forward-only, no privacy)")
acme directory:  $([ "$TLS_PRODUCTION" = "true" ] && echo "letsencrypt PRODUCTION" || echo "letsencrypt staging")
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

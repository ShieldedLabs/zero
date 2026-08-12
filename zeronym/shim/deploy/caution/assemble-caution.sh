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
#       [--app-source <public-git-url>] \
#       ( --hub <ip:port> --hub-tls <hub-cert-name>              # clearnet hop
#       | --hub-nym <addr1,addr2> --nym-egress <cidr:port[:proto]> ... # mixnet
#           [--nym-rotation-secs <n>] )                          \
#       [dest-dir]
#
# --hub (clearnet) or --hub-nym (mixnet) turns diversion ON; they are mutually
# exclusive. Without either, the shim is forward-only: it classifies and logs,
# and still hands every migration to the operator's indexer.
#
# --hub-nym diverts over the Nym mixnet. It needs one or more --nym-egress
# allowlist rules (the gateway(s) and nym-api set, from the host operator), and
# the enclave reaches those, NOT the hub directly. See the long note in the Nym
# transport block for the DNS / breadth / ticketbook decisions this requires.
#
# --app-source records, in the manifest's build block, the public git URL where
# this assembled repository is published. `caution verify` clones that URL and
# rebuilds; without it verify refuses outright and the attestation proves only
# that SOME image runs in a real enclave.
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
HUB_NYM=""
NYM_EGRESS=""
NYM_ROTATION=""
TLS_DOMAIN=""
APP_SOURCE=""
DEBUG="false"
DEST=""
while [ $# -gt 0 ]; do
	case "$1" in
		--name)             NAME=$2; shift 2 ;;
		--backend)          BACKEND=$2; shift 2 ;;
		--backend-tls)      BACKEND_TLS=$2; shift 2 ;;
		--hub)              HUB=$2; shift 2 ;;
		--hub-tls)          HUB_TLS=$2; shift 2 ;;
		# Divert over the Nym mixnet instead of the clearnet --hub hop. A
		# comma-separated list of hub Nym addresses (identity.encryption@gateway),
		# mutually exclusive with --hub. See the transport block below.
		--hub-nym)          HUB_NYM=$2; shift 2 ;;
		# One enclave egress allowlist entry for the mixnet, repeatable:
		# `cidr:port[:proto]`, e.g. --nym-egress 1.2.3.4/32:9001:tcp. The host
		# operator supplies these (gateway(s), nym-api set, optionally DNS/Nyx);
		# see the long note in the Nym transport block for what they cover and why
		# a plain /32 is not enough this time.
		--nym-egress)       NYM_EGRESS="$NYM_EGRESS $2"; shift 2 ;;
		# Rotate the shim's mixnet identity every N seconds (D11: the sender-tag
		# linkage window). Unset never rotates. A deployment decision.
		--nym-rotation-secs) NYM_ROTATION=$2; shift 2 ;;
		--tls-domain)       TLS_DOMAIN=$2; shift 2 ;;
		--app-source)       APP_SOURCE=$2; shift 2 ;;
		--debug)            DEBUG="true"; shift ;;
		-*) echo "unknown option: $1" >&2; exit 2 ;;
		*)  DEST=$1; shift ;;
	esac
done

[ -n "$NAME" ] || { echo "error: --name is required (e.g. zeronym-shim-zaino)" >&2; exit 2; }
[ -n "$BACKEND" ] || { echo "error: --backend is required (e.g. 66.42.124.202:443)" >&2; exit 2; }
[ -n "$BACKEND_TLS" ] || { echo "error: --backend-tls is required (the DNS name the backend's cert carries)" >&2; exit 2; }
[ -n "$TLS_DOMAIN" ] || { echo "error: --tls-domain is required (the name wallets connect to)" >&2; exit 2; }

# There is no staging knob on this path: the in-enclave Caddy picks the ACME
# directory itself and always uses production. Every push therefore spends one
# of this hostname's five weekly duplicate-certificate issuances, and running
# out fails closed (TCP accepts, TLS never completes) with no console to say
# why. Iterate on throwaway hostnames; see RESTARTS.md.
echo "==> Let's Encrypt PRODUCTION for $TLS_DOMAIN: every push spends one of this"
echo "    name's 5 weekly issuances. Iterate on throwaway names; see RESTARTS.md."

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
KEEP="$STAGE/keep"
# On ANY exit, put a preserved deployment link (see the block above the
# assemble.sh call) back in $DEST before the temp dir is swept. Losing
# .caution/ orphans a live app: push has no remote, verify has no endpoint,
# and teardown cannot find the resource to destroy, so on BYOC the AWS stack
# sits there billing with nothing left that knows about it. If the restore
# itself fails, keep $STAGE rather than delete the only copy.
cleanup() {
	if [ -d "$KEEP/.caution" ] || [ -d "$KEEP/.git" ]; then
		mkdir -p "$DEST" || true
	fi
	if [ -d "$KEEP/.caution" ]; then
		mv "$KEEP/.caution" "$DEST/.caution" || true
	fi
	if [ -d "$KEEP/.git" ]; then
		mv "$KEEP/.git" "$DEST/.git" || true
	fi
	if [ -d "$KEEP/.caution" ] || [ -d "$KEEP/.git" ]; then
		echo "warning: could not restore .caution/.git; recover them from $KEEP" >&2
	else
		rm -rf "$STAGE"
	fi
}
trap cleanup EXIT INT TERM
HUB_EGRESS="$STAGE/hub_egress.txt"
HUB_ENV="$STAGE/hub_env.txt"
: > "$HUB_EGRESS"
: > "$HUB_ENV"

# Transports are mutually exclusive: --hub is the clearnet hop, --hub-nym the
# mixnet. Which one carries a divert decides whether the operator can observe it,
# and whether this enclave needs mixnet egress at all, so it is never inferred.
if [ -n "$HUB_NYM" ] && [ -n "$HUB" ]; then
	echo "error: --hub and --hub-nym are mutually exclusive. Pick one transport." >&2
	exit 2
fi
if [ -n "$NYM_EGRESS" ] && [ -z "$HUB_NYM" ]; then
	echo "error: --nym-egress given without --hub-nym. Nothing would use it." >&2
	exit 2
fi

if [ -n "$HUB_NYM" ]; then
	# DIVERT OVER THE NYM MIXNET. Fundamentally different from the clearnet --hub
	# hop, and the egress reflects it. The shim does NOT reach the hub directly; it
	# hands Sphinx packets to a Nym ENTRY GATEWAY, which routes them through the
	# mixnet to the hub. So the destinations this enclave must reach are the
	# gateway(s) and the nym-api set (topology refresh), NOT the hub's IP. And
	# unlike the single /32 of the clearnet rule, this set is plural and it CHURNS:
	# the active gateway set reshuffles and keys rotate, so the allowlist is not a
	# static pin.
	#
	# THREE THINGS THE HOST OPERATOR MUST DECIDE, because they are not ours to
	# choose and this script does not invent them:
	#   1. DNS. The default Nym network reaches its nym-apis and gateways by NAME,
	#      and this enclave resolves nothing (no port 53 today). Either add a
	#      `--nym-egress <resolver>/32:53:udp` rule and accept DNS, or pin every
	#      endpoint by IP and keep the no-DNS posture. The no-DNS path ALSO needs
	#      shim-side support the driver does not yet expose (IP-literal --nym-apis,
	#      no_hostname, a custom topology), so today the deployable configuration is
	#      DNS-permitted. Tracked in NYM_PLAN.md M6.
	#   2. Breadth. One /32 per gateway and per nym-api is tightest but must be
	#      updated as the set churns; a broader mixnet CIDR is looser but stable.
	#   3. Ticketbooks. A public-network run needs bandwidth credentials, and if
	#      they are acquired on-chain in-enclave, a Nyx RPC egress rule as well.
	#
	# The operator passes each allowlist entry as --nym-egress cidr:port[:proto],
	# and every one becomes an egress block. At least one is required: a mixnet
	# enclave with no egress reaches no gateway and diverts nothing.
	[ -n "$NYM_EGRESS" ] || {
		echo "error: --hub-nym needs at least one --nym-egress rule (the gateway(s)" >&2
		echo "       and nym-api set the host operator allowlists). None given." >&2
		exit 2
	}
	# Shallow structural check on each address (identity.encryption@gateway), the
	# same shape the shim's own config enforces; the SDK does the real parse.
	OLDIFS=$IFS; IFS=','
	for addr in $HUB_NYM; do
		case "$addr" in
			?*.?*@?*) : ;;
			*) echo "error: --hub-nym entry '$addr' is not identity.encryption@gateway" >&2; exit 2 ;;
		esac
	done
	IFS=$OLDIFS

	# One egress block per --nym-egress rule (cidr:port[:proto], proto default tcp).
	for rule in $NYM_EGRESS; do
		cidr=${rule%%:*}; rest=${rule#*:}
		port=${rest%%:*}; proto=${rest#*:}
		[ "$proto" = "$rest" ] && proto=tcp
		echo "$port" | grep -qE '^[0-9]+$' || {
			echo "error: --nym-egress rule '$rule' has a non-numeric port" >&2; exit 2; }
		printf '\n    # Nym mixnet egress (gateway / nym-api / DNS / Nyx), operator-allowlisted.\n' >> "$HUB_EGRESS"
		printf '    egress {\n      cidr_ipv4   = "%s"\n      port        = %s\n      ip_protocol = "%s"\n    }\n' \
			"$cidr" "$port" "$proto" >> "$HUB_EGRESS"
	done

	# ZIS_HUB_NYM is the address list; the driver picks a live one and fails over
	# (D10). No ZIS_HUB_TLS: the mixnet IS the confidentiality boundary, so there
	# is no TLS name to verify on this hop. Rotation is the D11 linkage-window knob.
	{
		printf '\n      # Divert Orchard-touching transactions over the Nym mixnet to these hub\n'
		printf '      # addresses. The mixnet is the confidentiality boundary; there is no TLS\n'
		printf '      # name to verify on this hop. The driver tries each address until one acks.\n'
		printf '      ZIS_HUB_NYM = "%s"\n' "$HUB_NYM"
		[ -n "$NYM_ROTATION" ] && printf '      ZIS_NYM_ROTATION_SECS = "%s"\n' "$NYM_ROTATION"
	} >> "$HUB_ENV"
	echo "==> DIVERSION ON over the Nym mixnet to: $HUB_NYM"
	echo "    egress allowlist:$NYM_EGRESS"
	[ -n "$NYM_ROTATION" ] && echo "    identity rotation: every ${NYM_ROTATION}s" || echo "    identity rotation: never (--nym-rotation-secs unset; linkage window = process uptime)"
elif [ -n "$HUB" ]; then
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

# The manifest can record where this assembled repository is published, and
# verification hangs on it: Caution's own git remote is push-only, so the
# published repo is the ONLY route an auditor has to the deployed tree.
# Injected as a marker (like the hub blocks) because a git URL may contain
# characters sed treats as metacharacters in the replacement text.
APP_SRC_FILE="$STAGE/app_source.txt"
: > "$APP_SRC_FILE"
if [ -n "$APP_SOURCE" ]; then
	cat > "$APP_SRC_FILE" <<EOF

    # Where this assembled repository is published. 'caution verify' clones
    # this URL and rebuilds, so its root must be THIS directory, not the zero
    # monorepo, and the deployed commit must be pushed there on main and
    # tagged: the manifest pins branch AND commit.
    app_sources = ["$APP_SOURCE"]
EOF
else
	echo "==> WARNING: no --app-source. The manifest will record no application source,"
	echo "    so 'caution verify' refuses (\"Cannot reproduce private code deployment\")"
	echo "    and the attestation proves only that SOME image runs in a real enclave."
	echo "    Create a public repo for this assembled directory and pass its URL."
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

# assemble.sh starts with `rm -rf "$DEST"`: the clean slate is what makes the
# reproducibility argument work, and reproduce.sh depends on it staying that
# way, so the preservation lives HERE, not there. After `caution apps create`
# (or `caution init --byoc`) this directory also holds what binds it to the
# deployed app: .caution/ (deployment.json carries the resource_id) and .git
# (the 'caution' remote, and the history the platform already has). Wiping
# those orphans the app: no remote to push to, nothing for verify to infer,
# nothing for teardown to destroy. Step them aside for the duration; the
# cleanup trap restores them even if assemble.sh fails. Preserving .git also
# keeps every re-assembly on the same history, so a redeploy is a fast-forward
# `git push caution main` instead of the destroy/create/repoint-DNS cycle.
# (Reported by the zec.rocks operators, who lost a live deployment link to
# exactly this and recovered it from a backup they happened to have.)
if [ -d "$DEST/.caution" ] || [ -d "$DEST/.git" ]; then
	echo "==> preserving .caution/ and .git across re-assembly"
	mkdir -p "$KEEP"
	if [ -d "$DEST/.caution" ]; then mv "$DEST/.caution" "$KEEP/.caution"; fi
	if [ -d "$DEST/.git" ]; then mv "$DEST/.git" "$KEEP/.git"; fi
fi

# The build context: the shim crate plus the parts of zebra/ and zaino/ its path
# dependencies need. Identical to what the reproducibility check builds, because
# it is the same script.
sh "$ZERO_ROOT/zeronym/shim/deploy/assemble.sh" "$DEST"

# Put them straight back, so the git steps at the bottom of this script see the
# preserved history and commit on top of it (files the new context no longer
# carries become staged deletions via `add -A`).
if [ -d "$KEEP/.caution" ]; then mv "$KEEP/.caution" "$DEST/.caution"; fi
if [ -d "$KEEP/.git" ]; then mv "$KEEP/.git" "$DEST/.git"; fi

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
awk -v egress="$HUB_EGRESS" -v env="$HUB_ENV" -v appsrc="$APP_SRC_FILE" '
	/__HUB_EGRESS__/ { while ((getline l < egress) > 0) print l; next }
	/__HUB_ENV__/    { while ((getline l < env) > 0) print l; next }
	/__APP_SOURCE__/ { while ((getline l < appsrc) > 0) print l; next }
	{ print }
' "$HERE/caution.hcl.tmpl" > "$RENDERED"

sed \
	-e "s|__ENCLAVE_NAME__|$NAME|g" \
	-e "s|__BACKEND_ADDR__|$BACKEND|g" \
	-e "s|__BACKEND_CIDR__|$BACKEND_IP/32|g" \
	-e "s|__BACKEND_PORT__|$BACKEND_PORT|g" \
	-e "s|__BACKEND_TLS_NAME__|$BACKEND_TLS|g" \
	-e "s|__TLS_DOMAIN__|$TLS_DOMAIN|g" \
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
serves:          $TLS_DOMAIN (TLS terminated in-enclave, ACME production)
backend:         $BACKEND verified as $BACKEND_TLS
diversion:       $(if [ -n "$HUB_NYM" ]; then echo "ON -> Nym mixnet, hub(s): $HUB_NYM"; elif [ -n "$HUB" ]; then echo "ON -> hub $HUB${HUB_TLS:+ verified as $HUB_TLS}"; else echo "OFF (forward-only, no privacy)"; fi)
app source:      $([ -n "$APP_SOURCE" ] && echo "$APP_SOURCE" || echo "none (not independently verifiable)")
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
echo "  caution apps create    # fully-managed (no --name; auto-names, adds the 'caution' remote)"
echo "    or, in your own AWS account: AWS_PROFILE=<profile> caution init --byoc --region <region>"
echo "  git push caution main  # builds and boots the enclave; prints its IP"
echo ""
echo "Then publish this repo at the --app-source URL: push main and tag the commit"
echo "(the manifest pins branch AND commit), then 'caution verify' from this directory."
echo ""
echo "To REDEPLOY after re-assembling: git push caution main"
echo "  (.caution/ and .git are preserved across re-assembly, so the push fast-forwards)."
echo "If the push is refused (unrelated history, or the app is in a failed state):"
echo "  echo y | caution apps destroy <app-id>"
echo "  caution apps create && git push caution main   # new app id AND new IP: repoint DNS"

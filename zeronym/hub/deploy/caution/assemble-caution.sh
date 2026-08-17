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
#       --tls-domain <hub-domain> \
#       [--app-source <public-git-url>] \
#       [--nym --nym-egress <cidr:port[:proto]> ...] \
#       [--debug --ssh-key "<ssh pubkey>" ...] \
#       [dest-dir]
#
# --app-source records, in the manifest's build block, the public git URL where
# this assembled repository is published. `caution verify` clones that URL and
# rebuilds; without it verify refuses outright and the attestation proves only
# that SOME image runs in a real enclave. The hub is the component that holds
# migrations in plaintext, so it is the one that most needs to be checkable.
#
# --debug opens the enclave console over SSH (attestation OFF; diagnostic only). It
# REQUIRES --ssh-key: the authorized console key is an input, not a value baked into
# the repo, so whoever deploys is whoever can read the console. --ssh-key is
# repeatable and takes a full public-key line, e.g.
#   --ssh-key "$(cat ~/.ssh/id_ed25519.pub)"
#
# --nym additionally runs the hub's own Nym mixnet client so shims can submit over
# the mixnet (it logs its Nym address at startup; publish that to shims as
# --hub-nym). It needs one or more --nym-egress rules for the gateway/nym-api set
# (from the host operator). The nym-api endpoint is NOT configurable here, so
# those rules have to agree with what nym-sdk's own built-in endpoint list
# resolves to on the day; the checks in the --nym block say what breaks when they
# stop agreeing. The HTTP submit path stays either way; a fully mixnet-only hub
# (no inbound port) is the M7 tightening.
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
APP_SOURCE=""
NYM="false"
NYM_EGRESS=""
NYM_GATEWAY=""
DEBUG="false"
SSH_KEYS=""
DEST=""
while [ $# -gt 0 ]; do
	case "$1" in
		--name)          NAME=$2; shift 2 ;;
		--indexers)      INDEXERS=$2; shift 2 ;;
		--indexer-tls)   INDEXER_TLS=$2; shift 2 ;;
		--tls-domain)    TLS_DOMAIN=$2; shift 2 ;;
		# The public git URL where this assembled repository is published.
		# `caution verify` clones it and rebuilds; without it verify refuses
		# outright and the attestation proves only that SOME image runs in a real
		# enclave. That matters more here than on the shim: this is the enclave
		# trusted with plaintext migrations, so an unverifiable hub reduces the
		# whole privacy claim to a promise.
		--app-source)    APP_SOURCE=$2; shift 2 ;;
		# Also receive submissions over the Nym mixnet: the hub runs its OWN mixnet
		# client (ZIH_NYM), so the enclave needs egress to the gateway/nym-api set.
		--nym)           NYM="true"; shift ;;
		# One mixnet egress allowlist entry, repeatable: cidr:port[:proto]. The host
		# operator supplies these (gateway(s), nym-api set, optionally DNS/Nyx).
		--nym-egress)    NYM_EGRESS="$NYM_EGRESS $2"; shift 2 ;;
		# Pin the hub's ENTRY gateway by identity key. SINGLE value (unlike the
		# shim's list): the hub's address embeds its gateway and must stay stable,
		# so it does not rotate. Needs a matching --nym-egress <gateway-ip>/32 rule
		# (request_gateway takes the IDENTITY, egress takes the IP). Unset = SDK picks.
		--nym-gateway)   NYM_GATEWAY=$2; shift 2 ;;
		--debug)         DEBUG="true"; shift ;;
		# One authorized debug-console SSH public key, repeatable. Required with
		# --debug (SSH opens then); recorded-but-unused otherwise. A key line carries
		# spaces (type, base64, comment), so accumulate with a newline separator.
		--ssh-key)       SSH_KEYS="${SSH_KEYS}${2}
"; shift 2 ;;
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

# Debug mode opens SSH on the parent host; without a key you hold, the console you
# are turning on is one only someone else can read. Require the key as an explicit
# input so the operator deploying is the operator who can read it.
if [ "$DEBUG" = "true" ] && [ -z "$SSH_KEYS" ]; then
	echo "error: --debug opens the enclave console over SSH, but no --ssh-key was given." >&2
	echo "       Pass your own key so YOU can read it, e.g.:" >&2
	echo "         --ssh-key \"\$(cat ~/.ssh/id_ed25519.pub)\"" >&2
	exit 2
fi
if [ -n "$SSH_KEYS" ] && [ "$DEBUG" != "true" ]; then
	echo "==> NOTE: --ssh-key given without --debug. SSH is closed when attestation is"
	echo "    on, so the key is recorded in the HCL but unused until a --debug build."
fi

# There is no staging knob on this path: the in-enclave Caddy picks the ACME
# directory itself and always uses production. Every push therefore spends one
# of this hostname's five weekly duplicate-certificate issuances, and running
# out fails closed (TCP accepts, TLS never completes) with no console to say
# why. Iterate on throwaway hostnames; see RESTARTS.md.
echo "==> Let's Encrypt PRODUCTION for $TLS_DOMAIN: every push spends one of this"
echo "    name's 5 weekly issuances. Iterate on throwaway names; see RESTARTS.md."

ZERO_ROOT=$(git rev-parse --show-toplevel)
HERE="$ZERO_ROOT/zeronym/hub/deploy/caution"
DEST=${DEST:-"$(dirname "$ZERO_ROOT")/$NAME"}
SHA=$(git -C "$ZERO_ROOT" rev-parse HEAD)
SHORT=$(git -C "$ZERO_ROOT" rev-parse --short HEAD)

STAGE=$(mktemp -d)
KEEP="$STAGE/keep"
# On ANY exit, put a preserved deployment link (see the block above the
# assemble.sh call) back in $DEST before the temp dir is swept. Losing
# .caution/ orphans a live app: push has no remote, verify has no endpoint,
# and teardown cannot find the resource to destroy, so on BYOC the AWS stack
# sits there billing with nothing left that knows about it. Worse here than on
# the shim, because an ATTESTED hub has no SSH to recover through. If the
# restore itself fails, keep $STAGE rather than delete the only copy.
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

# Optional Nym mixnet reception. --nym runs the hub's own mixnet client so shims
# can submit over the mixnet; the enclave then needs egress to the gateway/nym-api
# set (operator-allowlisted via --nym-egress), APPENDED to the indexer egress
# above, and ZIH_NYM in the env. Additive: the HTTP ingress stays for transitional
# clearnet shims (the no-inbound-port tightening is M7).
NYM_ENV_FILE="$STAGE/nym_env.txt"
: > "$NYM_ENV_FILE"
if [ -n "$NYM_EGRESS" ] && [ "$NYM" != true ]; then
	echo "error: --nym-egress given without --nym. Nothing would use it." >&2
	exit 2
fi
if [ "$NYM" = true ]; then
	[ -n "$NYM_EGRESS" ] || {
		echo "error: --nym needs at least one --nym-egress rule (the gateway(s) and" >&2
		echo "       nym-api set the host operator allowlists). None given." >&2
		exit 2
	}
	# One egress block per --nym-egress rule, tallying the nym-api and resolver
	# hosts on the way past. A rule does not say which endpoint it was meant for, so
	# the role is read off the PORT: 53 is a resolver, 443 a nym-api, anything else
	# a gateway. That inference only drives the warnings below and never changes a
	# byte of what is emitted, so a rule the tally reads wrongly still lands in the
	# HCL exactly as given.
	#
	# Distinct CIDRs are counted rather than rules, because one host allowlisted on
	# two protocols (udp and tcp 53, say) is still ONE host, and it is hosts that
	# fail, not rules.
	nym_api_cidrs=""
	nym_api_n=0
	nym_dns_cidrs=""
	nym_dns_n=0
	for rule in $NYM_EGRESS; do
		cidr=${rule%%:*}; rest=${rule#*:}
		port=${rest%%:*}; proto=${rest#*:}
		[ "$proto" = "$rest" ] && proto=tcp
		echo "$port" | grep -qE '^[0-9]+$' || {
			echo "error: --nym-egress rule '$rule' has a non-numeric port" >&2; exit 2; }
		if [ "$port" = 443 ]; then
			case " $nym_api_cidrs " in
				*" $cidr "*) : ;;
				*) nym_api_cidrs="$nym_api_cidrs $cidr"; nym_api_n=$((nym_api_n + 1)) ;;
			esac
		fi
		if [ "$port" = 53 ]; then
			case " $nym_dns_cidrs " in
				*" $cidr "*) : ;;
				*) nym_dns_cidrs="$nym_dns_cidrs $cidr"; nym_dns_n=$((nym_dns_n + 1)) ;;
			esac
		fi
		printf '\n    # Nym mixnet egress (gateway / nym-api / DNS / Nyx), operator-allowlisted.\n' >> "$EGRESS"
		printf '    egress {\n      cidr_ipv4   = "%s"\n      port        = %s\n      ip_protocol = "%s"\n    }\n' \
			"$cidr" "$port" "$proto" >> "$EGRESS"
	done

	# THE NYM-API RULES AND THE CONFIGURED NYM-API HAVE TO AGREE, and nothing but
	# this check will ever tell you that they have stopped agreeing. Kept verbatim
	# with the shim's copy, because an operator reading only this script needs the
	# same warning.
	#
	# The hub has NO --nym-apis option: the endpoint set is nym-sdk's built-in
	# mainnet list, compiled into the binary, so the allowlist follows the SDK and
	# never the other way round. That list holds THREE HTTPS endpoints, which the
	# client shuffles at startup and rotates through on a network error:
	#
	#   validator.nymtech.net                one host (92.39.63.14 as measured)
	#   nym-frontdoor.global.ssl.fastly.net  fronted, shared CDN edge addresses
	#   cdn1.media-platform.net              one host
	#
	# So allowlisting exactly one of the three costs twice over. Most client builds
	# begin on one of the two this enclave blocks and spend a connect timeout before
	# rotating onto the one that answers, which is part of what makes a rebuild loop
	# hammer the nym-api. And on the day the single allowlisted address moves or
	# stops answering, all three attempts fail, no topology is ever fetched, and the
	# hub's mixnet client never connects: shims can then reach it over nothing but
	# the transitional clearnet path, with no console to say why.
	#
	# The fix is one --nym-egress rule per address that `dig +short
	# validator.nymtech.net` returns AT ASSEMBLE TIME. The rule is a snapshot of
	# DNS, not a configuration value, so it has to be re-taken on every redeploy.
	# Adding cdn1.media-platform.net's address as a SECOND, independent nym-api
	# costs one further /32 and is the cheap redundancy here. Do NOT allowlist the
	# Fastly frontdoor: those are shared CDN edge addresses, and permitting them
	# would let anything running in this enclave reach every origin behind that
	# CDN, which for the enclave that holds migrations in PLAINTEXT is a far larger
	# hole than the redundancy is worth.
	if [ "$nym_api_n" = 0 ]; then
		echo "==> WARNING: no tcp:443 --nym-egress rule, so NO nym-api is reachable. The"
		echo "    mixnet client refreshes the topology from a nym-api over HTTPS before it"
		echo "    can route a single Sphinx packet, so the hub's client fails closed on the"
		echo "    server. Add one '<nym-api-ip>/32:443:tcp' rule per address that"
		echo "    'dig +short validator.nymtech.net' returns."
	elif [ "$nym_api_n" = 1 ]; then
		echo "==> WARNING: exactly ONE nym-api address is allowlisted:$nym_api_cidrs"
		echo "    The nym-api endpoint is NOT configurable (there is no --nym-apis option),"
		echo "    so this rule has to match what 'dig +short validator.nymtech.net' returns"
		echo "    TODAY. If that address moves, topology refresh fails and the hub receives"
		echo "    nothing over the mixnet, silently. Pass one rule per returned address,"
		echo "    re-checked on every redeploy; see the note above for the second nym-api"
		echo "    worth adding."
	fi

	# WHAT STILL NEEDS DNS, and what does not. The indexers do not: ZIH_INDEXERS is
	# a list of literal addresses and ZIH_INDEXER_TLS names what each certificate
	# must say, so the enclave dials IPs and authenticates a name. Nor do the shims,
	# which reach this hub THROUGH its gateway over the mixnet and never by address.
	# Two things do, and both are inside nym-sdk: the nym-api endpoints above are
	# hostnames with no IP-literal alternative, and the entry gateway is dialled by
	# the HOSTNAME the topology carries, ahead of the IP addresses it carries
	# alongside it (the SDK has a no_hostname switch that would reverse that order,
	# and the driver does not set it). So DNS cannot be dropped from this deployment
	# by editing egress; it needs driver work. Tracked in NYM_PLAN.md M6.
	if [ "$nym_dns_n" = 0 ]; then
		echo "==> WARNING: no DNS (udp:53) --nym-egress rule. On the DEFAULT Nym network"
		echo "    the enclave resolves gateway/nym-api NAMES and has no resolver, so the"
		echo "    hub's mixnet client fails closed on the server. Add a"
		echo "    '<resolver>/32:53:udp' rule, or pin every endpoint by IP."
	elif [ "$nym_dns_n" = 1 ]; then
		echo "==> NOTE: exactly one resolver is allowlisted:$nym_dns_cidrs"
		echo "    That is a single point of failure the enclave cannot report on. If it"
		echo "    stops answering, every gateway and nym-api lookup fails, the hub receives"
		echo "    nothing over the mixnet, and /nym-status shows only a client that never"
		echo "    connects. A second '--nym-egress <resolver>/32:53:udp' costs one more /32"
		echo "    on the resolver port and grants no other reach, so it is the cheapest"
		echo "    redundancy available here. It helps only if the enclave's /etc/resolv.conf"
		echo "    actually lists that resolver: this allowlist PERMITS a resolver, it does"
		echo "    not choose one."
	fi
	{
		printf '\n      # Also accept submissions over the Nym mixnet (M5). The hub logs its\n'
		printf '      # own Nym address at startup; publish it to shims as --hub-nym.\n'
		printf '      ZIH_NYM = "true"\n'
		[ -n "$NYM_GATEWAY" ] && printf '      ZIH_NYM_GATEWAY = "%s"\n' "$NYM_GATEWAY"
	} > "$NYM_ENV_FILE"
	echo "==> MIXNET RECEPTION ON: the hub also runs a mixnet client. egress allowlist:$NYM_EGRESS"
	[ -n "$NYM_GATEWAY" ] && echo "    entry gateway pinned: $NYM_GATEWAY (stable address)" || echo "    entry gateway: SDK-selected (no --nym-gateway)"
fi

# The debug-console SSH key list, rendered into the debug{} block by awk below. One
# quoted entry per --ssh-key, at the block's indentation; an empty list otherwise.
# The require-with-debug rule is enforced up top, so "empty" here means a non-debug
# build where the list is inert anyway.
SSH_BLOCK="$STAGE/ssh_keys.txt"
if [ -n "$SSH_KEYS" ]; then
	printf '%s' "$SSH_KEYS" > "$STAGE/ssh_keys_raw.txt"
	{
		echo "    ssh_keys = ["
		while IFS= read -r ssh_key; do
			[ -n "$ssh_key" ] || continue
			printf '      "%s",\n' "$ssh_key"
		done < "$STAGE/ssh_keys_raw.txt"
		echo "    ]"
	} > "$SSH_BLOCK"
else
	echo "    ssh_keys = []" > "$SSH_BLOCK"
fi

# The manifest can record where this assembled repository is published, and
# verification hangs on it: Caution's own git remote is push-only, so the
# published repo is the ONLY route an auditor has to the deployed tree. Injected
# as a marker (like the egress and nym blocks) because a git URL may contain
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
	echo "    This is the enclave that holds migrations in PLAINTEXT, so without it"
	echo "    nobody -- including you -- can check what is running on it."
	echo "    Create a public repo for this assembled directory and pass its URL."
fi

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
# (The shim learned this from the zec.rocks operators, who lost a live
# deployment link to exactly this and recovered it from a chance backup.)
if [ -d "$DEST/.caution" ] || [ -d "$DEST/.git" ]; then
	echo "==> preserving .caution/ and .git across re-assembly"
	mkdir -p "$KEEP"
	if [ -d "$DEST/.caution" ]; then mv "$DEST/.caution" "$KEEP/.caution"; fi
	if [ -d "$DEST/.git" ]; then mv "$DEST/.git" "$KEEP/.git"; fi
fi

# The build context: the hub crate plus the parts of zebra/ its path dependency
# needs. Identical to what the reproducibility check builds, because it is the
# same script.
sh "$ZERO_ROOT/zeronym/hub/deploy/assemble.sh" "$DEST"

# Put them straight back, so the git steps at the bottom of this script see the
# preserved history and commit on top of it (files the new context no longer
# carries become staged deletions via `add -A`).
if [ -d "$KEEP/.caution" ]; then mv "$KEEP/.caution" "$DEST/.caution"; fi
if [ -d "$KEEP/.git" ]; then mv "$KEEP/.git" "$DEST/.git"; fi

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

# Render the enclave definition. Three markers carry multi-line content (the
# per-node egress blocks, the optional node-auth env, and the debug SSH key list),
# injected with awk from the files built above so no metacharacter has to survive
# sed. The scalar fields go through sed afterwards.
RENDERED="$STAGE/caution.hcl"
awk -v egress="$EGRESS" -v nymenv="$NYM_ENV_FILE" -v sshkeys="$SSH_BLOCK" -v appsrc="$APP_SRC_FILE" '
	/__EGRESS_BLOCKS__/  { while ((getline l < egress) > 0) print l; next }
	/__NYM_ENV__/        { while ((getline l < nymenv) > 0) print l; next }
	/__APP_SOURCE__/     { while ((getline l < appsrc) > 0) print l; next }
	/__DEBUG_SSH_KEYS__/ { while ((getline l < sshkeys) > 0) print l; next }
	{ print }
' "$HERE/caution.hcl.tmpl" > "$RENDERED"

sed \
	-e "s|__ENCLAVE_NAME__|$NAME|g" \
	-e "s|__INDEXERS__|$INDEXERS_ENV|g" \
	-e "s|__INDEXER_TLS__|$INDEXER_TLS|g" \
	-e "s|__TLS_DOMAIN__|$TLS_DOMAIN|g" \
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
serves:          $TLS_DOMAIN (TLS terminated in-enclave, ACME production)
broadcasts via:  $INDEXERS_ENV verified as $INDEXER_TLS
mixnet:          $([ "$NYM" = true ] && echo "ON (ZIH_NYM; egress:$NYM_EGRESS)" || echo "OFF (clearnet HTTP submit only)")
app source:      $([ -n "$APP_SOURCE" ] && echo "$APP_SOURCE" || echo "none (not independently verifiable)")
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
echo "  caution apps create    # no --name; auto-names the app and adds the 'caution' remote"
echo "  git push caution main  # builds and boots the enclave; prints its IP"
echo ""
echo "DNS: Caution uses managed DNS, so the record is a CNAME to"
echo "  <app-id>.apps.caution.sh. Create it AFTER 'apps create' (which prints the"
echo "  id) and BEFORE the push: the push boots the enclave and orders the"
echo "  certificate, and ACME needs the name already pointing here."
echo ""
echo "Then publish this repo at the --app-source URL: push main and tag the commit"
echo "(the manifest pins branch AND commit), then 'caution verify' from this directory."
echo ""
echo "To REDEPLOY after re-assembling: git push caution main"
echo "  (.caution/ and .git are preserved across re-assembly, so the push fast-forwards)."
echo "If the push is refused (unrelated history, or the app is in a failed state):"
echo "  echo y | caution apps destroy <app-id>"
echo "  caution apps create && git push caution main   # new app id: repoint the CNAME"

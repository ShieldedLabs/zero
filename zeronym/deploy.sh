#!/usr/bin/env sh
# One-shot Zeronym deploy to Caution + Vultr DNS.
#
# Reads a config file (default ./deploy.env, or the path in $1), assembles the
# shim or hub Caution deploy repo, creates + pushes the app, then points a Vultr
# DNS record at whatever the deploy tells it to (managed-DNS CNAME, or an A
# record). Collapses the old "assemble -> apps create -> set DNS by hand -> push"
# dance, with its ordering foot-guns, into one command.
#
# The DNS record is set BETWEEN `apps create` and the push, which is the ordering
# managed DNS requires: the push boots the enclave and orders the certificate, and
# ACME can only validate a name that already resolves (5 issuances per name per
# week, so a push into missing DNS burns one). The target is derivable from the app
# id alone, so nothing has to be scraped out of the push output first.
#
# For a hub deployed with NYM=1 it then waits for that hub to publish its own Nym
# address (`GET /nym-address`) AND to report that it is actually on the mixnet
# (`GET /nym-status`), and writes the address to stdout, the only thing this
# script puts there, so a caller can capture it directly. That read costs nothing
# now, but it used to require the enclave console, which only --debug opens and
# --debug turns attestation OFF: reading a hub's address and proving its binary
# were mutually exclusive. DEBUG=1 here is now about the SSH console alone.
#
# Requires: git, curl, jq, the `caution` CLI (LOGGED IN), and VULTR_API_KEY in the
# environment. POSIX sh; assemble scripts build from `git archive HEAD`, so commit
# your code first.
#
#   export VULTR_API_KEY=...          # never put this in deploy.env
#   caution login --qr --username <name>
#   ./zeronym/deploy.sh               # uses ./deploy.env
#   HUB_NYM=$(./zeronym/deploy.sh hub.env)   # deploy a hub, keep its Nym address
set -eu

log()  { printf '==> %s\n' "$*" >&2; }
warn() { printf '!!  %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing dependency: $1"; }

need git; need curl; need jq; need caution

CONFIG=${1:-deploy.env}
[ -f "$CONFIG" ] || die "config not found: $CONFIG (copy deploy.env.example to deploy.env)"
# shellcheck disable=SC1090
. "$CONFIG"

: "${COMPONENT:?set COMPONENT=shim|hub in $CONFIG}"
: "${NAME:?set NAME}"
: "${TLS_DOMAIN:?set TLS_DOMAIN}"
: "${DNS_DOMAIN:?set DNS_DOMAIN (the Vultr-hosted zone)}"
: "${VULTR_API_KEY:?export VULTR_API_KEY in your environment (not deploy.env)}"
DNS_TTL=${DNS_TTL:-300}
DEBUG=${DEBUG:-1}
SSH_PUBKEY_FILE=${SSH_PUBKEY_FILE:-$HOME/.ssh/id_ed25519.pub}
DNS_CNAME_TRAILING_DOT=${DNS_CNAME_TRAILING_DOT:-1}
# How long to wait for a NYM=1 hub to come up ON THE MIXNET after the push has
# already reported success. The two events are minutes apart: the push returns
# once the enclave serves TLS and passes its health check, while the mixnet
# client is still negotiating a gateway behind it.
NYM_WAIT_SECS=${NYM_WAIT_SECS:-300}
NYM_POLL_SECS=${NYM_POLL_SECS:-10}
HUB_NYM_ADDR=""    # the hub's published address, once it is known and checked
NYM_GAVE_UP=""     # why the wait above ended without one, if it did

# The Vultr record NAME is the label under the zone (e.g. "test-shim-nym-1").
case "$TLS_DOMAIN" in
  *".$DNS_DOMAIN") RECORD_NAME=${TLS_DOMAIN%".$DNS_DOMAIN"} ;;
  "$DNS_DOMAIN")   RECORD_NAME="" ;;   # apex
  *) die "TLS_DOMAIN ($TLS_DOMAIN) is not under DNS_DOMAIN ($DNS_DOMAIN)" ;;
esac

vultr() {  # vultr METHOD /path   (body, if any, on stdin)
  _m=$1; _p=$2
  case "$_m" in
    POST|PATCH) curl -fsS -X "$_m" -H "Authorization: Bearer $VULTR_API_KEY" \
                  -H "Content-Type: application/json" --data @- "https://api.vultr.com/v2$_p" ;;
    *)          curl -fsS -X "$_m" -H "Authorization: Bearer $VULTR_API_KEY" \
                  "https://api.vultr.com/v2$_p" ;;
  esac
}

# set_dns_record TYPE DATA — replace every record for this name in the zone.
set_dns_record() {
  _type=$1; _data=$2
  log "DNS: ${RECORD_NAME:-@}.$DNS_DOMAIN  $_type  ->  $_data"
  log "removing existing '${RECORD_NAME:-@}' records in $DNS_DOMAIN ..."
  vultr GET "/domains/$DNS_DOMAIN/records?per_page=500" \
    | jq -r --arg n "$RECORD_NAME" '.records[] | select(.name==$n) | .id' \
    | while IFS= read -r rid; do
        [ -n "$rid" ] || continue
        log "  delete $rid"
        vultr DELETE "/domains/$DNS_DOMAIN/records/$rid" >/dev/null
      done
  jq -nc --arg name "$RECORD_NAME" --arg type "$_type" --arg data "$_data" --argjson ttl "$DNS_TTL" \
    '{name:$name, type:$type, data:$data, ttl:$ttl}' \
    | vultr POST "/domains/$DNS_DOMAIN/records" >/dev/null
  log "DNS record set."
}

ROOT=$(git rev-parse --show-toplevel)
ASSEMBLE="$ROOT/zeronym/$COMPONENT/deploy/caution/assemble-caution.sh"
[ -f "$ASSEMBLE" ] || die "no assemble script for COMPONENT=$COMPONENT ($ASSEMBLE)"

# Assemble builds from HEAD; a dirty component tree would silently NOT be deployed.
if [ -n "$(git -C "$ROOT" status --porcelain -- "zeronym/$COMPONENT")" ]; then
  die "zeronym/$COMPONENT has uncommitted changes; commit them first (assemble uses git archive HEAD)"
fi

# ---------- build the assemble argument list ----------
set -- --name "$NAME" --tls-domain "$TLS_DOMAIN"
if [ "$COMPONENT" = shim ]; then
  : "${BACKEND:?set BACKEND for a shim}"; : "${BACKEND_TLS:?set BACKEND_TLS for a shim}"
  set -- "$@" --backend "$BACKEND" --backend-tls "$BACKEND_TLS"
  [ -n "${HUB_NYM:-}" ] && set -- "$@" --hub-nym "$HUB_NYM"
elif [ "$COMPONENT" = hub ]; then
  : "${INDEXERS:?set INDEXERS for a hub}"; : "${INDEXER_TLS:?set INDEXER_TLS for a hub}"
  set -- "$@" --indexers "$INDEXERS" --indexer-tls "$INDEXER_TLS"
  [ "${NYM:-0}" = 1 ] && set -- "$@" --nym
  # The hub's ack-wait-before-retransmit. Passed through only when set, so a
  # local or non-enclave hub keeps the SDK default. See deploy.env.example for
  # why 15000 is the value for an enclave hub.
  [ -n "${HUB_ACK_WAIT_MS:-}" ] && set -- "$@" --ack-wait-ms "$HUB_ACK_WAIT_MS"
else
  die "COMPONENT must be shim or hub (got: $COMPONENT)"
fi
if [ -n "${NYM_EGRESS:-}" ]; then
  for rule in $NYM_EGRESS; do set -- "$@" --nym-egress "$rule"; done
fi
if [ "$DEBUG" = 1 ]; then
  [ -f "$SSH_PUBKEY_FILE" ] || die "SSH_PUBKEY_FILE not found: $SSH_PUBKEY_FILE"
  set -- "$@" --ssh-key "$(cat "$SSH_PUBKEY_FILE")" --debug
else
  [ -n "${APP_SOURCE:-}" ] || die "DEBUG=0 (attested) requires APP_SOURCE (public repo URL for caution verify)"
  set -- "$@" --app-source "$APP_SOURCE"
fi

# ---------- assemble ----------
log "assembling $COMPONENT deploy repo from HEAD ..."
ASSEMBLE_OUT=$(cd "$ROOT" && sh "$ASSEMBLE" "$@")
printf '%s\n' "$ASSEMBLE_OUT" >&2
DEST=$(printf '%s\n' "$ASSEMBLE_OUT" | sed -n 's/^==> assembled: \([^ ]*\) .*/\1/p' | tail -1)
[ -n "$DEST" ] && [ -d "$DEST" ] || die "could not locate the assembled directory in the assemble output"
log "assembled at $DEST"

# ---------- optional: destroy the previous app for this repo (immutable apps) ----------
DEPLOY_JSON="$DEST/.caution/deployment.json"
if [ "${REDEPLOY_DESTROY_OLD:-0}" = 1 ] && [ -f "$DEPLOY_JSON" ]; then
  OLD_ID=$(jq -r '.app_id // .id // empty' "$DEPLOY_JSON" 2>/dev/null || true)
  if [ -n "$OLD_ID" ]; then
    log "destroying previous app $OLD_ID ..."
    caution apps destroy "$OLD_ID" --force >&2 || warn "destroy failed (continuing)"
  fi
fi

# ---------- create + push ----------
log "creating the Caution app ..."
CREATE_OUT=$(cd "$DEST" && caution apps create 2>&1)
printf '%s\n' "$CREATE_OUT" >&2
APP_ID=$(printf '%s\n' "$CREATE_OUT" | sed -n 's/^[[:space:]]*ID:[[:space:]]*\([0-9a-f-]\{36\}\).*/\1/p' | head -1)
[ -n "$APP_ID" ] || die "could not parse the new app ID from 'caution apps create' (is the session alive?)"
log "app id: $APP_ID"

# ---------- DNS, BEFORE the push ----------
# Ordering is load-bearing and used to be wrong here (DNS was set after the push).
# The push boots the enclave AND orders the certificate, and ACME can only validate
# a name that already resolves — against a budget of 5 issuances per name per week,
# so a push into missing DNS burns one. Caution's managed record is always a CNAME
# to <app-id>.apps.caution.sh, and `apps create` above already told us the id, so
# nothing has to be parsed out of the push output to know the target.
REC_TYPE=CNAME
REC_DATA="$APP_ID.apps.caution.sh"
[ "$DNS_CNAME_TRAILING_DOT" = 1 ] && REC_DATA="$REC_DATA."   # absolute; Vultr never appends the zone
set_dns_record "$REC_TYPE" "$REC_DATA"

log "pushing (build + boot + health check; ~15-20 min cold, faster cached) ..."
PUSH_OUT=$(cd "$DEST" && git push caution main 2>&1)
printf '%s\n' "$PUSH_OUT" >&2
printf '%s\n' "$PUSH_OUT" | grep -qi "Deployment successful" || \
  die "deploy did not report success. DNS for ${RECORD_NAME:-@}.$DNS_DOMAIN was already \
pointed at $REC_DATA; leaving it in place (harmless, and correct if you redeploy this \
same app id). Check the push output above."

# If the platform names a DIFFERENT target than the app-id CNAME we assumed, correct
# it. Expected to be a no-op; it exists so a platform change cannot silently leave a
# deployment pointing at the wrong host.
ANNOUNCED=$(printf '%s\n' "$PUSH_OUT" | sed -n 's/.*[Pp]ointing to \([A-Za-z0-9._-]\{1,\}\).*/\1/p' | tail -1)
[ -n "$ANNOUNCED" ] || ANNOUNCED=$(printf '%s\n' "$PUSH_OUT" | sed -n 's/.*DNS target:[[:space:]]*\([A-Za-z0-9._-]\{1,\}\).*/\1/p' | tail -1)
if [ -n "$ANNOUNCED" ]; then
  ANNOUNCED=${ANNOUNCED%.}
  if [ "$ANNOUNCED" != "${REC_DATA%.}" ]; then
    warn "the deploy announced '$ANNOUNCED', not the '$APP_ID.apps.caution.sh' we set; correcting the record"
    REC_DATA="$ANNOUNCED"
    [ "$DNS_CNAME_TRAILING_DOT" = 1 ] && REC_DATA="$REC_DATA."
    set_dns_record "$REC_TYPE" "$REC_DATA"
  fi
fi

# ---------- publish the app-source (attested deploys only) ----------
# `caution verify` clones the --app-source URL and rebuilds from it; Caution's own
# remote is push-only, so the assembled tree must ALSO live at a public, clonable
# repo, or the attestation is not independently verifiable. Push the EXACT deployed
# commit and tag it: the manifest pins branch AND commit, and a branch tip moves
# and can be garbage-collected. Push over APP_SOURCE_PUSH if set (e.g. an ssh URL),
# else the https APP_SOURCE the manifest records. Nothing secret is published:
# backend IP, hub address and egress are public by design, the only key is an SSH
# *public* key.
if [ "$DEBUG" != 1 ] && [ -n "${APP_SOURCE:-}" ]; then
  APP_SOURCE_PUSH=${APP_SOURCE_PUSH:-$APP_SOURCE}
  APP_SOURCE_TAG=${APP_SOURCE_TAG:-deploy-$APP_ID}
  log "publishing the app-source to $APP_SOURCE_PUSH (tag $APP_SOURCE_TAG) ..."
  ( cd "$DEST" &&
    { git remote remove app-source 2>/dev/null || true; } &&
    git remote add app-source "$APP_SOURCE_PUSH" &&
    git push app-source HEAD:main &&
    git tag -f "$APP_SOURCE_TAG" &&
    git push -f app-source "refs/tags/$APP_SOURCE_TAG"
  ) >&2 || die "app-source publish FAILED. The enclave is up, but it is not \
independently verifiable until '$DEST' is pushed to $APP_SOURCE_PUSH. Fix auth \
(gh/ssh) and push by hand, or re-run."
  log "app-source published: $APP_SOURCE @ $APP_SOURCE_TAG"
  log "verify with: caution verify (expect PCR0/1 FAILED on Caution's floating framework; PCR2 is the check that matters)"
fi

# ---------- the hub's Nym address ----------
# Every shim is built against this string, and the hub mints it inside the
# enclave, so it has to be read back out of a running deployment. It is read over
# HTTP from the hub itself, which is what makes an ATTESTED hub deployable at
# all: the enclave console is only open with --debug, and --debug disables
# attestation, so an address that could only be read from the console could never
# belong to a hub that had also been proven.
#
# The wait is not politeness, it is required. A push reports success once the
# enclave serves TLS and answers its health check, and the mixnet client connects
# some minutes after that; in between, the hub answers /nym-address with a 503
# that says so. That is a normal state on a fresh deploy, not a failure, so the
# only way to tell "not yet" from "never" is to keep asking until a deadline.
#
# A published address is NOT evidence that the hub can receive anything. The hub
# deliberately KEEPS the last address it published even after its mixnet client
# dies, because shims are baked against that string and it comes back on the next
# rebuild. So /nym-address and /healthz both answered 200 for hours on
# 2026-08-14 while the hub was carrying no mixnet traffic at all, and the whole
# afternoon went into suspecting the mixnet. /nym-status is the endpoint that
# separates "has an address" from "is reachable", so require mixnet_connected
# there before treating an address as usable.
if [ "$COMPONENT" = hub ] && [ "${NYM:-0}" = 1 ]; then

  # hub_get PATH: GET one of the hub's own endpoints, leaving the HTTP status in
  # $_code and the body in $_body. Deliberately no -f: a 503 from /nym-address is
  # an ANSWER here, and only the status code tells that apart from a 404 (an
  # image predating the endpoint, or DNS still pointing at some other host).
  # curl appends the code as the last space-separated field; neither an address
  # nor the status JSON contains a space, so the split cannot be ambiguous. A
  # transport failure (DNS, TLS, timeout) yields the code 000 and an empty body,
  # which is exactly how a name whose certificate has not been issued yet reads.
  hub_get() {
    _resp=$(curl -s --max-time 20 -w ' %{http_code}' "https://$TLS_DOMAIN$1") || true
    _code=${_resp##* }
    _body=${_resp% *}
  }

  # is_nym_address ADDR: the same structural check the shim applies to its own
  # --hub-nym (identity.encryption@gateway; see is_nym_address in
  # shim/src/config.rs), so this cannot reject a value the shim would accept. It
  # additionally requires the base58 character set, which the shim's version has
  # no reason to: this value arrives as an HTTP body rather than as an argv
  # entry, and a truncated read or an error page must never be baked into a
  # shim's config, where it would surface much later as lookups that time out
  # against a hub that does not exist.
  is_nym_address() {
    case $1 in
      ''|*[!0-9A-Za-z.@]*) return 1 ;;
    esac
    _keys=${1%%@*}; _gateway=${1#*@}
    _identity=${_keys%%.*}; _encryption=${_keys#*.}
    [ "$_keys" != "$1" ] || return 1            # there was an '@' at all
    [ "$_encryption" != "$_keys" ] || return 1  # and a '.' in front of it
    case $_gateway in *@*) return 1 ;; esac
    case $_encryption in *.*) return 1 ;; esac
    [ -n "$_identity" ] && [ -n "$_encryption" ] && [ -n "$_gateway" ]
  }

  log "waiting up to ${NYM_WAIT_SECS}s for the hub to publish its Nym address AND report mixnet_connected ..."
  _deadline=$(( $(date +%s) + NYM_WAIT_SECS ))
  while :; do
    hub_get /nym-address
    if [ "$_code" != 200 ]; then
      _why="/nym-address answered $_code (503 until the mixnet client connects; 000 while DNS or the certificate is still settling; 404 from an image older than the endpoint)"
    elif ! is_nym_address "$_body"; then
      _why="/nym-address answered 200 with a body that is not an identity.encryption@gateway address: '$_body'"
    else
      _addr=$_body
      hub_get /nym-status
      if [ "$_code" != 200 ]; then
        _why="/nym-address published an address but /nym-status answered $_code, so reachability is unconfirmed"
      elif ! printf '%s' "$_body" | jq -e '.mixnet_connected == true' >/dev/null 2>&1; then
        _why="the hub published an address but reports mixnet_connected=false, so it is receiving nothing: $_body"
      else
        HUB_NYM_ADDR=$_addr
        break
      fi
    fi
    if [ "$(date +%s)" -ge "$_deadline" ]; then
      NYM_GAVE_UP=$_why
      break
    fi
    log "not ready: $_why"
    sleep "$NYM_POLL_SECS"
  done

  if [ -n "$HUB_NYM_ADDR" ]; then
    log "hub Nym address: $HUB_NYM_ADDR"
    log "confirmed reachable: /nym-status reports mixnet_connected=true"
    # The one value a caller wants back. Every other line this script prints goes
    # to stderr, so stdout carries the address and nothing else, and it is empty
    # when the checks above did not pass.
    printf '%s\n' "$HUB_NYM_ADDR"
  else
    warn "gave up after ${NYM_WAIT_SECS}s: $NYM_GAVE_UP"
  fi
fi

NYM_REPORT=$HUB_NYM_ADDR
[ -n "$NYM_REPORT" ] || NYM_REPORT="n/a (not a NYM=1 hub, or never published; see above)"

cat >&2 <<EOF

==================== DONE ====================
component  : $COMPONENT
app id     : $APP_ID
deploy dir : $DEST
serves     : https://$TLS_DOMAIN
verify     : $( [ "$DEBUG" != 1 ] && [ -n "${APP_SOURCE:-}" ] && printf '%s @ %s — run: caution verify' "$APP_SOURCE" "${APP_SOURCE_TAG:-}" || printf 'n/a (debug deploy, not attested; no app-source published)' )
DNS set    : ${RECORD_NAME:-@}.$DNS_DOMAIN  $REC_TYPE  $REC_DATA  (ttl ${DNS_TTL}s, Vultr)
hub nym    : $NYM_REPORT
next       : allow a minute for DNS + the managed cert, then point the wallet at
             $TLS_DOMAIN:443. A shim diverting to this hub sets HUB_NYM to the
             'hub nym' address above.
=============================================
EOF

# Fatal, but only after the banner: the app id and deploy dir above are the
# things an operator must not lose, and the enclave, DNS and app-source really
# did succeed. What failed is the hub's purpose, since a hub that is not on the
# mixnet receives nothing, and the non-zero exit is what stops a caller from
# capturing an empty address and baking it into a shim.
if [ -n "$NYM_GAVE_UP" ]; then
  die "the hub is deployed and serving TLS, but never reported a usable Nym address: \
$NYM_GAVE_UP. Do NOT point a shim at it yet. Re-check with: curl https://$TLS_DOMAIN/nym-status"
fi

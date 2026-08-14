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
# Requires: git, curl, jq, the `caution` CLI (LOGGED IN), and VULTR_API_KEY in the
# environment. POSIX sh; assemble scripts build from `git archive HEAD`, so commit
# your code first.
#
#   export VULTR_API_KEY=...          # never put this in deploy.env
#   caution login --qr --username <name>
#   ./zeronym/deploy.sh               # uses ./deploy.env
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


cat >&2 <<EOF

==================== DONE ====================
component  : $COMPONENT
app id     : $APP_ID
deploy dir : $DEST
serves     : https://$TLS_DOMAIN
verify     : $( [ "$DEBUG" != 1 ] && [ -n "${APP_SOURCE:-}" ] && printf '%s @ %s — run: caution verify' "$APP_SOURCE" "${APP_SOURCE_TAG:-}" || printf 'n/a (debug deploy, not attested; no app-source published)' )
DNS set    : ${RECORD_NAME:-@}.$DNS_DOMAIN  $REC_TYPE  $REC_DATA  (ttl ${DNS_TTL}s, Vultr)
next       : allow a minute for DNS + the managed cert, then point the wallet at
             $TLS_DOMAIN:443 (or read the Nym address if this was a hub).
=============================================
EOF

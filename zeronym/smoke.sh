#!/usr/bin/env sh
# Post-deploy smoke test for a Zeronym shim and hub.
#
# Run this the moment a deploy reports success. "Deployed" has repeatedly NOT
# meant "works": an enclave that boots, serves valid TLS and answers /healthz
# with 200 can still be carrying no mixnet traffic at all, and on 2026-08-14
# exactly that hub went unnoticed for hours because /nym-address and /healthz
# both answered 200 the whole time (a published Nym address outlives the client
# that published it). Every check below exists because something that looked
# healthy was not, and each one prints what it MEASURED — status, bytes, elapsed
# — so a passing line can be audited rather than believed.
#
#   ./smoke.sh https://zeronym-shim-8.shieldedinfra.net https://zeronym-hub-3.shieldedinfra.net
#   ./smoke.sh --hub  https://zeronym-hub-3.shieldedinfra.net     # hub only
#   ./smoke.sh --shim https://zeronym-shim-8.shieldedinfra.net    # shim only
#
# Exits non-zero if any check failed, and names the failures on the last line.
#
# Requires curl and python3, and nothing else: no jq, no grpcurl, no proto
# checkout, no cargo. python3 builds the protobuf/gRPC request frames inline.
# The gRPC checks additionally need a curl built WITH HTTP/2 (`curl -V` must
# list HTTP2); the shim is an HTTP/2-only gRPC server and gRPC status arrives in
# HTTP/2 trailers, which HTTP/1.1 cannot carry. Git Bash's bundled curl on
# Windows is built without HTTP/2 — run those checks from WSL, or install a
# curl with nghttp2.
#
# Environment overrides:
#   SMOKE_LOOKUP_MAX_SECS   GetTransaction must finish within this (default 15)
#   SMOKE_LOOKUP_HARD_SECS  curl's own ceiling on the two heavy calls (default 90)
#   SMOKE_HTTP_TIMEOUT      ceiling on the small JSON calls (default 20)
#   SMOKE_BLOCK_START       first height of the GetBlockRange check (default 3444100)
#   SMOKE_FIXTURE           path to v6_migration.bin (default: alongside this script)

# Deliberately NOT `set -e`: a failing check must be reported and the remaining
# checks still run, because the useful diagnosis is usually the SHAPE of the
# failures (hub mixnet down => shim lookups fail too) rather than the first one.
set -u

SMOKE_LOOKUP_MAX_SECS=${SMOKE_LOOKUP_MAX_SECS:-15}
SMOKE_LOOKUP_HARD_SECS=${SMOKE_LOOKUP_HARD_SECS:-90}
SMOKE_HTTP_TIMEOUT=${SMOKE_HTTP_TIMEOUT:-20}
SMOKE_BLOCK_START=${SMOKE_BLOCK_START:-3444100}
BLOCK_COUNT=50

# A real, mined, immutable mainnet transaction: 15,150 bytes at height
# 3,444,122, the one the lookup architecture was validated against on
# 2026-08-14 (0.64 s direct to na.zec.rocks, 4.2 s through a shim over Nym).
# Using a historical transaction rather than a fresh one means this check never
# depends on the chain tip, on a wallet, or on a test migration.
LOOKUP_TXID=d8250be3873370cba3eb34b283444a67dfe367f4774b22f3ea1846c056efcb28

# 5-byte gRPC frame header + RawTransaction{data: 15150 bytes (1 tag + 2 length),
# height: 3444122 (1 tag + 4 varint)} = 5 + 15153 + 5 = 15163. It is a fixed
# number for a mined transaction; the tolerance only absorbs a re-encoding.
LOOKUP_EXPECT_BYTES=15163
LOOKUP_BYTES_SLACK=64

# 50 mainnet blocks compact-encoded are tens of KB. A floor this low is not a
# real assertion about the content, only a guard against the failure that
# actually happens: a stream that opens 200 and then delivers nothing.
BLOCKRANGE_MIN_BYTES=1000

GRPC_PREFIX=/cash.z.wallet.sdk.rpc.CompactTxStreamer

PASSED=0
FAILED=0
FAILED_NAMES=""

pass() { PASSED=$((PASSED + 1)); printf 'PASS  %-22s %s\n' "$1" "$2"; }
fail() {
  FAILED=$((FAILED + 1))
  FAILED_NAMES="$FAILED_NAMES $1"
  printf 'FAIL  %-22s %s\n' "$1" "$2"
}
note() { printf '      %-22s %s\n' "" "$1"; }
die()  { printf 'smoke.sh: %s\n' "$*" >&2; exit 2; }

usage() {
  cat >&2 <<'EOF'
usage: smoke.sh [--clearnet] [--shim URL] [--hub URL] [SHIM_URL [HUB_URL]]

  smoke.sh https://zeronym-shim-8.shieldedinfra.net https://zeronym-hub-3.shieldedinfra.net
  smoke.sh --hub  https://zeronym-hub-3.shieldedinfra.net     # hub only
  smoke.sh --shim https://zeronym-shim-8.shieldedinfra.net    # shim only

--clearnet: the pair runs without the mixnet hop (hub --http-submit, shim --hub/--hub-tls);
the four Nym-mode checks are replaced by their clearnet invariants rather than
reported as failures. Deployed this way from 2026-08-17.

Positional URLs are the shim then the hub; a bare URL whose host contains "hub"
is taken as the hub. https:// is assumed when no scheme is given. Use --shim /
--hub when in doubt: the run prints which role each URL was given.
EOF
  exit 2
}

# ---------------------------------------------------------------- arguments

SHIM=""
HUB=""
while [ $# -gt 0 ]; do
  case $1 in
    --shim) [ $# -ge 2 ] || usage; SHIM=$2; shift 2 ;;
    --hub)  [ $# -ge 2 ] || usage; HUB=$2;  shift 2 ;;
    --clearnet) CLEARNET=1; shift ;;
    -h|--help) usage ;;
    -*) printf 'smoke.sh: unknown option: %s\n' "$1" >&2; usage ;;
    *)
      case $1 in
        *hub*) [ -z "$HUB" ] || usage; HUB=$1 ;;
        *)
          if [ -z "$SHIM" ]; then SHIM=$1
          elif [ -z "$HUB" ]; then HUB=$1
          else usage
          fi ;;
      esac
      shift ;;
  esac
done
[ -n "$SHIM" ] || [ -n "$HUB" ] || usage

# Accept a bare hostname, and drop a trailing slash so paths concatenate cleanly.
normalise_url() {
  case $1 in
    http://*|https://*) _u=$1 ;;
    *) _u="https://$1" ;;
  esac
  printf '%s' "${_u%/}"
}
[ -n "$SHIM" ] && SHIM=$(normalise_url "$SHIM")
[ -n "$HUB" ]  && HUB=$(normalise_url "$HUB")

# ---------------------------------------------------------------- preflight

command -v curl >/dev/null 2>&1 || die "curl not found"

# Windows ships a Microsoft Store STUB at ...\WindowsApps\python3.exe that blocks
# on a console prompt instead of running, so a naive `python3 -c ...` from Git
# Bash hangs forever rather than failing. Skip that path and take the first
# candidate that actually reports a major version of 3.
find_python() {
  for _cand in python3 python py; do
    _path=$(command -v "$_cand" 2>/dev/null) || continue
    [ -n "$_path" ] || continue
    case $_path in *WindowsApps*) continue ;; esac
    if [ "$("$_cand" -c 'import sys; print(sys.version_info[0])' </dev/null 2>/dev/null)" = 3 ]; then
      printf '%s' "$_cand"
      return 0
    fi
  done
  return 1
}
PY=$(find_python) || die "no working python3 found (tried python3, python, py)"

# The hub's inbound server is plain HTTP/1.1 by design (a POST of raw bytes, no
# trailers), so its checks need nothing special and run on any curl. The shim is
# an HTTP/2-only gRPC server behind an h2c-upstream proxy, so its checks need a
# curl that speaks HTTP/2.
HUB_PROTO=--http1.1
CLEARNET=${CLEARNET:-0}
SHIM_PROTO=--http2
# A cleartext shim (a local one, or an enclave reached without the platform's
# TLS in front) is h2c with no ALPN to negotiate over, and its HTTP/2-only
# server drops an HTTP/1.1 Upgrade request rather than answering it. Prior
# knowledge is what the repo's own local-testing recipe uses.
case ${SHIM:-} in http://*) SHIM_PROTO=--http2-prior-knowledge ;; esac
HAVE_HTTP2=1
if ! curl -V 2>/dev/null | grep -q HTTP2; then
  HAVE_HTTP2=0
  SHIM_PROTO=--http1.1
fi

# One directory per run, and one NUMBERED file per request inside it. An earlier
# version of this test reused a fixed /tmp path per check and reported a PASS off
# a body left behind by a previous attempt — a request that never happened looked
# like one that succeeded. Nothing here is ever read unless the request that
# wrote it just returned.
RUN_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t zeronym-smoke) || die "mktemp failed"
REQ=0

printf '\n'
printf 'zeronym smoke test\n'
[ -n "$SHIM" ] && printf '  shim : %s\n' "$SHIM"
[ -n "$HUB" ]  && printf '  hub  : %s\n' "$HUB"
printf '  tools: %s, %s\n' "$(curl -V 2>/dev/null | head -1 | cut -d' ' -f1-2)" "$("$PY" -V 2>&1)"
printf '  files: %s\n' "$RUN_DIR"
printf '\n'

if [ -n "$SHIM" ] && [ "$HAVE_HTTP2" = 0 ]; then
  printf 'NOTE  this curl is built WITHOUT HTTP/2 (`curl -V` has no HTTP2). The three\n'
  printf '      gRPC checks cannot run at all, and the shim JSON endpoints are tried\n'
  printf '      over HTTP/1.1, which only works because a proxy fronts the shim.\n'
  printf '      Run from WSL, or install a curl with nghttp2, for a real result.\n\n'
fi

# ----------------------------------------------------------------- plumbing

# http_request METHOD URL TIMEOUT [curl args...]
# Leaves CODE, SECS, BYTES, BODY, HDRS, CURL_RC and CURL_ERR describing exactly
# the response it just fetched.
http_request() {
  _method=$1; _url=$2; _timeout=$3
  shift 3
  REQ=$((REQ + 1))
  BODY="$RUN_DIR/$REQ.body"
  HDRS="$RUN_DIR/$REQ.head"
  _err="$RUN_DIR/$REQ.err"
  # Fresh, empty, and guaranteed to exist. Fresh because a check must never read
  # a file some earlier request left behind, and guaranteed to exist because curl
  # writes no -o file at all when the connection itself fails.
  rm -f "$BODY" "$HDRS" "$_err"
  : > "$BODY"; : > "$HDRS"; : > "$_err"
  _w=$(curl -sS --max-time "$_timeout" -X "$_method" \
        -o "$BODY" -D "$HDRS" -w '%{http_code} %{time_total}' \
        "$@" "$_url" 2>"$_err")
  CURL_RC=$?
  CODE=${_w%% *}
  SECS=${_w##* }
  [ -n "$CODE" ] || CODE=000
  [ -n "$SECS" ] || SECS=0
  BYTES=$(wc -c < "$BODY" 2>/dev/null | tr -d ' ')
  [ -n "$BYTES" ] || BYTES=0
  CURL_ERR=$(tr -d '\r' < "$_err" 2>/dev/null | tr '\n' ' ')
}

# The response header dump carries HTTP/2 trailers too, which is where a
# SUCCESSFUL gRPC call puts grpc-status. A FAILED one is trailers-only, so its
# status lands in the initial header block and is always visible here.
header_value() { grep -i "^$2:" "$1" 2>/dev/null | tail -1 | sed 's/^[^:]*:[[:space:]]*//' | tr -d '\r'; }

json_field() {
  "$PY" - "$1" "$2" <<'PY'
import json, sys
try:
    with open(sys.argv[1], "rb") as f:
        doc = json.load(f)
except Exception:
    sys.exit(0)
if isinstance(doc, dict) and sys.argv[2] in doc:
    print(json.dumps(doc[sys.argv[2]]))
PY
}

excerpt() { head -c 200 "$1" 2>/dev/null | tr '\r\n' '  '; }

# A request that got no answer at all is its own verdict. Without this a refused
# connection is rendered as a wrong-looking response ("expected 404, got 000"),
# which sends the reader looking at the endpoint instead of at the host.
no_reply() {
  [ "$CURL_RC" = 0 ] && return 1
  fail "$1" "no reply after ${SECS}s: ${CURL_ERR:-curl exit $CURL_RC}"
  return 0
}

# POSIX sh cannot compare floats, and python3 is already a hard dependency here.
secs_gt() { "$PY" -c 'import sys; sys.exit(0 if float(sys.argv[1]) > float(sys.argv[2]) else 1)' "$1" "$2"; }

# A gRPC status that is visible and non-zero is a definitive failure. An ABSENT
# one is not: not every curl surfaces trailers, and a failed RPC returns an empty
# body anyway, which the byte-count check below already catches.
grpc_status_bad() {
  _st=$(header_value "$1" grpc-status)
  [ -n "$_st" ] && [ "$_st" != 0 ]
}
grpc_status_text() {
  _st=$(header_value "$1" grpc-status)
  if [ -z "$_st" ]; then printf 'grpc-status=<not surfaced>'
  else printf 'grpc-status=%s%s' "$_st" "$(_m=$(header_value "$1" grpc-message); [ -n "$_m" ] && printf ' (%s)' "$_m")"
  fi
}

grpc_call() {  # grpc_call METHOD REQUEST_FILE TIMEOUT
  http_request POST "$SHIM$GRPC_PREFIX/$1" "$3" "$SHIM_PROTO" \
    -H 'content-type: application/grpc' -H 'te: trailers' \
    --data-binary @"$2"
}

# -------------------------------------------------------------- hub checks
#
# The hub is checked first: if its mixnet client is down, the shim's lookup
# below cannot possibly pass, and reading the failures in that order saves a
# deployer from chasing the shim.

check_hub_nym_status() {
  # THE CHECK THIS SCRIPT EXISTS FOR. /healthz and /nym-address both answered
  # 200 for hours on 2026-08-14 while the hub's mixnet client was dead, because
  # process liveness and a published address survive the client. This endpoint
  # is the only one that reports whether the hub is on the mixnet RIGHT NOW.
  http_request GET "$HUB/nym-status" "$SMOKE_HTTP_TIMEOUT" "$HUB_PROTO"
  no_reply hub:nym-status && return
  _conn=$(json_field "$BODY" mixnet_connected)
  _deaths=$(json_field "$BODY" client_deaths)
  _fails=$(json_field "$BODY" consecutive_rebuild_failures)
  if [ "$CODE" = 200 ] && [ "$_conn" = true ]; then
    pass hub:nym-status "$CODE, mixnet_connected=true, client_deaths=${_deaths:-?}, rebuild_failures=${_fails:-?}, ${SECS}s"
  else
    fail hub:nym-status "$CODE, mixnet_connected=${_conn:-<absent>}, ${SECS}s ${CURL_ERR}"
    note "body: $(excerpt "$BODY")"
    note "a hub that is not on the mixnet accepts nothing a shim diverts, however healthy it looks"
  fi
}

check_hub_nym_address() {
  http_request GET "$HUB/nym-address" "$SMOKE_HTTP_TIMEOUT" "$HUB_PROTO"
  no_reply hub:nym-address && return
  _addr=$(tr -d '\r\n' < "$BODY" 2>/dev/null)
  _len=$(printf '%s' "$_addr" | wc -c | tr -d ' ')
  # identity.encryption@gateway, three base58 keys. Shape-checked because the
  # value is pasted into a shim's --hub-nym at assemble time, and a 503 body or
  # an error page in that field produces a shim that fails at its first divert.
  if [ "$CODE" = 200 ] && printf '%s' "$_addr" |
       grep -Eq '^[1-9A-HJ-NP-Za-km-z]{20,}\.[1-9A-HJ-NP-Za-km-z]{20,}@[1-9A-HJ-NP-Za-km-z]{20,}$'; then
    pass hub:nym-address "$CODE, well-formed, $_len chars, ${SECS}s"
    note "$_addr"
    note "every shim must be baked against exactly this value; it changes on a hub PROCESS restart"
  else
    fail hub:nym-address "$CODE, not an identity.encryption@gateway address, ${SECS}s ${CURL_ERR}"
    note "body: $(excerpt "$BODY")"
  fi
}

check_hub_submit_closed() {
  # The mixnet is the submit path. An open, unauthenticated clearnet POST / on a
  # 0.0.0.0/0 ingress has no legitimate user, so 404 here is a deployment
  # assertion: the transitional clearnet path was NOT left switched on.
  http_request POST "$HUB/" "$SMOKE_HTTP_TIMEOUT" "$HUB_PROTO" --data-binary ''
  no_reply hub:submit-closed && return
  if [ "$CODE" = 404 ]; then
    pass hub:submit-closed "$CODE as required, ${SECS}s"
  else
    fail hub:submit-closed "expected 404, got $CODE, ${SECS}s ${CURL_ERR}"
    note "the clearnet submit path looks OPEN; it should be indistinguishable from a path that never existed"
  fi
}

# ------------------------------------------------------------- shim checks

check_shim_nym_status() {
  http_request GET "$SHIM/nym-status" "$SMOKE_HTTP_TIMEOUT" "$SHIM_PROTO"
  no_reply shim:nym-status && return
  _conn=$(json_field "$BODY" mixnet_connected)
  _div=$(json_field "$BODY" diversion_configured)
  _deaths=$(json_field "$BODY" client_deaths)
  if [ "$CODE" = 200 ] && [ "$_conn" = true ] && [ "$_div" = true ]; then
    pass shim:nym-status "$CODE, mixnet_connected=true, diversion_configured=true, client_deaths=${_deaths:-?}, ${SECS}s"
  else
    fail shim:nym-status "$CODE, mixnet_connected=${_conn:-<absent>}, diversion_configured=${_div:-<absent>}, ${SECS}s ${CURL_ERR}"
    note "body: $(excerpt "$BODY")"
    # diversion_configured=false is the quiet one: the shim serves wallets
    # perfectly and forwards migrations to the operator in the clear, which is
    # the entire leak the system exists to close.
    [ "$_div" = false ] && note "diversion_configured=false means this shim has no hub and would FORWARD migrations to the operator"
  fi
}

# ---- clearnet mode: the same components, the mixnet hop replaced by HTTPS ----
#
# With --clearnet the four Nym-mode assertions above are wrong by construction:
# there is no mixnet client to be connected, no Nym address to publish, and the
# hub's POST / MUST be open because that is how the shim reaches it. Reporting a
# working clearnet pair as 4/8 FAILED is worse than useless -- it teaches people
# to ignore the script. So clearnet mode swaps in the invariants a clearnet pair
# actually has, rather than skipping checks silently: the status endpoints must
# still answer and must HONESTLY report no mixnet, and the hub's submit path must
# be reachable. Deployed this way from 2026-08-17 while the enclave's egress path
# throttles the mixnet client (see hub OPERATORS.md, "Clearnet mode").

check_hub_clearnet_status() {
  http_request GET "$HUB/nym-status" "$SMOKE_HTTP_TIMEOUT" "$HUB_PROTO"
  no_reply hub:clearnet-status && return
  _conn=$(json_field "$BODY" mixnet_connected)
  if [ "$CODE" = 200 ] && [ "$_conn" = false ]; then
    pass hub:clearnet-status "$CODE, mixnet_connected=false as expected (no client in clearnet mode), ${SECS}s"
  else
    fail hub:clearnet-status "$CODE, mixnet_connected=${_conn:-<absent>}, ${SECS}s ${CURL_ERR}"
    note "a clearnet hub must report honestly that it has no mixnet client; true here means the wrong build is deployed"
  fi
}

check_hub_submit_open() {
  # The inverse of submit-closed: on a clearnet hub POST / is the submit path and
  # must answer -- NOT 404 (path closed: ZIH_HTTP_SUBMIT unset) and NOT 000
  # (unreachable), either of which means shims cannot divert to this hub.
  #
  # A one-byte junk body gets 200, and that is CORRECT: the hub deliberately
  # admits unparseable payloads rather than refusing them (see
  # hub/tests/nym.rs an_unparseable_payload_is_admitted_not_refused), because a
  # hub that answered "not a transaction" would be an oracle for what it can
  # parse. So the pass condition is "the path answered like a submit path"; the
  # smoke test never sends a real transaction here (that would put junk in a
  # live batch), so it cannot and does not assert on the verdict.
  http_request POST "$HUB/" "$SMOKE_HTTP_TIMEOUT" "$HUB_PROTO" -H 'content-type: application/octet-stream' --data-binary x
  no_reply hub:submit-open && return
  case $CODE in
    200|400|413|415) pass hub:submit-open "$CODE: the submit path is open (200 = junk admitted by design, not an error), ${SECS}s" ;;
    404) fail hub:submit-open "404: submit path is CLOSED (ZIH_HTTP_SUBMIT unset?) -- shims cannot divert to this hub, ${SECS}s" ;;
    *)   fail hub:submit-open "unexpected $CODE, ${SECS}s ${CURL_ERR}" ;;
  esac
}

check_shim_clearnet_status() {
  http_request GET "$SHIM/nym-status" "$SMOKE_HTTP_TIMEOUT" "$SHIM_PROTO"
  no_reply shim:clearnet-status && return
  _conn=$(json_field "$BODY" mixnet_connected)
  if [ "$CODE" = 200 ] && [ "$_conn" = false ]; then
    pass shim:clearnet-status "$CODE, mixnet_connected=false as expected; diversion is over HTTPS, proven by shim:lookup below, ${SECS}s"
  else
    fail shim:clearnet-status "$CODE, mixnet_connected=${_conn:-<absent>}, ${SECS}s ${CURL_ERR}"
  fi
}

check_shim_healthz() {
  # The shim answers 503 here (not 200) once it cannot carry a migration, so
  # this is a real verdict rather than a process-liveness ping.
  http_request GET "$SHIM/healthz" "$SMOKE_HTTP_TIMEOUT" "$SHIM_PROTO"
  no_reply shim:healthz && return
  if [ "$CODE" = 200 ]; then
    pass shim:healthz "$CODE, $BYTES bytes, ${SECS}s"
  else
    fail shim:healthz "expected 200, got $CODE, ${SECS}s ${CURL_ERR}"
    note "body: $(excerpt "$BODY")"
  fi
}

check_shim_grpc_passthrough() {
  if [ "$HAVE_HTTP2" = 0 ]; then
    fail shim:grpc-passthrough "cannot run: this curl has no HTTP/2 support"
    return
  fi
  # An empty gRPC frame: a 5-byte header for a zero-length GetLightdInfo
  # request. Built with python because POSIX printf has no \xNN escape.
  _req="$RUN_DIR/getlightdinfo.grpc"
  rm -f "$_req"
  "$PY" -c 'import sys; sys.stdout.buffer.write(b"\x00" * 5)' > "$_req" || {
    fail shim:grpc-passthrough "could not build the request frame"; return; }
  grpc_call GetLightdInfo "$_req" "$SMOKE_HTTP_TIMEOUT"
  no_reply shim:grpc-passthrough && return
  if [ "$CODE" = 200 ] && [ "$BYTES" -gt 0 ] && ! grpc_status_bad "$HDRS"; then
    pass shim:grpc-passthrough "$CODE, $(grpc_status_text "$HDRS"), $BYTES bytes, ${SECS}s"
  else
    fail shim:grpc-passthrough "$CODE, $(grpc_status_text "$HDRS"), $BYTES bytes, ${SECS}s ${CURL_ERR}"
    note "the plain relay to the operator's indexer is broken, independent of the mixnet"
  fi
}

check_shim_lookup() {
  if [ "$HAVE_HTTP2" = 0 ]; then
    fail shim:lookup "cannot run: this curl has no HTTP/2 support"
    return
  fi
  _req="$RUN_DIR/gettransaction.grpc"
  rm -f "$_req"
  "$PY" - "$LOOKUP_TXID" > "$_req" <<'PY' || { fail shim:lookup "could not build the request frame"; return; }
import sys
# TxFilter.hash is INTERNAL byte order, the reverse of the hex a block explorer
# shows. Sending display order asks for a transaction that does not exist.
txid = bytes.fromhex(sys.argv[1])[::-1]
assert len(txid) == 32, "a txid is 32 bytes"
msg = b"\x1a" + bytes([len(txid)]) + txid          # field 3, length-delimited
sys.stdout.buffer.write(b"\x00" + len(msg).to_bytes(4, "big") + msg)
PY
  # The hard ceiling is deliberately far above the pass threshold: a lookup that
  # takes 90 s and one that curl abandoned at 15 s are different diagnoses, and
  # this check has to be able to report "slow" as distinct from "hung".
  grpc_call GetTransaction "$_req" "$SMOKE_LOOKUP_HARD_SECS"
  _low=$((LOOKUP_EXPECT_BYTES - LOOKUP_BYTES_SLACK))
  _high=$((LOOKUP_EXPECT_BYTES + LOOKUP_BYTES_SLACK))
  _measured="$CODE, $(grpc_status_text "$HDRS"), $BYTES bytes (expect ~$LOOKUP_EXPECT_BYTES), ${SECS}s (limit ${SMOKE_LOOKUP_MAX_SECS}s)"
  if [ "$CURL_RC" != 0 ]; then
    fail shim:lookup "no reply within ${SMOKE_LOOKUP_HARD_SECS}s: ${CURL_ERR:-curl exit $CURL_RC}"
    note "every diverted migration is looked up this way; a wallet cannot see its own transaction while this fails"
  elif [ "$CODE" != 200 ] || grpc_status_bad "$HDRS"; then
    fail shim:lookup "$_measured"
    note "UNAVAILABLE here usually means the hub was unreachable over the mixnet; check hub:nym-status above"
  elif [ "$BYTES" -lt "$_low" ] || [ "$BYTES" -gt "$_high" ]; then
    fail shim:lookup "$_measured"
    note "the transaction is mined and immutable, so its encoded size is fixed; a different size is a different answer"
  elif secs_gt "$SECS" "$SMOKE_LOOKUP_MAX_SECS"; then
    # Slow is a FAILURE, not a warning. Past a wallet's gRPC deadline the wallet
    # gives up and silently falls back to a non-private server, so the migration
    # it was protecting is exposed even though this lookup eventually returned.
    fail shim:lookup "TOO SLOW — $_measured"
    note "past a wallet's gRPC deadline the wallet gives up and silently falls back to a non-private server"
  else
    pass shim:lookup "$_measured"
  fi
}

# ---------------------------------------------------------------- the divert
#
# The one check that proves this shim is PRIVATE rather than merely alive.
#
# Every other check in this file passes on a shim with no hub configured, which
# runs forward-only: it hands the operator's indexer every transaction it is
# given, migrations included. `shim:lookup` above fetches a MINED transaction,
# and the operator's indexer serves that byte-identically -- so on 2026-08-17
# `--clearnet` reported "7/7, this pair is serving" for a deployment whose
# divert path had never been exercised at all.
#
# The fixture is consensus-INVALID by construction (a zero-filled halo2 proof),
# so it can never be mined and no indexer will accept it. That is what makes it
# a discriminator: a diverting shim classifies it as a migration, sends it to
# the hub, and gets back a txid; a forward-only shim shows it to the operator's
# indexer and gets back a rejection. Looking it up afterwards can then only be
# answered from the hub's queue, at height 0, and only if the hub really holds
# it -- so the pair of calls proves admission end to end.
#
# It leaves the fixture in the hub's live queue. The next flush drops it on the
# indexer's verdict, which is the same harmless thing `hub:submit-open` already
# does with its junk bytes.
MIGRATION_FIXTURE=${SMOKE_FIXTURE:-$(dirname "$0")/shim/tests/fixtures/v6_migration.bin}

check_shim_divert() {
  if [ "$HAVE_HTTP2" = 0 ]; then
    fail shim:divert "cannot run: this curl has no HTTP/2 support"
    return
  fi
  if [ ! -r "$MIGRATION_FIXTURE" ]; then
    fail shim:divert "fixture not readable: $MIGRATION_FIXTURE"
    note "run smoke.sh from a checkout, or point SMOKE_FIXTURE at shim/tests/fixtures/v6_migration.bin"
    return
  fi

  # ---- 1. submit: the shim must classify this as a migration and divert it.
  _req="$RUN_DIR/sendtransaction.grpc"
  rm -f "$_req"
  "$PY" - "$MIGRATION_FIXTURE" > "$_req" <<'PY' || { fail shim:divert "could not build the request frame"; return; }
import sys

def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)

with open(sys.argv[1], "rb") as f:
    tx = f.read()
# RawTransaction { bytes data = 1; uint64 height = 2 }. A wallet sends no height
# on a submit and proto3 omits a zero, so field 2 is absent -- byte-identical to
# what a real wallet puts on the wire.
msg = b"\x0a" + varint(len(tx)) + tx
sys.stdout.buffer.write(b"\x00" + len(msg).to_bytes(4, "big") + msg)
PY
  grpc_call SendTransaction "$_req" "$SMOKE_LOOKUP_HARD_SECS"
  _submit_secs=$SECS
  if [ "$CURL_RC" != 0 ]; then
    fail shim:divert "submit got no reply within ${SMOKE_LOOKUP_HARD_SECS}s: ${CURL_ERR:-curl exit $CURL_RC}"
    return
  fi
  if [ "$CODE" != 200 ] || grpc_status_bad "$HDRS"; then
    fail shim:divert "submit: $CODE, $(grpc_status_text "$HDRS"), ${SECS}s"
    note "UNAVAILABLE here is the shim failing closed because the hub was unreachable -- check the hub above"
    return
  fi

  # SendResponse { int32 error_code = 1; string error_message = 2 }, where the
  # shim puts the hub's txid in error_message exactly as lightwalletd does.
  _verdict=$("$PY" - "$BODY" <<'PY'
import sys

def take_varint(buf, i):
    n = shift = 0
    while True:
        b = buf[i]; i += 1
        n |= (b & 0x7F) << shift
        if not b & 0x80:
            return n, i
        shift += 7

try:
    with open(sys.argv[1], "rb") as f:
        body = f.read()
    if len(body) < 5 or body[0] != 0:
        print("ERR\tnot an uncompressed gRPC frame")
        raise SystemExit
    msg = body[5:5 + int.from_bytes(body[1:5], "big")]
    code, message, i = None, "", 0
    while i < len(msg):
        key, i = take_varint(msg, i)
        field, wire = key >> 3, key & 7
        if wire == 0:
            val, i = take_varint(msg, i)
            if field == 1:
                # int32 negative arrives as a 10-byte two's-complement varint.
                code = val - (1 << 64) if val >= 1 << 63 else val
        elif wire == 2:
            ln, i = take_varint(msg, i)
            val, i = msg[i:i + ln], i + ln
            if field == 2:
                message = val.decode("utf-8", "replace")
        else:
            print("ERR\tunexpected wire type %d" % wire)
            raise SystemExit
    print("%s\t%s" % (0 if code is None else code, message))
except SystemExit:
    raise
except Exception as exc:
    print("ERR\t%s" % exc)
PY
)
  _code=${_verdict%%	*}
  _txid=${_verdict#*	}
  if [ "$_code" = ERR ]; then
    fail shim:divert "submit reply did not parse as a SendResponse: $_txid"
    return
  fi
  if [ "$_code" != 0 ]; then
    fail shim:divert "submit REJECTED: error_code=$_code, error_message=\"$_txid\", ${_submit_secs}s"
    note "this is what a FORWARD-ONLY shim does: the fixture is consensus-invalid, so the operator's"
    note "indexer rejects it. A diverting shim never shows it to the indexer. Is ZIS_HUB set?"
    return
  fi
  if [ "$(printf '%s' "$_txid" | wc -c | tr -d ' ')" != 64 ]; then
    fail shim:divert "submit accepted but returned no txid (error_message=\"$_txid\"), ${_submit_secs}s"
    return
  fi

  # ---- 2. look it back up: only the hub's queue can answer this.
  _req2="$RUN_DIR/divert-lookup.grpc"
  rm -f "$_req2"
  "$PY" - "$_txid" > "$_req2" <<'PY' || { fail shim:divert "could not build the lookup frame"; return; }
import sys
txid = bytes.fromhex(sys.argv[1])[::-1]     # display order -> internal order
assert len(txid) == 32, "a txid is 32 bytes"
msg = b"\x1a" + bytes([len(txid)]) + txid   # TxFilter.hash, field 3
sys.stdout.buffer.write(b"\x00" + len(msg).to_bytes(4, "big") + msg)
PY
  grpc_call GetTransaction "$_req2" "$SMOKE_LOOKUP_HARD_SECS"
  _measured="submit ${_submit_secs}s, lookup ${SECS}s, $BYTES bytes"
  if [ "$CURL_RC" != 0 ]; then
    fail shim:divert "lookup got no reply within ${SMOKE_LOOKUP_HARD_SECS}s: ${CURL_ERR:-curl exit $CURL_RC}"
    return
  fi
  if [ "$CODE" != 200 ] || grpc_status_bad "$HDRS"; then
    fail shim:divert "lookup: $CODE, $(grpc_status_text "$HDRS"), $_measured"
    note "NOT_FOUND means the transaction is not in the hub's queue: it was never diverted there,"
    note "or a flush has already dropped it (it is consensus-invalid, so a flush always will)"
    return
  fi
  # The reply must be the fixture BYTE FOR BYTE at height 0. Height 0 is the
  # mempool sentinel: it came out of the hub's queue, not off the chain -- and
  # this transaction can never be on the chain, which is what makes the whole
  # assertion airtight.
  _echo=$("$PY" - "$BODY" "$MIGRATION_FIXTURE" <<'PY'
import sys

def take_varint(buf, i):
    n = shift = 0
    while True:
        b = buf[i]; i += 1
        n |= (b & 0x7F) << shift
        if not b & 0x80:
            return n, i
        shift += 7

try:
    with open(sys.argv[1], "rb") as f:
        body = f.read()
    with open(sys.argv[2], "rb") as f:
        want = f.read()
    if len(body) < 5 or body[0] != 0:
        print("ERR\tnot an uncompressed gRPC frame")
        raise SystemExit
    msg = body[5:5 + int.from_bytes(body[1:5], "big")]
    data, height, i = b"", 0, 0
    while i < len(msg):
        key, i = take_varint(msg, i)
        field, wire = key >> 3, key & 7
        if wire == 0:
            val, i = take_varint(msg, i)
            if field == 2:
                height = val
        elif wire == 2:
            ln, i = take_varint(msg, i)
            val, i = msg[i:i + ln], i + ln
            if field == 1:
                data = val
        else:
            print("ERR\tunexpected wire type %d" % wire)
            raise SystemExit
    if data != want:
        print("ERR\tthe %d bytes returned are not the %d-byte fixture" % (len(data), len(want)))
    elif height != 0:
        print("ERR\theight=%d, expected 0 (the mempool sentinel a queue hit carries)" % height)
    else:
        print("OK\t%d bytes byte-identical, height=0" % len(data))
except SystemExit:
    raise
except Exception as exc:
    print("ERR\t%s" % exc)
PY
)
  case $_echo in
    OK*)
      pass shim:divert "$_measured — ${_echo#*	}, txid $_txid"
      ;;
    *)
      fail shim:divert "lookup returned the wrong thing: ${_echo#*	} ($_measured)"
      ;;
  esac
}

check_shim_blockrange() {
  if [ "$HAVE_HTTP2" = 0 ]; then
    fail shim:blockrange "cannot run: this curl has no HTTP/2 support"
    return
  fi
  _end=$((SMOKE_BLOCK_START + BLOCK_COUNT - 1))
  _req="$RUN_DIR/getblockrange.grpc"
  rm -f "$_req"
  "$PY" - "$SMOKE_BLOCK_START" "$_end" > "$_req" <<'PY' || { fail shim:blockrange "could not build the request frame"; return; }
import sys

def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)

def delimited(tag, payload):
    return tag + varint(len(payload)) + payload

def block_id(height):
    return b"\x08" + varint(height)          # BlockID.height, field 1

msg = delimited(b"\x0a", block_id(int(sys.argv[1]))) \
    + delimited(b"\x12", block_id(int(sys.argv[2])))  # BlockRange.start, .end
sys.stdout.buffer.write(b"\x00" + len(msg).to_bytes(4, "big") + msg)
PY
  # Block sync is the SAFE channel: it is how a wallet legitimately receives its
  # own migration's block once the hub publishes it. It is also the bulk of what
  # a wallet asks for, so a shim that serves lookups but stalls the sync stream
  # is unusable in practice.
  grpc_call GetBlockRange "$_req" "$SMOKE_LOOKUP_HARD_SECS"
  _measured="$CODE, $(grpc_status_text "$HDRS"), $BYTES bytes for blocks $SMOKE_BLOCK_START-$_end, ${SECS}s"
  if [ "$CURL_RC" != 0 ]; then
    fail shim:blockrange "no reply within ${SMOKE_LOOKUP_HARD_SECS}s: ${CURL_ERR:-curl exit $CURL_RC}"
  elif [ "$CODE" != 200 ] || grpc_status_bad "$HDRS"; then
    fail shim:blockrange "$_measured"
  elif [ "$BYTES" -lt "$BLOCKRANGE_MIN_BYTES" ]; then
    fail shim:blockrange "$_measured — under the $BLOCKRANGE_MIN_BYTES-byte floor, so the stream opened and delivered nothing"
  else
    pass shim:blockrange "$_measured"
  fi
}

# ------------------------------------------------------------------- run it

if [ -n "$HUB" ]; then
  printf 'hub  %s\n' "$HUB"
  if [ "$CLEARNET" = 1 ]; then
    check_hub_clearnet_status
    check_hub_submit_open
  else
    check_hub_nym_status
    check_hub_nym_address
    check_hub_submit_closed
  fi
  printf '\n'
fi

if [ -n "$SHIM" ]; then
  printf 'shim %s\n' "$SHIM"
  if [ "$CLEARNET" = 1 ]; then check_shim_clearnet_status; else check_shim_nym_status; fi
  check_shim_healthz
  check_shim_grpc_passthrough
  check_shim_lookup
  check_shim_divert
  check_shim_blockrange
  printf '\n'
fi

TOTAL=$((PASSED + FAILED))
if [ "$FAILED" -eq 0 ]; then
  rm -rf "$RUN_DIR"
  printf '%d/%d checks passed. This pair is serving.\n' "$PASSED" "$TOTAL"
  exit 0
fi

printf '%d/%d checks passed, %d FAILED:%s\n' "$PASSED" "$TOTAL" "$FAILED" "$FAILED_NAMES"
printf 'responses kept for inspection in %s\n' "$RUN_DIR"
exit 1

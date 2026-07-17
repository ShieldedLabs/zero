#!/usr/bin/env bash
# Z3 stack smoke probes: liveness + wallet-RPC contract checks against a
# running zebra+zaino+zallet compose stack. This is the single source of
# truth for the probe sequence: .github/workflows/z3-smoke.yml runs it in CI,
# and it runs identically against a locally built stack for fast iteration:
#
#   cd deploy/z3-stack
#   docker compose -f docker-compose.yml -f docker-compose.build.yml build
#   ./smoke.sh --init --up        # first run (creates wallet encryption)
#   ./smoke.sh                    # subsequent iterations: probes only, ~2 min
#
# The stack needs no synced chain: every probe passes against a fresh zebra
# that is still downloading blocks. Probes carry PER-CALL timeouts on top of
# retry deadlines: a hanging RPC (the production z_listunspent hang was this
# class) becomes a named failed probe within seconds instead of an
# unattributed job timeout.
#
# Flags:
#   --init   one-time offline wallet init (age identity + encryption + mnemonic)
#   --up     docker compose up -d before probing
# Environment:
#   COMPOSE           compose invocation (default "docker compose")
#   PROTO_DIR         walletrpc proto dir (default: derived from repo layout)
#   ZEBRA_HEALTH_DEADLINE / PROBE_DEADLINE   seconds (defaults 300 / 180)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPOSE="${COMPOSE:-docker compose}"
PROTO_DIR="${PROTO_DIR:-$REPO_ROOT/zaino/packages/zaino-proto/lightwallet-protocol/walletrpc}"
ZEBRA_HEALTH_DEADLINE="${ZEBRA_HEALTH_DEADLINE:-300}"
PROBE_DEADLINE="${PROBE_DEADLINE:-180}"

INIT=0
UP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --init) INIT=1 ;;
    --up) UP=1 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

cd "$SCRIPT_DIR"

FAILURES=0
PASSES=0
pass() { PASSES=$((PASSES + 1)); printf 'ok:   %s\n' "$*"; }
fail() { FAILURES=$((FAILURES + 1)); printf 'FAIL: %s\n' "$*" >&2; }

# probe_until <seconds> <command...>: poll every 5s until the command
# succeeds or the deadline passes. Returns the command's final verdict, so
# `if probe_until ...` cannot be fooled by the loop's own exit status.
probe_until() {
  local budget="$1"; shift
  local deadline=$((SECONDS + budget))
  while true; do
    if "$@" > /dev/null 2>&1; then return 0; fi
    if [ "$SECONDS" -ge "$deadline" ]; then return 1; fi
    sleep 5
  done
}

# Plain grep (not -q): -q closes the pipe on match, and under pipefail the
# resulting SIGPIPE in the producer would read as failure.
contains() { printf '%s\n' "$1" | grep -- "$2" > /dev/null; }

zallet_rpc() {
  timeout 20 $COMPOSE exec -T zallet zallet \
    --datadir /var/lib/zallet --config /etc/zallet/zallet.toml rpc "$@"
}

dump_state() {
  echo "=== compose ps ==="
  $COMPOSE ps || true
  # Full logs per container, not a tail: a crash-looping container keeps
  # every restart's output in one log, and the interesting part (the first
  # crash) is at the top.
  for c in zebra zaino zallet; do
    echo "=== $c (RestartCount=$(docker inspect --format '{{.RestartCount}}' "$c" 2>/dev/null || echo '?')) ==="
    docker logs "$c" 2>&1 || true
  done
}

if [ "$INIT" = 1 ]; then
  echo "--- one-time wallet init (offline)"
  if [ ! -f encryption-identity.txt ]; then
    docker run --rm -v "$PWD:/out" alpine:3.20 \
      sh -c 'apk add --no-cache age >/dev/null && age-keygen -o /out/encryption-identity.txt'
    # The zallet container runs as uid 1000 and refuses group/world-readable
    # identities. sudo covers the CI runner; local Docker Desktop bind
    # mounts do not preserve host ownership, so plain chmod suffices there.
    if command -v sudo > /dev/null 2>&1 && sudo -n true 2>/dev/null; then
      sudo chown 1000:1000 encryption-identity.txt || true
      sudo chmod 400 encryption-identity.txt
    else
      chmod 400 encryption-identity.txt
    fi
  fi
  # Deliberately before zebra exists: init must work offline.
  $COMPOSE run --rm --no-deps zallet \
    --datadir /var/lib/zallet --config /etc/zallet/zallet.toml init-wallet-encryption
  $COMPOSE run --rm --no-deps zallet \
    --datadir /var/lib/zallet --config /etc/zallet/zallet.toml generate-mnemonic
fi

if [ "$UP" = 1 ]; then
  $COMPOSE up -d
fi

# --- probe: zebra healthy -----------------------------------------------
# The healthcheck passes when RPC answers, long before the chain is synced;
# it gates zaino/zallet startup ordering.
zebra_healthy() { [ "$(docker inspect --format '{{.State.Health.Status}}' zebra 2>/dev/null)" = healthy ]; }
if probe_until "$ZEBRA_HEALTH_DEADLINE" zebra_healthy; then
  pass "zebra healthy"
else
  fail "zebra never became healthy"
fi

# The zebra network name differs between CI (z3-stack_default) and local
# checkouts (directory-name prefixed); derive it from the running container.
STACK_NETWORK="$(docker inspect zebra --format '{{range $k, $v := .NetworkSettings.Networks}}{{$k}}{{end}}' 2>/dev/null | head -1)"

# --- probe: zaino serves lightwalletd gRPC -------------------------------
zaino_grpc_answers() {
  timeout 20 docker run --rm --network "$STACK_NETWORK" \
    -v "$PROTO_DIR:/protos:ro" \
    fullstorydev/grpcurl -plaintext -max-time 15 -import-path /protos -proto service.proto \
    zaino:8137 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo \
    | grep chainName
}
if [ -n "$STACK_NETWORK" ] && probe_until "$PROBE_DEADLINE" zaino_grpc_answers; then
  pass "zaino serves lightwalletd gRPC"
else
  fail "zaino gRPC never answered"
fi

# --- probe: zallet answers wallet RPC with a node tip ---------------------
zallet_has_node_tip() { zallet_rpc getwalletstatus | grep node_tip; }
if probe_until "$PROBE_DEADLINE" zallet_has_node_tip; then
  pass "zallet reports node_tip"
else
  fail "zallet RPC never reported node_tip"
fi

# --- probe: z_importaddress is compiled in (transparent-key-import) -------
# Invalid hex with a parseable account UUID deterministically reaches the
# feature-gated parser, which must answer "Invalid hex encoding": positive
# proof the real implementation is present, with no wallet state involved.
# (`zallet rpc` parses each param as JSON, hence the quoted strings.)
out=$(zallet_rpc z_importaddress '"00000000-0000-0000-0000-000000000000"' '"zz"' 2>&1) || true
if contains "$out" "Invalid hex encoding"; then
  pass "z_importaddress feature is compiled in"
else
  fail "z_importaddress guard: expected 'Invalid hex encoding', got: $out"
fi

# --- probe: z_listunspent honors its contract -----------------------------
# Pins the wallet-RPC behavior classes that shipped as production incidents:
# a real watch-only import succeeds, filtered and unfiltered listings answer
# promptly with well-formed results (the v11 dust sweep hung ~10 minutes on a
# filtered call), and a bad filter address is a clean error. Fresh wallet,
# unsynced chain: the contract under test is shape and latency, not content.
# Public test constant (regtest/testnet-derived pubkey; the mainnet encoding
# of its address is derived by the wallet on import).
pubkey="0220f133a0751f6a70ce2dc506da68891b827296a0b13fb7883ceea25f7926f5d5"
account=""
fetch_account() {
  account=$(zallet_rpc z_getnewaccount '"smoke"' 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["account_uuid"])' 2>/dev/null) \
    && [ -n "$account" ]
}
probe_until 120 fetch_account || true
if [ -n "$account" ]; then
  pass "z_getnewaccount succeeded"
  imported=$(zallet_rpc z_importaddress "\"$account\"" "\"$pubkey\"" false) || imported=""
  address=$(printf '%s' "$imported" | python3 -c 'import json,sys; print(json.load(sys.stdin)["address"])' 2>/dev/null) || address=""
  if [ -n "$address" ]; then
    pass "watch-only import returned an address"
  else
    fail "watch-only import failed: $imported"
  fi
  if zallet_rpc z_listunspent | python3 -c 'import json,sys; assert isinstance(json.load(sys.stdin), list)' 2>/dev/null; then
    pass "unfiltered z_listunspent is a prompt JSON array"
  else
    fail "unfiltered z_listunspent malformed or slow"
  fi
  if [ -n "$address" ] && zallet_rpc z_listunspent 1 9999999 true "[\"$address\"]" \
      | python3 -c 'import json,sys; assert json.load(sys.stdin) == []' 2>/dev/null; then
    pass "filtered z_listunspent answers promptly and empty"
  else
    fail "filtered z_listunspent malformed or slow"
  fi
  out=$(zallet_rpc z_listunspent 1 9999999 true '["not-an-address"]' 2>&1) || true
  if contains "$out" "Not a valid Zcash address"; then
    pass "invalid filter address is a clean error"
  else
    fail "invalid filter address: expected clean error, got: $out"
  fi
else
  fail "z_getnewaccount never succeeded"
fi

# --- probe: no crash-loops, no accidental exposure ------------------------
for c in zaino zallet; do
  rc="$(docker inspect --format '{{.RestartCount}}' "$c" 2>/dev/null || echo '?')"
  if [ "$rc" = "0" ]; then pass "$c has no restarts"; else fail "$c RestartCount=$rc"; fi
done
if docker port zebra | grep 8233 > /dev/null; then
  pass "zebra publishes its P2P port"
else
  fail "zebra P2P port not published"
fi
if [ -z "$(docker port zaino)" ] && [ -z "$(docker port zallet)" ]; then
  pass "zaino and zallet expose nothing to the host"
else
  fail "unexpected published ports: zaino='$(docker port zaino)' zallet='$(docker port zallet)'"
fi

echo "=== smoke: $PASSES passed, $FAILURES failed ==="
if [ "$FAILURES" -gt 0 ]; then
  dump_state
  exit 1
fi

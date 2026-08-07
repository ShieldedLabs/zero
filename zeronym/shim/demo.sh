#!/usr/bin/env bash
#
# Zeronym zero-indexer-shim demo.
#
#   ./demo.sh                 offline demo. Needs nothing but cargo. Always works.
#   ./demo.sh HOST:PORT       live demo in front of a real lightwalletd or Zaino.
#
# The offline demo stands up a stub indexer, puts a real shim in front of it,
# and sends eight calls through, so you see the classifier's verdicts and the
# proxy's log lines without a node, a chain, or grpcurl. It then runs the test
# suite, which is where the transparency properties are actually asserted.
#
# The live demo points the shim at an indexer you already run and drives it with
# grpcurl. It falls back to the offline demo if grpcurl is missing or the
# backing indexer is not reachable.
#
# This proof of concept is NON-DESTRUCTIVE. It classifies and logs; it never
# diverts. See README.md.

set -euo pipefail

SHIM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROTO_DIR="$(cd "$SHIM_DIR/../../zaino/packages/zaino-proto/proto" && pwd)"
MIGRATION_FIXTURE="$SHIM_DIR/tests/fixtures/v6_migration.bin"

LISTEN="${ZIS_LISTEN:-127.0.0.1:19068}"
BACKEND="${1:-${ZIS_BACKEND:-}}"

cd "$SHIM_DIR"

rule() { printf '\n\033[1m== %s\033[0m\n\n' "$1"; }
note() { printf '   %s\n' "$1"; }

# ---------------------------------------------------------------- offline

run_offline() {
    rule "1/2  The shim, end to end, with a stub indexer behind it"
    note "cargo run --example shim_demo"
    note "Eight calls. The predicate is the presence of Orchard actions, so the"
    note "first three are all MIGRATION: Orchard into Ironwood, Orchard with NO"
    note "Ironwood bundle, and Orchard netting to exactly zero. That last one is"
    note "the case the old exit predicate passed through in the clear: watch for"
    note "MIGRATION on a line reading orchard_vb=+0. Then the boundary in the"
    note "other direction, an Ironwood-only tx (passthrough on a line reading"
    note "orchard_actions=0, because Ironwood is the new pool where ordinary"
    note "time-sensitive commerce lives), a real transparent tx, garbage, a"
    note "compressed body, and one ordinary proxied method. Watch the"
    note "zis::classify verdicts. All eight are forwarded: this PoC does not"
    note "divert."
    echo
    cargo run --quiet --example shim_demo

    rule "2/2  The assertions"
    note "cargo test -- --nocapture"
    echo
    cargo test -- --nocapture

    rule "Done"
    note "Transparency lives in tests/grpc_transparency.rs (real tonic server and"
    note "client, every call made twice and compared) and tests/proxy_transparency.rs"
    note "(raw HTTP/2: byte-exact frames, trailers, streaming under a gate)."
    note "The classifier's verdicts are asserted in tests/classify_logging.rs."
}

# ------------------------------------------------------------------- live

reachable() {
    local host="${1%:*}" port="${1##*:}"
    (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null && exec 3>&- && return 0
    return 1
}

grpc_call() {
    local target="$1" method="$2" data="$3"
    grpcurl -plaintext -max-time 20 \
        -import-path "$PROTO_DIR" -proto service.proto \
        -d "$data" "$target" "cash.z.wallet.sdk.rpc.CompactTxStreamer/$method" \
        2>&1 | head -20 || true
}

run_live() {
    rule "Live demo: shim on $LISTEN in front of $BACKEND"

    local log build
    log="$(mktemp -t zero-indexer-shim-demo)"
    build="$(mktemp -t zero-indexer-shim-build)"
    if ! cargo build --quiet >"$build" 2>&1; then
        cat "$build"
        rm -f "$build" "$log"
        exit 1
    fi
    rm -f "$build"

    # zis::proxy=debug turns on the per-request line, which the binary keeps
    # below its default level on purpose: it is an access log on the operator's
    # box. The demo wants to show it.
    RUST_LOG="${RUST_LOG:-zis::proxy=debug,info}" ./target/debug/zero-indexer-shim \
        --listen "$LISTEN" --backend "$BACKEND" >"$log" 2>&1 &
    local pid=$!
    # shellcheck disable=SC2064
    trap "kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -f '$log'" EXIT

    for _ in $(seq 1 50); do
        reachable "$LISTEN" && break
        sleep 0.1
    done
    if ! kill -0 "$pid" 2>/dev/null; then
        note "the shim exited immediately. Its output:"
        cat "$log"
        exit 1
    fi

    rule "1/3  A pass-through method (GetLightdInfo), through the shim"
    note "The shim never decodes this. It relays the request, the response, and"
    note "the grpc-status trailer verbatim."
    echo
    grpc_call "$LISTEN" GetLightdInfo '{}'

    rule "2/3  SendTransaction carrying a real V6 that carries Orchard actions"
    note "This is the intercepted path. The shim decodes the body, classifies it,"
    note "logs MIGRATION, and then forwards it anyway (non-destructive PoC)."
    note "The fixture is a synthetic transaction, so your node will reject it."
    note "The rejection is expected: the log line above the rejection is the point."
    echo
    local b64
    b64="$(base64 <"$MIGRATION_FIXTURE" | tr -d '\n')"
    grpc_call "$LISTEN" SendTransaction "{\"data\": \"$b64\"}"

    rule "3/3  What the shim logged"
    echo
    cat "$log"

    rule "Done"
    note "Compare against the same calls made directly at $BACKEND: the client"
    note "cannot tell the difference. That is the property the tests assert."
}

# ------------------------------------------------------------------- main

if [ -z "$BACKEND" ]; then
    note "No backing indexer given, running the offline demo."
    note "For the live demo: ./demo.sh HOST:PORT   (a lightwalletd or Zaino gRPC port)"
    run_offline
    exit 0
fi

if ! command -v grpcurl >/dev/null 2>&1; then
    note "grpcurl is not installed, so the live demo cannot drive the shim."
    note "Install it (brew install grpcurl) or run the offline demo instead."
    note "Falling back to the offline demo."
    run_offline
    exit 0
fi

if ! reachable "$BACKEND"; then
    note "Cannot reach $BACKEND. Is your lightwalletd or Zaino listening there?"
    note "Falling back to the offline demo."
    run_offline
    exit 0
fi

run_live

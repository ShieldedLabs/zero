#!/bin/sh
# smoke-local.sh -- run smoke.sh against a LOCAL shim and LOCAL hub on the REAL
# public Nym mixnet, with no deploy.
#
# Why this exists. Every unit and integration test in shim/ and hub/ is
# deterministic and mock-based; none of them exercises the real nym-sdk against
# the real network. That gap is where the two most expensive misdiagnoses of
# 2026-08-14..17 lived: "the mixnet is slow" (it was a dead deployed hub) and
# "enclave shims are 10x slower" (it was a degraded deployed hub). Both were
# settled in minutes by exactly this: a shim and a hub started locally, talking
# over the public mixnet, timed. Deploying to find the same answer costs ~25 min
# per component plus a CNAME. So this is the cheap real-mixnet regression check,
# and smoke.sh's timed lookup (SMOKE_LOOKUP_MAX_SECS) is what makes it a test
# rather than a demo: a local pair should answer a lookup in single-digit
# seconds, and if it does not, something upstream of any enclave has changed.
#
# What it needs: release builds of both binaries with the mixnet driver, and
# outbound internet (the clients register with a real gateway). Set the paths
# with SHIM_BIN / HUB_BIN, or it looks in each crate's target/release.
#
#   cargo build --release --features mixnet-driver     # in shim/ and in hub/
#   sh zeronym/smoke-local.sh
#
# The hub's mixnet client takes 30-90 s to register with a gateway; the shim's
# likewise. The script polls /nym-status on both and only runs smoke once each
# reports mixnet_connected true, so a slow gateway costs time, not a false FAIL.
# On any exit both processes are killed; nothing is left listening.
#
# Ports are high and fixed so a second copy does not collide with a first; set
# HUB_PORT / SHIM_PORT to move them.

set -u

here=$(cd "$(dirname "$0")" && pwd)
SHIM_BIN=${SHIM_BIN:-$here/shim/target/release/zero-indexer-shim}
HUB_BIN=${HUB_BIN:-$here/hub/target/release/zero-indexer-hub}
HUB_PORT=${HUB_PORT:-19970}
SHIM_PORT=${SHIM_PORT:-19971}
INDEXER=${INDEXER:-66.241.124.200:443}
INDEXER_TLS=${INDEXER_TLS:-na.zec.rocks}
CONNECT_WAIT_SECS=${CONNECT_WAIT_SECS:-200}
LOG_DIR=${LOG_DIR:-${TMPDIR:-/tmp}/smoke-local.$$}

for bin in "$SHIM_BIN" "$HUB_BIN"; do
    if [ ! -x "$bin" ]; then
        echo "smoke-local: missing binary: $bin" >&2
        echo "  build it: cargo build --release --features mixnet-driver (in shim/ and hub/)" >&2
        exit 2
    fi
done
mkdir -p "$LOG_DIR"

hub_pid=""
shim_pid=""
cleanup() {
    # Kill both, hub last: the shim's client is registered with a gateway and a
    # clean stop lets it deregister rather than leaving a stale registration the
    # next run's random identity does not care about anyway. Best effort.
    [ -n "$shim_pid" ] && kill "$shim_pid" 2>/dev/null
    [ -n "$hub_pid" ] && kill "$hub_pid" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT INT TERM

# Wait until URL/nym-status reports mixnet_connected true, or give up. Prints the
# seconds it took, because that number is itself diagnostic: a client that takes
# three minutes to register is telling you something about the gateway it drew.
wait_connected() {
    _url=$1; _what=$2; _t=0
    while [ "$_t" -lt "$CONNECT_WAIT_SECS" ]; do
        case $(curl -sS -m 5 ${3:-} "$_url/nym-status" 2>/dev/null) in
            *'"mixnet_connected":true'*)
                echo "smoke-local: $_what connected after ${_t}s"
                return 0 ;;
        esac
        sleep 5; _t=$((_t + 5))
    done
    echo "smoke-local: $_what did not connect within ${CONNECT_WAIT_SECS}s; log: $LOG_DIR/$_what.log" >&2
    tail -5 "$LOG_DIR/$_what.log" >&2
    return 1
}

echo "smoke-local: starting hub on 127.0.0.1:$HUB_PORT (logs in $LOG_DIR)"
ZIH_LISTEN=127.0.0.1:$HUB_PORT ZIH_INDEXERS=$INDEXER ZIH_INDEXER_TLS=$INDEXER_TLS ZIH_NYM=true \
RUST_LOG=${RUST_LOG:-warn} "$HUB_BIN" >"$LOG_DIR/hub.log" 2>&1 &
hub_pid=$!
wait_connected "http://127.0.0.1:$HUB_PORT" hub || exit 1

hub_addr=$(curl -sS -m 10 "http://127.0.0.1:$HUB_PORT/nym-address" 2>/dev/null)
case $hub_addr in
    *.*@*) ;;
    *) echo "smoke-local: hub published no usable address: '$hub_addr'" >&2; exit 1 ;;
esac
echo "smoke-local: hub address ...@${hub_addr##*@}"

echo "smoke-local: starting shim on 127.0.0.1:$SHIM_PORT -> hub"
ZIS_LISTEN=127.0.0.1:$SHIM_PORT ZIS_BACKEND=$INDEXER ZIS_BACKEND_TLS=$INDEXER_TLS \
ZIS_HUB_NYM=$hub_addr ZIS_LOOKUP_TIMEOUT_SECS=${ZIS_LOOKUP_TIMEOUT_SECS:-90} \
RUST_LOG=${RUST_LOG:-warn} "$SHIM_BIN" >"$LOG_DIR/shim.log" 2>&1 &
shim_pid=$!
# The shim is h2c on loopback, so its status endpoint needs prior-knowledge h2.
wait_connected "http://127.0.0.1:$SHIM_PORT" shim --http2-prior-knowledge || exit 1

echo "smoke-local: both connected; running smoke.sh"
echo "----------------------------------------------------------------------"
# smoke.sh already switches to --http2-prior-knowledge for an http:// shim, and
# to plain http/1.1 for the hub, so no flags are needed here beyond the URLs.
sh "$here/smoke.sh" --shim "http://127.0.0.1:$SHIM_PORT" --hub "http://127.0.0.1:$HUB_PORT"
rc=$?
echo "----------------------------------------------------------------------"
if [ "$rc" -eq 0 ]; then
    echo "smoke-local: PASS (logs removed: $LOG_DIR)"
    rm -rf "$LOG_DIR"
else
    echo "smoke-local: FAIL rc=$rc; logs kept in $LOG_DIR" >&2
    echo "  bandwidth warnings -- shim: $(grep -c 'Not enough bandwidth' "$LOG_DIR/shim.log") hub: $(grep -c 'Not enough bandwidth' "$LOG_DIR/hub.log")" >&2
    echo "  duplicate fragments -- shim: $(grep -c 'duplicate fragment' "$LOG_DIR/shim.log") hub: $(grep -c 'duplicate fragment' "$LOG_DIR/hub.log")" >&2
fi
exit "$rc"

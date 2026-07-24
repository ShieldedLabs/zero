#!/bin/sh
# Reference supervisor for the co-located enclave: Caution runs exactly one
# unit per enclave, so a single PID-1 process must launch both zebrad and
# zainod. This is the integration seam between Anton's team's zebra packaging
# and our zaino component; adapt paths to the combined image.
#
# Order: start zebrad, wait until its RPC answers (opening the ~276 GB tmpfs
# state can take minutes), then start zainod against localhost. If either
# process exits, tear down the other and exit non-zero so the enclave restarts
# the unit as a whole. Assumes a busybox-class runtime (sh + wget).

set -eu

ZEBRA_BIN=${ZEBRA_BIN:-/zebrad}
ZEBRA_CONF=${ZEBRA_CONF:-/etc/zebra/zebrad.toml}
ZAINO_BIN=${ZAINO_BIN:-/zainod}
ZAINO_CONF=${ZAINO_CONF:-/etc/zaino/zainod-colocated.toml}
ZEBRA_RPC=${ZEBRA_RPC:-http://127.0.0.1:8232/}
RPC_WAIT_TRIES=${RPC_WAIT_TRIES:-900}   # x2s = up to 30 min for state open + tip

zebra_pid=""
zaino_pid=""

shutdown() {
  echo "supervisor: signalling children"
  [ -n "$zebra_pid" ] && kill -TERM "$zebra_pid" 2>/dev/null || true
  [ -n "$zaino_pid" ] && kill -TERM "$zaino_pid" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap shutdown TERM INT

echo "supervisor: starting zebrad"
"$ZEBRA_BIN" start --config "$ZEBRA_CONF" &
zebra_pid=$!

echo "supervisor: waiting for zebra RPC at $ZEBRA_RPC"
i=0
until wget -q -O /dev/null \
    --header='Content-Type: application/json' \
    --post-data='{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
    "$ZEBRA_RPC" 2>/dev/null; do
  i=$((i + 1))
  if ! kill -0 "$zebra_pid" 2>/dev/null; then
    echo "supervisor: zebrad exited during startup"
    exit 1
  fi
  if [ "$i" -ge "$RPC_WAIT_TRIES" ]; then
    echo "supervisor: zebra RPC not up in time"
    shutdown
    exit 1
  fi
  sleep 2
done

echo "supervisor: zebra RPC up, starting zainod"
"$ZAINO_BIN" start --config "$ZAINO_CONF" &
zaino_pid=$!

# Exit as soon as either child dies (portable poll; busybox ash lacks wait -n).
while kill -0 "$zebra_pid" 2>/dev/null && kill -0 "$zaino_pid" 2>/dev/null; do
  sleep 5
done

echo "supervisor: a child exited, tearing down"
shutdown
exit 1

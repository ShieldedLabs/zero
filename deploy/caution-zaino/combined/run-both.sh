#!/bin/sh
# Reference supervisor for the co-located enclave: Caution runs exactly one
# unit per enclave, so a single PID-1 process must launch both zebrad and
# zainod.
#
# Order: start zebrad, wait until its RPC answers, then start zainod against
# localhost. If either process exits, tear down the other and exit non-zero so
# the enclave restarts the unit. Assumes a busybox-class runtime (sh + wget).
#
# IMPORTANT: zebrad and zainod both read `ZEBRA_*` / `ZAINO_*`-prefixed
# environment variables as CONFIGURATION. So this supervisor takes ALL of its
# own settings from `SV_*` variables (never `ZEBRA_*` / `ZAINO_*`), and scrubs
# the colliding names from the environment before exec. Otherwise a variable
# meant for the supervisor leaks into the child and aborts it: e.g. `ZEBRA_CONF`
# is parsed by zebrad as an unknown config field `conf` and it exits, which
# panics the enclave (PID 1 died) into a reboot loop.

set -eu

zebra_bin=${SV_ZEBRA_BIN:-/usr/local/bin/zebrad}
zebra_conf=${SV_ZEBRA_CONF:-/etc/zebra/zebrad.toml}
zaino_bin=${SV_ZAINO_BIN:-/usr/local/bin/zainod}
zaino_conf=${SV_ZAINO_CONF:-/etc/zaino/zainod-colocated.toml}
zebra_rpc=${SV_ZEBRA_RPC:-http://127.0.0.1:8232/}
rpc_wait_tries=${SV_RPC_WAIT_TRIES:-900}   # x2s = up to 30 min for state open
zebra_grace=${SV_ZEBRA_GRACE:-60}

# Scrub the config-path vars the binaries would misread. Targeted (not all
# ZEBRA_*/ZAINO_*) so genuine env-config overrides still reach the binaries.
unset ZEBRA_CONF ZAINO_CONF ZEBRA_BIN ZAINO_BIN ZEBRA_RPC 2>/dev/null || true

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
# zebrad's config flag is GLOBAL and must precede the subcommand:
# `zebrad -c <file> start`. `zebrad start --config <file>` errors with
# "unexpected argument '--config'". (zainod uses the `start --config <file>`
# form below: the two CLIs differ.)
"$zebra_bin" -c "$zebra_conf" start &
zebra_pid=$!

# Gate on zebra RPC readiness. Prefer a real RPC probe via wget; if the runtime
# lacks wget, fall back to a fixed grace period (zainod also retries the
# validator connection on its own, per the [zero] startup-hardening carries).
if command -v wget >/dev/null 2>&1; then
  echo "supervisor: waiting for zebra RPC at $zebra_rpc"
  i=0
  until wget -q -O /dev/null \
      --header='Content-Type: application/json' \
      --post-data='{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
      "$zebra_rpc" 2>/dev/null; do
    i=$((i + 1))
    if ! kill -0 "$zebra_pid" 2>/dev/null; then
      echo "supervisor: zebrad exited during startup"
      exit 1
    fi
    if [ "$i" -ge "$rpc_wait_tries" ]; then
      # Do not treat an unconfirmed probe as fatal: busybox wget may lack
      # long-option support, in which case the probe never succeeds. zainod
      # retries the validator on its own, so start it anyway rather than
      # restart-looping the enclave.
      echo "supervisor: zebra RPC not confirmed in time, starting zainod anyway"
      break
    fi
    sleep 2
  done
else
  echo "supervisor: wget absent, waiting ${zebra_grace}s before starting zaino"
  sleep "$zebra_grace"
  if ! kill -0 "$zebra_pid" 2>/dev/null; then
    echo "supervisor: zebrad exited during startup"
    exit 1
  fi
fi

echo "supervisor: starting zainod"
"$zaino_bin" start --config "$zaino_conf" &
zaino_pid=$!

# Exit as soon as either child dies (portable poll; busybox ash lacks wait -n).
while kill -0 "$zebra_pid" 2>/dev/null && kill -0 "$zaino_pid" 2>/dev/null; do
  sleep 5
done

echo "supervisor: a child exited, tearing down"
shutdown
exit 1

#!/usr/bin/env bash
# Run the regtest harness scenarios as three concurrent port-isolated stacks,
# all restored from the same golden chain snapshot (see run.sh). Wall-clock
# becomes the slowest group instead of the sum:
#
#   group A: the six fast wallet-contract scenarios
#   group B: spend-poison (signer + recovery wallet lifecycles)
#   group C: reorg (mutates its stack's chain; isolated here by construction)
#
# Local iteration tool: CI keeps the serial run.sh (2-core runners cannot
# host three stacks). Usage:
#
#   qa/regtest-harness/run-parallel.sh [--build]
#
# Prints each group's output prefixed, then an aggregate verdict.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN="$SCRIPT_DIR/run.sh"
BUILD_FLAG=""
[ "${1:-}" = "--build" ] && BUILD_FLAG="--build"

BASE="${PARALLEL_WORKDIR:-$(mktemp -d "${TMPDIR:-/tmp}/z3-par.XXXXXX")}"
mkdir -p "$BASE"

# Pre-warm the golden chain once (fast if the snapshot already exists); this
# also performs any --build compilation before the groups race.
echo "--- pre-warming golden chain"
HARNESS_WORKDIR="$BASE/prewarm" ZEBRA_RPC_PORT=18262 ZALLET_RPC_PORT=28262 \
  "$RUN" $BUILD_FLAG --setup-only

run_group() {
  local name="$1" scenarios="$2" zport="$3" wport="$4"
  HARNESS_WORKDIR="$BASE/$name" ZEBRA_RPC_PORT="$zport" ZALLET_RPC_PORT="$wport" \
    "$RUN" --only "$scenarios" 2>&1 | sed "s/^/[$name] /"
  return "${PIPESTATUS[0]}"
}

echo "--- launching groups"
pids=()
run_group fast "baseline,dust,filter,union,hang-guard,poison-heal" 18263 28263 &
pids+=($!)
run_group poison "spend-poison" 18264 28264 &
pids+=($!)
run_group reorg "reorg" 18265 28265 &
pids+=($!)

status=0
for pid in "${pids[@]}"; do
  wait "$pid" || status=1
done

if [ "$status" = 0 ]; then
  echo "=== run-parallel: all groups green ==="
else
  echo "=== run-parallel: FAILURES (see group output above) ===" >&2
fi
exit "$status"

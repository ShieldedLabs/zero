#!/bin/sh
# Simulate the divert model and assert every invariant's expected outcome.
#
# "holds" rows are the protocol's safety claims. "fails" rows are the two fixed
# bugs, reproduced with their fix switched off, and the three known gaps: if
# one starts holding, the code or the model changed and divert.qnt needs a
# look.
#
# QUINT defaults to `npx @informalsystems/quint@0.32.0`. QUINT_BACKEND=typescript
# skips the Rust evaluator, which is downloaded from GitHub on first use.
set -u

cd "$(dirname "$0")"
QUINT=${QUINT:-"npx --yes @informalsystems/quint@0.32.0"}
BACKEND=${QUINT_BACKEND:-rust}
SAMPLES=${QUINT_SAMPLES:-20000}
failures=0

check() {
  main=$1 invariant=$2 expect=$3
  if $QUINT run divert.qnt --backend="$BACKEND" --main="$main" --invariant="$invariant" \
      --max-samples="$SAMPLES" --max-steps=30 --seed=7 >/dev/null 2>&1; then
    got=holds
  else
    got=fails
  fi
  if [ "$got" = "$expect" ]; then
    echo "ok    $main.$invariant $got"
  else
    echo "FAIL  $main.$invariant expected $expect, got $got"
    failures=$((failures + 1))
  fi
}

$QUINT typecheck divert.qnt || exit 1

check current        safety          holds
check honest         safety          holds
check honest         pendingIsTrue   holds
check before_ecb4641 pendingVisible  fails
check before_45e408f noQueuedBytes   fails
check current        noSilentRefusal fails
check current        pendingIsTrue   fails
check honest         pendingMonotone fails

exit "$failures"

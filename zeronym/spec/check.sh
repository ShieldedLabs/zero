#!/bin/sh
# Simulate the divert model and assert every invariant's expected outcome.
#
# "holds" rows are the protocol's safety claims: the shim's against any hub,
# the hub's against an honest one. "fails" rows are the two fixed
# bugs, reproduced with their fix switched off, and the known gaps: if
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

# Classified by Quint's verdict line, so a crash, a download failure or a
# misspelt invariant fails the gate instead of passing as a counterexample.
check() {
  main=$1 invariant=$2 expect=$3
  out=$($QUINT run divert.qnt --backend="$BACKEND" --main="$main" --invariant="$invariant" \
      --max-samples="$SAMPLES" --max-steps=30 --seed=7 2>&1)
  case $out in
    *"[ok] No violation found"*) got=holds ;;
    *"[violation] Found an issue"*) got=fails ;;
    *)
      echo "ERROR $main.$invariant: quint gave no verdict"
      echo "$out" | tail -20
      failures=$((failures + 1))
      return
      ;;
  esac
  if [ "$got" = "$expect" ]; then
    echo "ok    $main.$invariant $got"
  else
    echo "FAIL  $main.$invariant expected $expect, got $got"
    failures=$((failures + 1))
  fi
}

$QUINT typecheck divert.qnt || exit 1

check current        shimSafety      holds
check honest         safety          holds
check honest         pendingIsTrue   holds
check before_ecb4641 pendingVisible  fails
check before_45e408f noQueuedBytes   fails
check current        noQueuedBytes   fails
check current        noEarlyBytes    fails
check current        noSilentRefusal fails
check current        pendingIsTrue   fails
check honest         pendingMonotone fails

exit "$failures"

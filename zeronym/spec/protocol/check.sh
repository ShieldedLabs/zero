#!/bin/sh
# Check the zeronym protocol specification and assert every expected outcome.
#
# "Holds" here means bounded random simulation: fixed constants, a fixed step
# bound, a fixed number of traces and one seed. It is not a proof and it is
# not exhaustive to any depth. The exception is tier 4, the hub specification,
# where TLC visits every reachable state of each configuration.
#
# Tiers:
#   1  typecheck every file.
#   2  quint test: the functional layer, the scripted runs, and each
#      configuration's assumptions.
#   3  quint run: invariants. "holds" rows are the guarantees, on the
#      configurations where they are claimed. "fails" rows are the known gaps
#      and the guarantees under the Byzantine component they depend on; such a
#      row shows polarity only, and the scripted run in tier 2 carries the cause.
#   3b quint run: witnesses. Every listed state must be reached in at least
#      one trace. These runs also re-check the configuration's guarantees, on
#      longer traces and under the narrower step relations, which get deeper
#      into the protocol than `step` does.
#   4  tlc.sh: the hub specification, exhaustively. "holds" rows are the
#      schedule guarantees; "violated" rows are the known gaps, the guarantees
#      under a Byzantine hub or indexer, and the states that must be reachable
#      (each as `not(..)`). Needs Java; fails, never skips, without it.
#   5  opt-in, QUINT_TLC=1: the two-state properties, with TLC. Needs Java 21.
#
# A row that starts holding where it is expected to fail, or the reverse,
# means the specification or the prediction changed: read README.md before
# changing the row.
#
# QUINT defaults to `npx @informalsystems/quint@0.33.0`. QUINT_BACKEND=typescript
# skips the Rust evaluator, which is downloaded from GitHub on first use; the
# sample counts and seeds below were settled on the Rust evaluator.
# QUINT_JOBS is how many rows run at once.
set -u

cd "$(dirname "$0")"
QUINT=${QUINT:-"npx --yes @informalsystems/quint@0.33.0"}
BACKEND=${QUINT_BACKEND:-rust}
SAMPLES=${QUINT_SAMPLES:-2000}
JOBS=${QUINT_JOBS:-4}
SEED=7
failures=0

# Rows run JOBS at a time, each writing its result lines to a file of its own;
# `finish` prints them in the order the rows were written and counts the
# failures.
results=$(mktemp -d)
trap 'rm -rf "$results"' EXIT
queued=0
running=0

job() {
  queued=$((queued + 1))
  ("$@") >"$results/$(printf '%04d' "$queued")" 2>&1 &
  running=$((running + 1))
  if [ "$running" -ge "$JOBS" ]; then
    wait
    running=0
  fi
}

finish() {
  wait
  running=0
  for file in "$results"/*; do
    [ -f "$file" ] || continue
    cat "$file"
    failures=$((failures + $(grep -c '^FAIL' "$file")))
    rm -f "$file"
  done
}

SPELLS="spells/basicSpells.qnt spells/soup.qnt"
MODULES="types.qnt wire.qnt indexer.qnt hub.qnt hubMachine.qnt shim.qnt state.qnt properties.qnt protocol.qnt instances.qnt"
FUNCTIONAL="tests/wireTest.qnt tests/indexerTest.qnt tests/hubTest.qnt tests/shimTest.qnt tests/hubScenariosTest.qnt"
INSTANCES="baseline byzShim byzHub byzIndexer replicated replicatedOneByz"
SCENARIOS="baselineScenarios replicatedScenarios byzHubScenarios"
TRUST="byzShimTrust byzHubTrust byzIndexerTrust replicatedOneByzTrust"

fail() {
  echo "FAIL  $1"
}

# run_tests FILE [MODULE]
run_tests() {
  if [ $# -eq 2 ]; then
    out=$($QUINT test "$1" --main="$2" --backend="$BACKEND" 2>&1)
  else
    out=$($QUINT test "$1" --backend="$BACKEND" 2>&1)
  fi
  status=$?
  passing=$(echo "$out" | sed -n 's/^ *\([0-9][0-9]*\) passing.*/\1/p')
  if [ "$status" -eq 0 ] && [ -n "$passing" ]; then
    echo "ok    test ${2:-$1}: $passing passing"
  else
    echo "$out" | tail -25
    fail "test ${2:-$1}"
  fi
}

# simulate MAIN STEP MAX_STEPS ARGS...: one simulation of $samples traces; the
# output is left in $out.
samples=$SAMPLES
simulate() {
  main=$1 step=$2 steps=$3
  shift 3
  out=$($QUINT run instances.qnt --backend="$BACKEND" --main="$main" --step="$step" \
      --max-samples="$samples" --max-steps="$steps" --seed="$SEED" "$@" 2>&1)
}

# Classified by Quint's verdict line, so a crash, a download failure or a
# misspelt name fails the gate instead of passing as a counterexample.
verdict() {
  case $out in
    *"[ok] No violation found"*) got=holds ;;
    *"[violation] Found an issue"*) got=fails ;;
    *) got=none ;;
  esac
}

# holds MAIN INVARIANT...: all of them, together, under `step`.
holds() {
  main=$1
  shift
  simulate "$main" step 40 --invariants "$@"
  verdict
  case $got in
    holds) echo "ok    $main: holds: $*" ;;
    fails)
      echo "$out" | grep -a -E '^ *❌' | sed 's/^/      /'
      fail "$main: expected to hold, violated (among: $*)"
      ;;
    *)
      echo "$out" | tail -20
      fail "$main: quint gave no verdict"
      ;;
  esac
}

# fails MAIN STEP MAX_STEPS INVARIANT [TRACES]: violated in some trace of STEP.
fails() {
  samples=${5:-$SAMPLES}
  simulate "$1" "$2" "$3" --invariant="$4"
  verdict
  case $got in
    fails) echo "ok    $1: fails: $4 ($2, $3 steps)" ;;
    holds) fail "$1: $4 expected to fail under $2, no violation in $samples traces" ;;
    *)
      echo "$out" | tail -20
      fail "$1: $4: quint gave no verdict"
      ;;
  esac
}

# The names after `--` in "$@", and the names before it.
after_dashes() {
  seen=0
  for arg in "$@"; do
    if [ "$seen" -eq 1 ]; then printf '%s ' "$arg"; fi
    if [ "$arg" = "--" ]; then seen=1; fi
  done
}

before_dashes() {
  for arg in "$@"; do
    if [ "$arg" = "--" ]; then break; fi
    printf '%s ' "$arg"
  done
}

# reaches MAIN STEP MAX_STEPS WITNESS... -- INVARIANT...: every witness is
# reached in at least one trace of STEP, and no invariant is violated on the
# way.
reaches() {
  main=$1 step=$2 steps=$3
  shift 3
  witnesses=$(before_dashes "$@")
  invariants=$(after_dashes "$@")
  # shellcheck disable=SC2086
  simulate "$main" "$step" "$steps" --witnesses $witnesses --invariants $invariants
  verdict
  case $got in
    holds) ;;
    fails)
      echo "$out" | grep -a -E '^ *❌' | sed 's/^/      /'
      fail "$main: an invariant was violated under $step (among: $invariants)"
      return
      ;;
    *)
      echo "$out" | tail -20
      fail "$main: witnesses under $step: quint gave no verdict"
      return
      ;;
  esac
  for witness in $witnesses; do
    count=$(echo "$out" | sed -n "s/^$witness was witnessed in \([0-9][0-9]*\) trace.*/\1/p")
    if [ -z "$count" ]; then
      fail "$main: witness $witness: no count reported"
    elif [ "$count" -eq 0 ]; then
      fail "$main: witness $witness never reached under $step in $samples traces"
    else
      echo "ok    $main: reached: $witness ($count of $samples, $step, $steps steps)"
    fi
  done
}

# tlc_holds INIT STEP INVARIANT: TLC exhausts the configuration and finds
# every reachable state satisfies the invariant.
tlc_holds() {
  out=$(QUINT=$QUINT sh ./tlc.sh hubMachine.qnt hubMachine "$1" "$2" "$3" 2>&1)
  case $out in
    "holds "*) echo "ok    $1: holds, exhaustively: $3 ($2; states and depth: ${out#holds })" ;;
    "violated "*) fail "$1: $3 expected to hold under $2, TLC found a counterexample of ${out#violated } states" ;;
    *)
      echo "$out" | tail -20
      fail "$1: $3: tlc.sh gave no verdict"
      ;;
  esac
}

# tlc_violated INIT STEP INVARIANT LENGTH: TLC finds a counterexample, and it
# is no longer than the recorded one. With one worker TLC searches breadth
# first and its counterexample is a shortest one, so a longer one means the
# recorded counterexample is gone.
tlc_violated() {
  out=$(QUINT=$QUINT TLC_WORKERS=1 sh ./tlc.sh hubMachine.qnt hubMachine "$1" "$2" "$3" 2>&1)
  case $out in
    "violated "*)
      length=${out#violated }
      if [ "$length" -le "$4" ]; then
        echo "ok    $1: violated: $3 ($2, $length states)"
      else
        fail "$1: $3 violated under $2 in $length states, recorded as $4"
      fi
      ;;
    "holds "*) fail "$1: $3 expected to be violated under $2, TLC found it holds" ;;
    *)
      echo "$out" | tail -20
      fail "$1: $3: tlc.sh gave no verdict"
      ;;
  esac
}

typecheck() {
  if out=$($QUINT typecheck "$1" 2>&1); then
    echo "ok    typecheck $1"
  else
    echo "$out" | tail -20
    fail "typecheck $1"
  fi
}

echo "---- 1 typecheck"
for file in $SPELLS $MODULES $FUNCTIONAL tests/scenariosTest.qnt tests/trustTest.qnt; do
  job typecheck "$file"
done
finish
if [ "$failures" -ne 0 ]; then
  exit "$failures"
fi

echo "---- 2 tests"
for file in $SPELLS $FUNCTIONAL; do
  job run_tests "$file"
done
for module in $SCENARIOS; do
  job run_tests tests/scenariosTest.qnt "$module"
done
for module in $TRUST; do
  job run_tests tests/trustTest.qnt "$module"
done
for module in $INSTANCES; do
  job run_tests instances.qnt "$module"
done
finish

echo "---- 3 invariants ($SAMPLES traces, seed $SEED)"

# The guarantees, where they are claimed. G7 `wellFormed` is checked everywhere.
job holds baseline            operatorBlind queuedBytesConfidential txidAuthenticity lookupValidityPerHub \
                              ackImpliesQueued wellFormed
job holds byzShim             ackImpliesQueued wellFormed
job holds byzHub              operatorBlind txidAuthenticity wellFormed
job holds byzIndexer          operatorBlind txidAuthenticity ackImpliesQueued wellFormed
job holds replicated          lookupValidityPerHub wellFormed
job holds replicatedOneByz    txidAuthenticity ackImpliesQueuedForHonestHubs wellFormed

# The trust matrix: each guarantee fails once the component it depends on is
# Byzantine. The schedule guarantees' rows are in tier 4.
job fails byzShim             step      40 operatorBlind
job fails byzShim             step      40 queuedBytesConfidential
job fails byzShim             step      40 txidAuthenticity
job fails byzShim             step      40 lookupValidityPerHub
job fails byzHub              step      40 queuedBytesConfidential
job fails byzHub              step      40 lookupValidityPerHub
job fails byzHub              step      40 ackImpliesQueued
job fails byzIndexer          step      40 queuedBytesConfidential
job fails byzIndexer          step      40 lookupValidityPerHub
job fails replicatedOneByz    step      40 queuedBytesConfidential
job fails replicatedOneByz    step      40 lookupValidityPerHub

# The known gaps, with every component honest.
job fails baseline            quietStep 40 statusNeverRegresses                    # K2
job fails replicated          quietStep 40 statusNeverRegresses                    # K2

finish

echo "---- 3b witnesses ($SAMPLES traces, seed $SEED)"

BASELINE_HOLDS="operatorBlind queuedBytesConfidential txidAuthenticity lookupValidityPerHub ackImpliesQueued wellFormed"

# W4 (four of the five refusals), W8, W17, K1a, K1b, and the antecedents of
# G1, G2, G8.
job reaches baseline step 40 \
  wRefusedTipStale wRefusedDraining wRefusedTooLarge wRefusedExpiryTooTight \
  wQueuedDisclosed wThirdPartyPayloadQueued wToldRefusedEverywhere wToldNeverDelivered \
  vOperatorBlind vQueuedBytesConfidential vAckImpliesQueued \
  -- $BASELINE_HOLDS
# W1, W2, W3, W5, W6, W9, and the antecedents of G3, G4.
job reaches baseline quietStep 80 \
  wPending wTxInMempool wTxMined wRequeued wDroppedExpired wUnparseableMissed \
  vTxidAuthenticity vLookupValidityPerHub \
  -- $BASELINE_HOLDS
# W4 (the fifth refusal), W7, W12.
job reaches baseline outageStep 80 \
  wRefusedFull wDroppedExhausted wQueueOverCapacity \
  -- $BASELINE_HOLDS

job reaches byzShim quietStep 40 \
  vAckImpliesQueued \
  -- ackImpliesQueued wellFormed
# W16, both halves.
job reaches byzHub quietStep 40 \
  vOperatorBlind vTxidAuthenticity wTwinServed wFalseHeightServed \
  -- operatorBlind txidAuthenticity wellFormed
job reaches byzIndexer quietStep 40 \
  vOperatorBlind vTxidAuthenticity vAckImpliesQueued \
  -- operatorBlind txidAuthenticity ackImpliesQueued wellFormed
# W13, K1c.
job reaches replicated step 40 \
  wFailoverAnswered wToldPrefixOnly \
  -- lookupValidityPerHub wellFormed
# W14.
job reaches replicated quietStep 80 \
  vLookupValidityPerHub wPublishedByTwoHubs \
  -- lookupValidityPerHub wellFormed
job reaches replicatedOneByz quietStep 40 \
  vTxidAuthenticity vAckImpliesQueued \
  -- txidAuthenticity ackImpliesQueuedForHonestHubs wellFormed
finish

echo "---- 4 hub specification (TLC, exhaustive)"

G6A=offeredBeforeExpiry
G6B=conformingFirstOfferBeforeExpiry
G6C=conformingFirstOfferJudgedBeforeExpiry
K5=ackedIsHeldOrSettled
K6=conformingEveryOfferBeforeExpiry

# The schedule guarantees with every component honest: all of them under a
# timely tip; under a tip that may be reported behind the chain, those for
# supported wallets; and with a slow flight as well, the one about the offer.
job tlc_holds    initTimely             step "$G6A and $G6B and $G6C"
job tlc_holds    initTimely             step $K6
job tlc_holds    initFlakyTip           step "$G6B and $G6C"
job tlc_holds    initFlakyTipSlowFlight step $G6B

# The known gaps, each on the configuration that isolates its cause. The last
# argument is the length of TLC's counterexample.
job tlc_violated initFlakyTip           step $G6A 8                     # K3
job tlc_violated initFlakyTipNoSlack    step $G6B 12                    # K3'
job tlc_violated initFlakyTipSlowFlight step $G6C 14                    # K7
job tlc_violated initStaleLag           step $G6A 12                    # K4
job tlc_violated initStaleLag           step $G6B 13                    # K4
job tlc_violated initStaleLag           step $G6C 14                    # K4
job tlc_violated initStaleLag           step $K6 13                     # K6
job tlc_violated initStaleLagWithSlack  step $G6B 19                    # finding 2
job tlc_violated initStaleLagWithSlack  step $G6C 20                    # finding 2
# K5, three causes: a crash; without one, a final flush nothing judged; with
# no shutdown either, a requeue that gives the entry up as expired.
job tlc_violated initTimely             step $K5 5
job tlc_violated initTimely             noCrashStep $K5 8
job tlc_violated initTimely             quietStep $K5 9

# The trust matrix: each schedule guarantee is violated once the component it
# depends on is Byzantine.
job tlc_violated initByzHub             step $G6A 8
job tlc_violated initByzHub             step $G6B 12
job tlc_violated initByzHub             step $G6C 13
job tlc_violated initByzIndexer         step $G6A 8
job tlc_violated initByzIndexer         step $G6B 16
job tlc_violated initByzIndexer         step $G6C 17

# Reachability. The antecedents of G6a, G6b and G6c, so that a "holds" is not
# vacuous; and one state per family of steps, because TLC runs with deadlock
# checking off and a machine whose steps died would hold everything.
job tlc_violated initTimely             step "not(wOfferWithExpiry)" 6
job tlc_violated initTimely             step "not(wConformingFirstOffer)" 6
job tlc_violated initTimely             step "not(wConformingFirstOfferInFlightABlock)" 7
job tlc_violated initTimely             step "not(wOffered)" 7
job tlc_violated initTimely             step "not(wRequeued)" 9
job tlc_violated initTimely             step "not(wDown)" 2
job tlc_violated initTimely             step "not(wRestartedOwing)" 6
job tlc_violated initTimely             step "not(wBlockInFlight)" 7
job tlc_violated initTimely             step "not(wStopped)" 4
job tlc_violated initFlakyTip           step "not(wOfferWithExpiry)" 6
job tlc_violated initFlakyTip           step "not(wConformingFirstOffer)" 6
job tlc_violated initFlakyTip           step "not(wConformingFirstOfferInFlightABlock)" 7
job tlc_violated initFlakyTip           step "not(wOffered)" 7
job tlc_violated initFlakyTip           step "not(wRequeued)" 9
job tlc_violated initFlakyTip           step "not(wDown)" 2
job tlc_violated initFlakyTip           step "not(wRestartedOwing)" 6
job tlc_violated initFlakyTip           step "not(wBlockInFlight)" 7
job tlc_violated initFlakyTip           step "not(wStopped)" 4
job tlc_violated initFlakyTip           step "not(wTimelyQueuedBehindEpoch)" 6
job tlc_violated initFlakyTipSlowFlight step "not(wConformingFirstOffer)" 6
job tlc_violated initFlakyTipSlowFlight step "not(wBlockInFlight)" 7
job tlc_violated initStaleLag           step "not(wStale)" 6
job tlc_violated initStaleLagWithSlack  step "not(wStale)" 6
finish

# Tier 5. Not part of the default gate, not run in CI, and never executed while
# this script was written: the verdict strings matched below are what Quint
# prints for the simulator, and are untested against the TLC backend.
if [ "${QUINT_TLC:-0}" = "1" ]; then
  echo "---- 5 two-state properties (TLC)"
  out=$($QUINT verify instances.qnt --backend=tlc --main=baseline \
      --temporal=chainMonotone,neverEvict,drainIsFinal 2>&1)
  verdict
  case $got in
    holds) echo "ok    baseline: holds: chainMonotone neverEvict drainIsFinal (TLC)" ;;
    *)
      echo "$out" | tail -40
      fail "baseline: two-state properties under TLC"
      ;;
  esac
fi

exit "$failures"

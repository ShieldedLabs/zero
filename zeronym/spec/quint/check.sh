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
#   2  quint test: the functional layer and the scripted runs, among them
#      `liveInitsTest`, which starts from every named init.
#   3  quint run: invariants. "holds" rows are the guarantees, on the
#      configurations where they are claimed. The "fails" row is K2, a known
#      gap; it shows polarity only, and the scripted runs in tier 2 carry the
#      causes. A guarantee under the Byzantine component it depends on has a
#      scripted run and its control, and no row here.
#   3b quint run: witnesses. Every listed state must be reached in at least
#      one trace. These runs also re-check the configuration's guarantees, on
#      longer traces and under the narrower step relations, which get deeper
#      into the protocol than `step` does.
#   4  tlc.sh: the hub specification, exhaustively. "holds" rows are the
#      schedule guarantees; "violated" rows are the known gaps, the guarantees
#      under a Byzantine hub or indexer, and the states that must be reachable
#      (each as `not(..)`). Needs Java; fails, never skips, without it.
#
# A row that starts holding where it is expected to fail, or the reverse,
# means the specification or the prediction changed: read README.md before
# changing the row.
#
# QUINT defaults to `npx @informalsystems/quint@0.33.0`. QUINT_BACKEND=typescript
# skips the Rust evaluator, which is downloaded from GitHub on first use; the
# sample counts and seeds below were settled on the Rust evaluator.
# QUINT_JOBS is how many rows run at once. CHECK_TIERS picks what runs after
# tier 1: `simulation` (2 to 3b), `tlc` (4) or `all`, the default.
set -u

cd "$(dirname "$0")"
QUINT=${QUINT:-"npx --yes @informalsystems/quint@0.33.0"}
BACKEND=${QUINT_BACKEND:-rust}
SAMPLES=${QUINT_SAMPLES:-2000}
JOBS=${QUINT_JOBS:-4}
TIERS=${CHECK_TIERS:-all}
case $TIERS in
  all | simulation | tlc) ;;
  *) echo "CHECK_TIERS is '$TIERS', not all, simulation or tlc" >&2; exit 2 ;;
esac
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

# Each file with tests carries the fewest it may report, so a test that stops
# being found (renamed so it no longer ends in `Test`, say) fails the gate.
SPELLS="spells/basicSpells.qnt:6 spells/soup.qnt:4"
MODULES="types.qnt wire.qnt indexer.qnt hub.qnt abstractHub.qnt hubMachine.qnt shim.qnt protocol.qnt"
FUNCTIONAL="tests/wireTest.qnt:11 tests/indexerTest.qnt:14 tests/hubTest.qnt:26 tests/shimTest.qnt:13
  tests/hubScenariosTest.qnt:28 tests/scenariosTest.qnt:21 tests/trustTest.qnt:12"

fail() {
  echo "FAIL  $1"
}

# run_tests FILE:MIN: every test passes, and there are at least MIN.
run_tests() {
  file=${1%:*} least=${1##*:}
  out=$($QUINT test "$file" --backend="$BACKEND" 2>&1)
  status=$?
  passing=$(echo "$out" | sed -n 's/^ *\([0-9][0-9]*\) passing.*/\1/p')
  if [ "$status" -eq 0 ] && [ -n "$passing" ] && [ "$passing" -ge "$least" ]; then
    echo "ok    test $file: $passing passing"
  elif [ "$status" -eq 0 ] && [ -n "$passing" ]; then
    fail "test $file: $passing passing, expected at least $least"
  else
    echo "$out" | tail -25
    fail "test $file"
  fi
}

# simulate CONFIG STEP MAX_STEPS ARGS...: one simulation of $samples traces
# from CONFIG's named init; the output is left in $out.
samples=$SAMPLES
simulate() {
  main=$1 step=$2 steps=$3
  shift 3
  case $main in
    baseline) init=initBaseline ;;
    byzHub) init=initByzHub ;;
    byzIndexer) init=initByzIndexer ;;
    *) init=none ;;
  esac
  out=$($QUINT run protocol.qnt --backend="$BACKEND" --main=protocol --init="$init" --step="$step" \
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

# holds CONFIG INVARIANT...: all of them, together, under `step`.
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

# fails CONFIG STEP MAX_STEPS INVARIANT [TRACES]: violated in some trace of STEP.
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

# reaches CONFIG STEP MAX_STEPS WITNESS... -- INVARIANT...: every witness is
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
for file in $SPELLS $MODULES $FUNCTIONAL; do
  job typecheck "${file%:*}"
done
finish
if [ "$failures" -ne 0 ]; then
  exit "$failures"
fi

if [ "$TIERS" != tlc ]; then
echo "---- 2 tests"
for file in $SPELLS $FUNCTIONAL; do
  job run_tests "$file"
done
finish

echo "---- 3 invariants ($SAMPLES traces, seed $SEED)"

# The guarantees, where they are claimed.
job holds baseline            operatorBlind queuedBytesConfidential txidAuthenticity lookupValidityPerHub
job holds byzHub              operatorBlind txidAuthenticity
job holds byzIndexer          operatorBlind txidAuthenticity

# The known gaps, with every component honest.
job fails baseline            quietStep 40 statusNeverRegresses                    # K2

finish

echo "---- 3b witnesses ($SAMPLES traces, seed $SEED)"

BASELINE_HOLDS="operatorBlind queuedBytesConfidential txidAuthenticity lookupValidityPerHub"

# W8, W19, and the antecedents of G1 and G2. W19 is G2's reply-body branch,
# which `vQueuedBytesConfidential` does not reach on its own.
job reaches baseline step 40 \
  wQueuedDisclosed wThirdPartyServedBody \
  vOperatorBlind vQueuedBytesConfidential \
  -- $BASELINE_HOLDS
# The antecedent of G3, and G4's under a step that keeps to one migration.
job reaches baseline quietStep 80 \
  vTxidAuthenticity \
  -- $BASELINE_HOLDS
job reaches baseline earlyLookupStep 40 \
  vLookupValidityPerHub \
  -- $BASELINE_HOLDS

# W16, both halves.
job reaches byzHub quietStep 40 \
  vOperatorBlind vTxidAuthenticity wTwinServed wFalseHeightServed \
  -- operatorBlind txidAuthenticity
job reaches byzIndexer quietStep 40 \
  vOperatorBlind vTxidAuthenticity \
  -- operatorBlind txidAuthenticity
finish
fi

if [ "$TIERS" != simulation ]; then
echo "---- 4 hub specification (TLC, exhaustive)"

G6B=conformingFirstOfferBeforeExpiry
G6C=conformingFirstOfferJudgedBeforeExpiry
K5=ackedIsHeldOrSettled
K6=conformingEveryOfferBeforeExpiry

# The schedule guarantees with every component honest: under a timely tip;
# under a tip that may be reported behind the chain; and with a slow flight as
# well, the one about the offer.
job tlc_holds    initTimely             step "$G6B and $G6C"
job tlc_holds    initTimely             step $K6
job tlc_holds    initFlakyTip           step "$G6B and $G6C"
job tlc_holds    initFlakyTipSlowFlight step $G6B

# The known gaps, each on the configuration that isolates its cause. The last
# argument is the length of TLC's counterexample.
job tlc_violated initFlakyTipNoSlack    step $G6B 12                    # K3'
job tlc_violated initFlakyTipSlowFlight step $G6C 14                    # K7
job tlc_violated initStaleLag           step $G6B 13                    # K4
job tlc_violated initStaleLag           step $G6C 14                    # K4
job tlc_violated initStaleLag           step $K6 13                     # K6
job tlc_violated initStaleLagWithSlack  step $G6B 19                    # finding 2
job tlc_violated initStaleLagWithSlack  step $G6C 20                    # finding 2
# K5, three causes: a crash; without one, a final flush nothing judged; with
# no shutdown either, a requeue that gives the entry up as expired.
job tlc_violated initTimely             step $K5 5
job tlc_violated initTimely             noCrashStep $K5 8
job tlc_violated initTimely             quietStep $K5 12

# The trust matrix: each schedule guarantee is violated once the component it
# depends on is Byzantine.
job tlc_violated initByzHub             step $G6B 12
job tlc_violated initByzHub             step $G6C 13
job tlc_violated initByzIndexer         step $G6B 16
job tlc_violated initByzIndexer         step $G6C 17

# Reachability. The antecedents of G6b and G6c, so that a "holds" is not
# vacuous; and one state per family of steps, because TLC runs with deadlock
# checking off and a machine whose steps died would hold everything.
job tlc_violated initTimely             step "not(wConformingFirstOffer)" 6
job tlc_violated initTimely             step "not(wConformingFirstOfferInFlightABlock)" 7
job tlc_violated initTimely             step "not(wOffered)" 7
job tlc_violated initTimely             step "not(wRequeued)" 9
job tlc_violated initTimely             step "not(wDown)" 2
job tlc_violated initTimely             step "not(wRestartedOwing)" 6
job tlc_violated initTimely             step "not(wBlockInFlight)" 7
job tlc_violated initTimely             step "not(wStopped)" 4
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
fi

exit "$failures"

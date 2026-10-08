#!/bin/sh
# Check one invariant of one configuration exhaustively, with TLC.
#
#   tlc.sh FILE MAIN INIT STEP INVARIANT
#
# FILE is a Quint file, MAIN the module in it, INIT a named init action built
# on `initWith`, STEP a step relation and INVARIANT a state predicate. On
# success exactly one line is printed:
#
#   holds <distinct states> <depth>     every reachable state satisfies it
#   violated <trace length>             TLC found a counterexample that long
#
# Anything else is a failure: a line `FAIL <stage>: <reason>` on stderr, no
# verdict line, and a non-zero exit status. Nothing skips a stage.
#
# The route, and why it is not `quint verify`: the Apalache server behind
# `quint verify` refuses a specification larger than 20 MB, and these compile
# to more. So the three stages are run by hand:
#
#   1. quint compile --target=json   (stdout; `--out` writes another format)
#   2. apalache-mc typecheck         (exports TLA+, named after the output file)
#   3. tlc2.TLC from the Apalache jar
#
# Between 2 and 3 a filter removes the primes that the export leaves on the
# assignments of the init operator, which TLC rejects. That is a workaround
# for the export, tied to the pinned versions below.
#
# TLC_WORKERS and TLC_HEAP size the run, and TLC_TIMEOUT (seconds) bounds it: a
# run that TLC has not finished by then is a failure, never a verdict. QUINT is
# the Quint command. TLC_TRACE, if set, names a file that receives TLC's output
# when it finds a violation.
set -u

QUINT=${QUINT:-"npx --yes @informalsystems/quint@0.33.0"}
QUINT_VERSION=0.33.0
APALACHE_VERSION=0.62.1
APALACHE=$HOME/.quint/apalache-dist-$APALACHE_VERSION/apalache
JAR=$APALACHE/lib/apalache.jar
WORKERS=${TLC_WORKERS:-2}
HEAP=${TLC_HEAP:-4g}
TIMEOUT=${TLC_TIMEOUT:-300}

die() {
  echo "FAIL  $1" >&2
  exit 1
}

[ $# -eq 5 ] || die "usage: tlc.sh FILE MAIN INIT STEP INVARIANT"
case $1 in
  /*) file=$1 ;;
  *) file=$PWD/$1 ;;
esac
main=$2 init=$3 step=$4 invariant=$5
[ -f "$file" ] || die "compile: no such file: $file"

command -v java >/dev/null 2>&1 || die "toolchain: java not found"
java -version >/dev/null 2>&1 || die "toolchain: java does not run"
version=$($QUINT --version 2>/dev/null)
[ "$version" = "$QUINT_VERSION" ] || die "toolchain: quint is '$version', not $QUINT_VERSION"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work" || die "toolchain: cannot enter $work"

# The Apalache distribution is fetched by Quint the first time it verifies
# anything. The verdict of this run is irrelevant.
if [ ! -f "$JAR" ]; then
  cat >fetch.qnt <<'EOF'
module fetch {
  var x: int
  action init = x' = 0
  action step = x' = x
}
EOF
  $QUINT verify fetch.qnt --max-steps=1 >fetch.log 2>&1
  [ -f "$JAR" ] || { tail -5 fetch.log >&2; die "toolchain: no Apalache $APALACHE_VERSION at $JAR"; }
fi

# 1. Compile. A misspelt name leaves stdout empty and exits non-zero.
if ! $QUINT compile --target=json --main="$main" --init="$init" --step="$step" \
    --invariant="$invariant" "$file" >"$main.qnt.json" 2>compile.log; then
  tail -5 compile.log >&2
  die "compile: quint compile failed ($main $init $step $invariant)"
fi
[ -s "$main.qnt.json" ] || die "compile: quint compile wrote nothing"

# 2. Export. TLC wants the module named after its file.
if ! "$APALACHE/bin/apalache-mc" typecheck --out-dir="$work/apalache" \
    --output="$work/export.tla" "$main.qnt.json" >export.log 2>&1; then
  tail -5 export.log >&2
  die "export: apalache-mc typecheck failed"
fi
grep -q "^-* MODULE export -*\$" export.tla 2>/dev/null || die "export: no TLA+ module was written"

# The filter. It rewrites `x' := e` to `x := e` (the export may break the line
# after the prime) in the definitions named
# `initWith` and INIT and in no other, so a step action whose name merely
# contains "init" keeps its assignments. Exactly one of the two holds
# assignments (`initWith`); any other count means the export is not shaped as
# expected, and a verdict from it would be about another machine.
if ! awk -v names="initWith $init" -v expected=1 '
  BEGIN { count = split(names, list, " "); for (i = 1; i <= count; i++) wanted[list[i]] = 1 }
  /^[A-Za-z_][A-Za-z0-9_]*(\(.*\))? ==/ {
    name = $0
    sub(/[( ].*/, "", name)
    inside = (name in wanted)
    fresh = 1
  }
  /^$/ { inside = 0 }
  inside {
    if (gsub(/\047 :=/, " :=") + sub(/\047$/, "") > 0 && fresh) { changed++; fresh = 0 }
    if ($0 ~ /[A-Za-z0-9_]\047/) { left++ }
  }
  { sub(/ MODULE export /, " MODULE " module " ") ; print }
  END {
    if (left > 0) { print "a primed variable is left in an init operator" > "/dev/stderr"; exit 4 }
    if (changed != expected) {
      print changed + 0 " init definitions rewritten, expected " expected > "/dev/stderr"
      exit 3
    }
  }' module="$main" export.tla >"$main.tla" 2>filter.log; then
  cat filter.log >&2
  die "filter: the init operator is not as expected"
fi

# 3. TLC. `-deadlock` switches deadlock checking off: every machine here stops
# at its height bound.
printf 'INIT q_init\nNEXT q_step\nINVARIANT q_inv\n' >"$main.cfg"
java "-Xmx$HEAP" -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -deadlock -workers "$WORKERS" \
  -metadir "$work/states" -config "$main.cfg" "$main.tla" >tlc.log 2>&1 &
tlc=$!
(sleep "$TIMEOUT" && touch "$work/timed-out" && kill "$tlc") >/dev/null 2>&1 &
watchdog=$!
wait "$tlc"
status=$?
kill "$watchdog" 2>/dev/null
wait "$watchdog" 2>/dev/null

# Out of time. How far TLC got is reported, as its last progress line has it.
if [ -f "$work/timed-out" ]; then
  reached=$(sed -n 's/^Progress(\([0-9]*\)).*, \([0-9,]*\) distinct states found.*, \([0-9,]*\) states left on queue.*/\2 distinct states, depth \1, \3 on queue/p' tlc.log | tail -1)
  die "tlc: not exhausted in $TIMEOUT s (${reached:-no progress reported})"
fi

# A dead init (a guard that is false) leaves TLC with nothing to explore, and
# it reports "No error has been found".
if grep -q "Finished computing initial states: 0 distinct states generated" tlc.log; then
  die "tlc: no initial state: the guard of $init is false"
fi
if [ "$status" -eq 0 ] \
    && grep -q "Finished computing initial states: [1-9]" tlc.log \
    && grep -q "Model checking completed. No error has been found." tlc.log \
    && grep -q " 0 states left on queue" tlc.log; then
  states=$(sed -n 's/^[0-9][0-9]* states generated, \([0-9][0-9]*\) distinct states found, 0 states left on queue.*/\1/p' tlc.log | tail -1)
  depth=$(sed -n 's/^The depth of the complete state graph search is \([0-9][0-9]*\).*/\1/p' tlc.log | tail -1)
  [ -n "$states" ] && [ -n "$depth" ] || { tail -15 tlc.log >&2; die "tlc: no state count or depth reported"; }
  echo "holds $states $depth"
elif [ "$status" -eq 12 ] && grep -q "Error: Invariant q_inv is violated by the initial state" tlc.log; then
  [ -n "${TLC_TRACE:-}" ] && cp tlc.log "$TLC_TRACE"
  echo "violated 1"
elif [ "$status" -eq 12 ] && grep -q "Error: Invariant q_inv is violated." tlc.log; then
  length=$(grep -c '^State [0-9][0-9]*:' tlc.log)
  [ "$length" -gt 0 ] || { tail -15 tlc.log >&2; die "tlc: a violation with no trace"; }
  [ -n "${TLC_TRACE:-}" ] && cp tlc.log "$TLC_TRACE"
  echo "violated $length"
else
  tail -15 tlc.log >&2
  die "tlc: no verdict (exit status $status)"
fi

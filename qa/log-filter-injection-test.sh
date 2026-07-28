#!/usr/bin/env bash
#
# Regression test for the zebrad-log-filter shell-injection fix
# ([upstream-pending #11050], upstream ZcashFoundation/zebra#11050).
#
# The old filter piped each log line through GNU sed's `e` flag, which executes
# the pattern space as a shell command. Log text reached that shell verbatim, so
# an attacker who could influence a logged string (a peer address, an error
# message, an RPC argument echoed into a log) could execute commands on the
# operator's box. The fix expands hashes with bash parameter expansion and
# printf instead, so nothing derived from a log line is ever executed.
#
# This lives in qa/ rather than the z3 smoke probes because the runtime zebra
# image ships only the zebrad binary and entrypoint.sh; zebra-utils scripts are
# not in it. The test needs no build, no container, and no chain: it runs the
# script directly against crafted input and takes well under a second.
#
# Host note: the vulnerability needs GNU sed. On macOS, BSD sed rejects the `e`
# flag outright, so the old script fails closed and the three injection
# assertions below pass even against it; the expansion and passthrough
# assertions are what catch the regression there. On Linux (CI, and every image
# we ship) the injection assertions are the live ones. Verified against the
# pre-fix script: 4 of 7 assertions fail on macOS, so the test does detect a
# reverted fix on either platform.
#
# Usage: qa/log-filter-injection-test.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FILTER="$REPO_ROOT/zebra/zebra-utils/zebrad-log-filter"

FAILURES=0
PASSES=0
pass() { PASSES=$((PASSES + 1)); printf 'ok:   %s\n' "$*"; }
fail() { FAILURES=$((FAILURES + 1)); printf 'FAIL: %s\n' "$*" >&2; }

if [ ! -x "$FILTER" ]; then
  echo "FAIL: $FILTER is missing or not executable" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Stand in for zebrad-hash-lookup so the filter has something to pipe into.
# Echoes a marker so we can prove the hash actually reached the lookup path.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/zebrad-hash-lookup" <<'STUB'
#!/bin/sh
read -r h
echo "[LOOKUP:$h]"
STUB
chmod +x "$WORK/bin/zebrad-hash-lookup"

HASH="$(printf 'a%.0s' $(seq 64))"
CANARY="$WORK/PWNED"

run_filter() { PATH="$WORK/bin:$PATH" bash "$FILTER"; }

# --- 1. command substitution in a log line is not executed ----------------
rm -f "$CANARY"
out=$(printf 'block %s accepted $(touch %s) done\n' "$HASH" "$CANARY" | run_filter 2>&1) || true
if [ -e "$CANARY" ]; then
  fail "command substitution executed: \$(touch ...) ran"
else
  pass "command substitution in a log line is not executed"
fi
if printf '%s' "$out" | grep -q "LOOKUP:$HASH"; then
  pass "the hash still reaches zebrad-hash-lookup"
else
  fail "hash was not expanded; filter output: $out"
fi
if printf '%s' "$out" | grep -qF '$(touch'; then
  pass "the payload is reproduced literally, not evaluated"
else
  fail "payload text was not preserved verbatim; filter output: $out"
fi

# --- 2. backtick substitution is not executed -----------------------------
rm -f "$CANARY"
printf 'peer %s said `touch %s`\n' "$HASH" "$CANARY" | run_filter > /dev/null 2>&1 || true
if [ -e "$CANARY" ]; then
  fail "backtick substitution executed"
else
  pass "backtick substitution in a log line is not executed"
fi

# --- 3. shell metacharacters are not interpreted --------------------------
rm -f "$CANARY"
printf 'tx %s ; touch %s\n' "$HASH" "$CANARY" | run_filter > /dev/null 2>&1 || true
if [ -e "$CANARY" ]; then
  fail "a ';' in a log line started a new command"
else
  pass "shell metacharacters are not interpreted"
fi

# --- 4. backslashes in log text survive -----------------------------------
# The old sed pipeline mangled these; #11050 explicitly restored them.
out=$(printf 'path C:\\temp\\zebra %s\n' "$HASH" | run_filter 2>&1) || true
if printf '%s' "$out" | grep -qF 'C:\temp\zebra'; then
  pass "backslashes in log text are preserved"
else
  fail "backslashes were mangled; filter output: $out"
fi

# --- 5. a line with no hash passes through unchanged -----------------------
out=$(printf 'plain log line with no hash\n' | run_filter 2>&1) || true
if [ "$out" = "plain log line with no hash" ]; then
  pass "a line with no hash passes through unchanged"
else
  fail "expected the line verbatim, got: $out"
fi

echo "=== log-filter injection test: $PASSES passed, $FAILURES failed ==="
[ "$FAILURES" -eq 0 ]

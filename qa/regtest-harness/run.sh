#!/usr/bin/env bash
# Z3 regtest harness: deterministic wallet-behavior regression tests.
#
# Runs zebrad (regtest, internal miner) and zallet (zaino backend) as local
# processes against a throwaway datadir, drives them through the wallet
# scenarios that have failed in production, and asserts on RPC responses and
# wallet-database state. No network access is required or performed: regtest
# never dials peers, and both keypairs below are public test constants that
# can never hold real funds.
#
# Scenario <-> incident map (see qa/regtest-harness/README.md):
#   baseline        fresh-wallet sync, listunspent contract and latency
#   dust            sub-marginal-fee UTXOs listed (zcash/zallet#594)
#   filter          per-address filtering, no cross-address leakage (#595)
#   union           multi-address filters return the union (#596)
#   hang-guard      filtered listing must not sweep other addresses' dust
#                   (v11 -> v12 regression; ~10 min hang in production)
#   poison-heal     a stored tx row with no mined height and zero expiry must
#                   not crash the wallet, and must self-heal (zcash/zallet#568)
#
# Scenarios run in the order above and are stateful by design (later ones
# build on earlier ones' chain/wallet); `--only` skips assertions but never
# reorders.
#
# Usage:
#   qa/regtest-harness/run.sh [--build] [--only <name>[,<name>...]] [--keep]
#
#   --build   cargo-build the required binaries first (otherwise the script
#             expects ZEBRAD_BIN / ZALLET_BIN, defaulting to target/debug).
#   --only    run a subset of scenarios (comma-separated names). Setup always
#             runs.
#   --keep    leave the stack running and the workdir in place on exit.
#
# Environment:
#   ZEBRAD_BIN, ZALLET_BIN   binary paths (defaults below)
#   HARNESS_WORKDIR          working directory (default: mktemp -d)
#   MINE_PHASE_BLOCKS        blocks mined per funding phase (default 120)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ZEBRAD_BIN="${ZEBRAD_BIN:-$REPO_ROOT/zebra/target/debug/zebrad}"
ZALLET_BIN="${ZALLET_BIN:-$REPO_ROOT/zallet/target/debug/zallet}"
MINE_PHASE_BLOCKS="${MINE_PHASE_BLOCKS:-120}"

# Public test constants (regtest-only; never reuse where value can exist).
# Compressed secp256k1 pubkeys and their regtest P2PKH encodings.
PUBKEY_A="0220f133a0751f6a70ce2dc506da68891b827296a0b13fb7883ceea25f7926f5d5"
ADDR_A="tmYGYsZtgazT5DYJaRG47AuEDCexnQyvS2U"
PUBKEY_B="02a340922511d719b08b4d79509909d503fff005a01b72e73e867367f430e1bc00"
ADDR_B="tmVb46GXKmsy4iogqVYaR5ZcmdNr4cXaU3V"
# Syntactically valid regtest address that is never imported: filter queries
# naming it must return empty results, proving no cross-address leakage.
ADDR_UNRELATED="tmJGRY2ME1HZqWbg8wKVQYo6tTrC5WJ9ENv"

ZEBRA_RPC_PORT="${ZEBRA_RPC_PORT:-18232}"
ZALLET_RPC_PORT="${ZALLET_RPC_PORT:-28232}"
RPC_USER=harness
RPC_PASS=harness
# Per-RPC-call timeout. Anything slower on a tiny regtest wallet is a
# regression of the production hang class; scenarios with legitimately
# heavier calls pass an explicit larger budget.
RPC_CALL_TIMEOUT=15
# Bound for every polling wait in the harness.
WAIT_TIMEOUT=300

KEEP=0
BUILD=0
ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --build) BUILD=1 ;;
    --only) ONLY="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

WORKDIR="${HARNESS_WORKDIR:-$(mktemp -d "${TMPDIR:-/tmp}/z3-regtest.XXXXXX")}"
mkdir -p "$WORKDIR"
ZEBRA_PID=""
ZALLET_PID=""
FAILURES=0
PASSES=0

log() { printf '%s %s\n' "[$(date +%H:%M:%S)]" "$*"; }

fail() {
  FAILURES=$((FAILURES + 1))
  printf 'FAIL: %s\n' "$*" >&2
}

pass() {
  PASSES=$((PASSES + 1))
  printf 'ok:   %s\n' "$*"
}

# assert <description> <command...>: run the command in THIS shell (so shell
# functions are callable), record pass/fail without aborting the run.
assert() {
  local desc="$1"
  shift
  if "$@"; then pass "$desc"; else fail "$desc"; fi
}

# Small predicates for use with `assert` (avoids `bash -c`, which cannot see
# shell functions).
# grep reads to EOF (no -q): -q closes the pipe on match and under
# pipefail the resulting SIGPIPE in printf would invert the result for
# large haystacks (same pitfall documented in z3-smoke.yml).
contains() { printf '%s\n' "$1" | grep -- "$2" > /dev/null; }
not_contains() { ! contains "$1" "$2"; }

cleanup() {
  local status=$?
  if [ "$KEEP" = 1 ]; then
    log "--keep: leaving stack running in $WORKDIR (zebra pid=$ZEBRA_PID, zallet pid=$ZALLET_PID)"
    return
  fi
  stop_zallet || true
  stop_zebra || true
  if [ "$status" -ne 0 ] || [ "$FAILURES" -gt 0 ]; then
    log "failure: preserving workdir for inspection: $WORKDIR"
  else
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# wait_until <description> <command...>: poll (1s) until the command succeeds
# or WAIT_TIMEOUT elapses. The command itself must be internally bounded.
wait_until() {
  local desc="$1"
  shift
  local deadline=$((SECONDS + WAIT_TIMEOUT))
  until "$@" > /dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      fail "timed out waiting for: $desc"
      return 1
    fi
    sleep 1
  done
}

zebra_rpc() {
  local method="$1" params="${2:-[]}"
  curl -sf -m "$RPC_CALL_TIMEOUT" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":\"h\",\"method\":\"$method\",\"params\":$params}" \
    "http://127.0.0.1:$ZEBRA_RPC_PORT"
}

# wallet_rpc <method> [params-json] [per-call timeout]
wallet_rpc() {
  local method="$1" params="${2:-[]}" tmo="${3:-$RPC_CALL_TIMEOUT}"
  curl -sf -m "$tmo" -u "$RPC_USER:$RPC_PASS" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":\"h\",\"method\":\"$method\",\"params\":$params}" \
    "http://127.0.0.1:$ZALLET_RPC_PORT"
}

# All harness SQL goes through one entry point with a busy timeout, so
# concurrent reads against a live zallet don't flake on transient locks.
# Writes only ever happen while zallet is stopped.
wallet_db() {
  sqlite3 -cmd '.timeout 5000' "$WORKDIR/zallet-data/wallet.db" "$@"
}

zebra_height() {
  zebra_rpc getblockcount | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"])'
}

write_zebra_config() {
  local miner_address="$1" internal_miner="$2"
  cat > "$WORKDIR/zebrad.toml" <<EOF
[network]
network = "Regtest"
# Regtest never dials peers, but zebra still binds a P2P listener; port 0
# picks a free one so concurrent harnesses (or unrelated zebra instances on
# the default 18233) cannot collide.
listen_addr = "127.0.0.1:0"

[network.testnet_parameters.activation_heights]
NU5 = 1

[mining]
miner_address = '$miner_address'
internal_miner = $internal_miner

[state]
cache_dir = "$WORKDIR/zebra-cache"

[rpc]
listen_addr = "127.0.0.1:$ZEBRA_RPC_PORT"
enable_cookie_auth = false
cookie_dir = "$WORKDIR"
EOF
}

start_zebra() {
  local logfile="$1"
  "$ZEBRAD_BIN" -c "$WORKDIR/zebrad.toml" start >> "$WORKDIR/$logfile" 2>&1 &
  ZEBRA_PID=$!
  wait_until "zebra RPC answering" zebra_rpc getblockcount
}

stop_zebra() {
  if [ -n "$ZEBRA_PID" ] && kill -0 "$ZEBRA_PID" 2>/dev/null; then
    kill "$ZEBRA_PID"
    # Bounded wait; escalate rather than hang forever on a stuck shutdown.
    for _ in $(seq 1 30); do
      kill -0 "$ZEBRA_PID" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$ZEBRA_PID" 2>/dev/null || true
    wait "$ZEBRA_PID" 2>/dev/null || true
  fi
  ZEBRA_PID=""
}

start_zallet() {
  local logfile="$1"
  RUST_LOG="${ZALLET_RUST_LOG:-info}" \
    "$ZALLET_BIN" -d "$WORKDIR/zallet-data" -c "$WORKDIR/zallet.toml" start \
    >> "$WORKDIR/$logfile" 2>&1 &
  ZALLET_PID=$!
  wait_until "zallet RPC answering" wallet_rpc getwalletstatus
}

stop_zallet() {
  if [ -n "$ZALLET_PID" ] && kill -0 "$ZALLET_PID" 2>/dev/null; then
    kill "$ZALLET_PID"
    for _ in $(seq 1 30); do
      kill -0 "$ZALLET_PID" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$ZALLET_PID" 2>/dev/null || true
    wait "$ZALLET_PID" 2>/dev/null || true
  fi
  ZALLET_PID=""
}

zallet_alive() {
  [ -n "$ZALLET_PID" ] && kill -0 "$ZALLET_PID" 2>/dev/null
}

# mine_to <address> <target-height>: (re)start zebra with the internal miner
# paying <address> until the chain reaches <target-height>, then restart it
# without the miner so the chain is static for assertions.
mine_to() {
  local address="$1" target="$2"
  stop_zebra
  write_zebra_config "$address" true
  start_zebra "zebra-mine.log"
  local deadline=$((SECONDS + 1800))
  until [ "$(zebra_height 2>/dev/null || echo 0)" -ge "$target" ]; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "::error::mining to height $target timed out" >&2
      exit 1
    fi
    sleep 5
  done
  stop_zebra
  write_zebra_config "$address" false
  start_zebra "zebra-serve.log"
}

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }

fully_synced() {
  local want="$1"
  [ "$(wallet_rpc getwalletstatus | json_field "['result'].get('fully_synced_height', -1)")" = "$want" ]
}

# Setup is converged only when the scan is complete AND every coinbase receipt
# has been ingested. Every block 1..TIP pays a watched address exactly once,
# so the receipt count must equal TIP; fully_synced alone races the receipt
# ingestion that follows the import-triggered rescan.
setup_converged() {
  fully_synced "$TIP" \
    && [ "$(wallet_db 'SELECT COUNT(*) FROM transparent_received_outputs;' 2>/dev/null)" = "$TIP" ]
}

raw_rows_present() {
  [ "$(wallet_db 'SELECT COUNT(*) FROM transactions WHERE raw IS NOT NULL AND mined_height IS NOT NULL;' 2>/dev/null)" -ge 3 ]
}

# transparent_utxos [filter-params-json] [timeout]: one line per transparent
# UTXO as "txid outindex valueZat address".
transparent_utxos() {
  local params="${1:-[]}" tmo="${2:-$RPC_CALL_TIMEOUT}"
  wallet_rpc z_listunspent "$params" "$tmo" | python3 -c '
import json, sys
for u in json.load(sys.stdin)["result"]:
    if u["pool"] == "transparent":
        print(u["txid"], u["outindex"], u["valueZat"], u.get("address"))'
}

count_lines() { grep -c . || true; }

setup() {
  log "workdir: $WORKDIR"
  [ -x "$ZEBRAD_BIN" ] || { echo "::error::zebrad not found at $ZEBRAD_BIN (need --features internal-miner)" >&2; exit 1; }
  [ -x "$ZALLET_BIN" ] || { echo "::error::zallet not found at $ZALLET_BIN" >&2; exit 1; }
  mkdir -p "$WORKDIR/zallet-data" "$WORKDIR/zebra-cache"

  cat > "$WORKDIR/zallet.toml" <<EOF
[builder.limits]
[consensus]
network = "regtest"
regtest_nuparams = ["c2d6d0b4:1"]
[database]
[external]
[features]
as_of_version = "0.0.0"
[features.deprecated]
[features.experimental]
[indexer]
validator_address = "127.0.0.1:$ZEBRA_RPC_PORT"
[keystore]
encryption_identity = "encryption-identity.txt"
[note_management]
[rpc]
bind = ["127.0.0.1:$ZALLET_RPC_PORT"]
[[rpc.auth]]
user = "$RPC_USER"
password = "$RPC_PASS"
EOF

  log "initializing wallet (offline)"
  "$ZALLET_BIN" -d "$WORKDIR/zallet-data" -c "$WORKDIR/zallet.toml" generate-encryption-identity > "$WORKDIR/init.log" 2>&1
  "$ZALLET_BIN" -d "$WORKDIR/zallet-data" -c "$WORKDIR/zallet.toml" init-wallet-encryption >> "$WORKDIR/init.log" 2>&1
  "$ZALLET_BIN" -d "$WORKDIR/zallet-data" -c "$WORKDIR/zallet.toml" generate-mnemonic >> "$WORKDIR/init.log" 2>&1
  # Creates the default account (birthday at height 0). It prints a miner
  # address we deliberately do NOT use: all funds in this harness go to the
  # imported watch addresses, mirroring the exchange deployment shape.
  "$ZALLET_BIN" -d "$WORKDIR/zallet-data" -c "$WORKDIR/zallet.toml" regtest generate-account-and-miner-address >> "$WORKDIR/init.log" 2>&1

  # Register the watch addresses BEFORE any block exists: the initial block
  # walk then records every coinbase receipt scan-side as it goes. (Importing
  # after mining would instead depend on the wallet's tip-change-driven
  # address-history passes, which advance a bounded watermark per new block
  # and therefore never complete on a frozen chain.)
  log "starting the genesis-only stack to register watch addresses"
  write_zebra_config "$ADDR_A" false
  start_zebra "zebra-serve.log"
  start_zallet "zallet.log"
  ACCOUNT=$(wallet_rpc z_listaccounts | json_field "['result'][0]['account_uuid']")
  local resp
  resp=$(wallet_rpc z_importaddress "[\"$ACCOUNT\", \"$PUBKEY_A\", false]") || resp="(rpc failure)"
  contains "$resp" '"address"' || { echo "::error::import A failed: $resp" >&2; exit 1; }
  resp=$(wallet_rpc z_importaddress "[\"$ACCOUNT\", \"$PUBKEY_B\", false]") || resp="(rpc failure)"
  contains "$resp" '"address"' || { echo "::error::import B failed: $resp" >&2; exit 1; }
  # Stop zallet while zebra bounces for the mining phases (its chain-indexer
  # watchdog treats a vanishing validator as fatal, by design).
  stop_zallet

  log "funding phase 1: mining to height $MINE_PHASE_BLOCKS for $ADDR_A"
  mine_to "$ADDR_A" "$MINE_PHASE_BLOCKS"
  # The internal miner can produce a block or two between the height check
  # and shutdown, so every expectation below derives from the ACTUAL mined
  # boundaries, never from the requested targets.
  PHASE1_TIP=$(zebra_height)
  log "funding phase 2: mining to height $((PHASE1_TIP + MINE_PHASE_BLOCKS)) for $ADDR_B"
  mine_to "$ADDR_B" $((PHASE1_TIP + MINE_PHASE_BLOCKS))
  TIP=$(zebra_height)

  # Coinbase outputs need 100 confirmations and z_listunspent evaluates
  # spendability at the next block (target height TIP+1), so heights
  # 1..TIP-99 are spendable. Addr A owns heights 1..PHASE1_TIP, addr B owns
  # PHASE1_TIP+1..TIP. (Verified empirically at two chain lengths.)
  MATURE_TOTAL=$((TIP - 99))
  MATURE_A=$((PHASE1_TIP < MATURE_TOTAL ? PHASE1_TIP : MATURE_TOTAL))
  MATURE_B=$((MATURE_TOTAL - MATURE_A))
  log "mined boundaries: phase1=$PHASE1_TIP tip=$TIP mature=(A=$MATURE_A B=$MATURE_B total=$MATURE_TOTAL)"
  if [ "$MATURE_B" -le 0 ]; then
    echo "::error::MINE_PHASE_BLOCKS too small: phase-2 coinbases never mature" >&2
    exit 1
  fi

  log "starting zallet; walking the chain (stores every coinbase transaction)"
  start_zallet "zallet.log"
  WAIT_TIMEOUT=$((TIP * 3)) wait_until "initial walk reaches $TIP" fully_synced "$TIP" \
    || { echo "::error::initial walk never converged" >&2; exit 1; }

  # Receipt rows for watch-only addresses are written by the wallet's
  # enhancement pass over its stored transactions, and that pass is driven by
  # tip changes plus one catch-up sweep at startup. On this frozen chain the
  # tip never advances, so: restart once (fresh startup sweep) and re-import
  # one key with rescan=true (queues the work). With every transaction
  # already stored by the walk above, the sweep enhances known txids directly
  # and receipt ingestion completes in one pass; this sequence is the
  # empirically deterministic one.
  log "restarting zallet and kicking a rescan to flood receipt ingestion"
  stop_zallet
  start_zallet "zallet.log"
  local resp
  resp=$(wallet_rpc z_importaddress "[\"$ACCOUNT\", \"$PUBKEY_A\", true]" 30) || resp="(rpc failure)"
  contains "$resp" '"address"' || { echo "::error::rescan kick failed: $resp" >&2; exit 1; }
  WAIT_TIMEOUT=$((TIP * 5)) wait_until "wallet synced to $TIP with all $TIP coinbase receipts ingested" setup_converged \
    || { echo "::error::setup never converged (receipts: $(wallet_db 'SELECT COUNT(*) FROM transparent_received_outputs;' 2>/dev/null))" >&2; exit 1; }
}

scenario_baseline() {
  local t0 t1 count err not_watch_only
  t0=$SECONDS
  count=$(transparent_utxos | count_lines) || true
  t1=$((SECONDS - t0))
  assert "baseline: $MATURE_TOTAL mature coinbase UTXOs listed (got $count)" \
    [ "$count" = "$MATURE_TOTAL" ]
  assert "baseline: unfiltered listing answered promptly (${t1}s)" \
    [ "$t1" -le 10 ]
  # Every transparent UTXO here landed on a standalone imported pubkey the
  # wallet cannot spend from, so each row must be flagged watch-only even
  # though the account itself holds spending keys (Binance report: imported
  # addresses came back is_watch_only=false because the flag was account-level).
  not_watch_only=$(wallet_rpc z_listunspent | python3 -c '
import json, sys
rows = [u for u in json.load(sys.stdin)["result"] if u["pool"] == "transparent"]
print(sum(1 for u in rows if not u["is_watch_only"]))' || true)
  assert "baseline: imported-address UTXOs flagged watch-only ($not_watch_only unflagged)" \
    [ "$not_watch_only" = "0" ]
  # The youngest mature coinbase output has exactly 100 confirmations at the
  # frozen tip, so maxconf=99 must exclude every transparent UTXO (regression:
  # maxconf was only enforced for shielded notes).
  count=$(transparent_utxos '[1, 99]' | count_lines) || true
  assert "baseline: maxconf=99 excludes all mature coinbase UTXOs (got $count)" \
    [ "$count" = "0" ]
  err=$(wallet_rpc z_listunspent '[5, 1]' || true)
  assert "baseline: inverted minconf/maxconf window is a clean error" \
    contains "$err" "Maximum number of confirmations"
  assert "baseline: getwalletstatus answers within ${RPC_CALL_TIMEOUT}s" \
    wallet_rpc getwalletstatus
  err=$(wallet_rpc z_listunspent '[1, 9999999, true, ["not-an-address"]]' || true)
  assert "baseline: invalid filter address is a clean error" \
    contains "$err" "Not a valid Zcash address"
}

scenario_dust() {
  stop_zallet
  local row txid_hex
  row=$(wallet_db "SELECT u.id FROM transparent_received_outputs u
                   JOIN transactions t ON t.id_tx = u.transaction_id
                   JOIN addresses a ON a.id = u.address_id
                   WHERE a.cached_transparent_receiver_address = '$ADDR_A'
                   AND t.mined_height = 50 LIMIT 1;")
  if [ -z "$row" ]; then
    fail "dust: no addr-A receipt at height 50 to poison (setup not converged?)"
    start_zallet "zallet.log"
    return
  fi
  txid_hex=$(wallet_db "SELECT lower(hex(t.txid)) FROM transparent_received_outputs u
                        JOIN transactions t ON t.id_tx = u.transaction_id WHERE u.id = $row;")
  DUST_TXID=$(python3 -c "print(bytes.fromhex('$txid_hex')[::-1].hex())")
  wallet_db "UPDATE transparent_received_outputs SET value_zat = 2796 WHERE id = $row;"
  start_zallet "zallet.log"

  local unfiltered filtered
  unfiltered=$(transparent_utxos) || true
  filtered=$(transparent_utxos "[1, 9999999, true, [\"$ADDR_A\"]]") || true
  assert "dust: 2796-zat output listed unfiltered" \
    contains "$unfiltered" "^$DUST_TXID 0 2796 "
  assert "dust: 2796-zat output listed under its address filter" \
    contains "$filtered" " 2796 "
  assert "dust: total count unchanged by the value edit" \
    [ "$(printf '%s\n' "$unfiltered" | count_lines)" = "$MATURE_TOTAL" ]
}

scenario_filter() {
  local out_a out_b out_none a b none
  out_a=$(transparent_utxos "[1, 9999999, true, [\"$ADDR_A\"]]") || true
  out_b=$(transparent_utxos "[1, 9999999, true, [\"$ADDR_B\"]]") || true
  out_none=$(transparent_utxos "[1, 9999999, true, [\"$ADDR_UNRELATED\"]]") || true
  a=$(printf '%s\n' "$out_a" | count_lines)
  b=$(printf '%s\n' "$out_b" | count_lines)
  none=$(printf '%s\n' "$out_none" | count_lines)
  assert "filter: addr A returns exactly its $MATURE_A UTXOs (got $a)" [ "$a" = "$MATURE_A" ]
  assert "filter: addr B returns exactly its $MATURE_B UTXOs (got $b)" [ "$b" = "$MATURE_B" ]
  assert "filter: unrelated address returns none (got $none)" [ "$none" = "0" ]
  assert "filter: no foreign addresses leak into the A filter" \
    not_contains "$out_a" " $ADDR_B$"
}

scenario_union() {
  local both
  both=$(transparent_utxos "[1, 9999999, true, [\"$ADDR_A\", \"$ADDR_B\"]]" | count_lines) || true
  assert "union: [A, B] filter returns the union ($MATURE_TOTAL; got $both)" \
    [ "$both" = "$MATURE_TOTAL" ]
}

scenario_hang_guard() {
  stop_zallet
  # Synthesize a large dust population on addr A. The production incident was
  # a single-address query sweeping every OTHER address's dust through a
  # per-outpoint check (~10 minutes on an exchange wallet).
  # 100 synthetic dust rows per addr-A coinbase (~12k total): enough that a
  # reverted account-wide sweep costs tens of seconds (unambiguously over the
  # 10s budget below) while the fixed, scoped path stays at milliseconds.
  wallet_db "
WITH RECURSIVE n(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM n WHERE n < 100)
INSERT INTO transparent_received_outputs
  (transaction_id, output_index, account_id, address, address_id, script, value_zat, max_observed_unspent_height)
SELECT u.transaction_id, u.output_index + 100 + n.n, u.account_id, u.address, u.address_id, u.script, 999, u.max_observed_unspent_height
FROM transparent_received_outputs u, n
WHERE u.address_id = (SELECT id FROM addresses WHERE cached_transparent_receiver_address = '$ADDR_A' LIMIT 1)
AND u.value_zat > 999 AND u.output_index = 0;"
  SYNTH_DUST=$(wallet_db "SELECT COUNT(*) FROM transparent_received_outputs WHERE value_zat = 999;")
  start_zallet "zallet.log"

  # The other address's listing must be unaffected: this is the regression
  # guard. 10s is a ~200x margin over the fixed behavior (0.05s measured)
  # and far below the broken behavior (minutes).
  local t0 elapsed b
  t0=$SECONDS
  b=$(transparent_utxos "[1, 9999999, true, [\"$ADDR_B\"]]" 12 | count_lines) || true
  elapsed=$((SECONDS - t0))
  assert "hang-guard: addr B filter untouched by $SYNTH_DUST foreign dust rows (${elapsed}s)" \
    [ "$elapsed" -le 10 ]
  assert "hang-guard: addr B result still exact (got $b)" [ "$b" = "$MATURE_B" ]

  # Listing the dusty address itself costs time proportional to its own dust,
  # but must stay inside an absolute budget and return every dust row.
  local a
  t0=$SECONDS
  a=$(transparent_utxos "[1, 9999999, true, [\"$ADDR_A\"]]" 120 | count_lines) || true
  elapsed=$((SECONDS - t0))
  assert "hang-guard: dusty address lists its own dust within budget (${elapsed}s)" \
    [ "$elapsed" -le 120 ]
  assert "hang-guard: dusty address returns base + dust rows (got $a, want $((MATURE_A + SYNTH_DUST)))" \
    [ "$a" = "$((MATURE_A + SYNTH_DUST))" ]
}

scenario_poison_heal() {
  # Raw transaction bytes are stored by the enhancement queue, which runs in
  # the background after sync; wait for it so the poison UPDATE below has
  # rows to bite (and so this scenario can never pass vacuously).
  if ! wait_until "at least 3 raw transactions stored" raw_rows_present; then
    fail "poison-heal: no raw transactions to poison; scenario cannot run"
    return
  fi
  stop_zallet
  # The production shape behind zcash/zallet#568: raw transaction rows whose
  # mined height is not yet recorded and which carry no expiry. Re-parsing
  # them fails until a GetStatus records the mined height; before the
  # containment fix this crash-looped the whole wallet on every start.
  wallet_db "UPDATE transactions SET mined_height = NULL, expiry_height = 0, confirmed_unmined_at_height = NULL
             WHERE id_tx IN (SELECT id_tx FROM transactions WHERE raw IS NOT NULL AND mined_height IS NOT NULL LIMIT 3);"
  local poisoned remaining
  poisoned=$(wallet_db "SELECT COUNT(*) FROM transactions WHERE mined_height IS NULL AND raw IS NOT NULL;")
  # Vacuity guard: if the UPDATE matched nothing the scenario proves nothing.
  assert "poison-heal: poisoned exactly 3 rows (got $poisoned)" [ "$poisoned" = "3" ]
  start_zallet "zallet.log"

  local deadline=$((SECONDS + 120))
  remaining=$poisoned
  while [ "$SECONDS" -lt "$deadline" ]; do
    zallet_alive || break
    remaining=$(wallet_db "SELECT COUNT(*) FROM transactions WHERE mined_height IS NULL AND raw IS NOT NULL;" 2>/dev/null || echo "$poisoned")
    [ "$remaining" = "0" ] && break
    sleep 3
  done
  assert "poison-heal: zallet survived $poisoned poisoned rows (no process exit)" zallet_alive
  assert "poison-heal: all poisoned rows healed (remaining: $remaining)" \
    [ "$remaining" = "0" ]
  assert "poison-heal: wallet RPC still serving" wallet_rpc getwalletstatus
}

run_scenario() {
  local name="$1"
  if [ -n "$ONLY" ] && ! contains ",$ONLY," ",$name,"; then
    log "skipping scenario: $name"
    return
  fi
  log "=== scenario: $name ==="
  "scenario_${name//-/_}"
}

main() {
  if [ "$BUILD" = 1 ]; then
    log "building zebrad (internal-miner) and zallet (zaino backend)"
    (cd "$REPO_ROOT/zebra" && cargo build -p zebrad --features internal-miner)
    (cd "$REPO_ROOT/zallet" && cargo build -p zallet --no-default-features --features zaino,rpc-cli,zcashd-import)
  fi

  setup
  run_scenario baseline
  run_scenario dust
  run_scenario filter
  run_scenario union
  run_scenario hang-guard
  run_scenario poison-heal

  log "=== results: $PASSES passed, $FAILURES failed ==="
  [ "$FAILURES" = 0 ]
}

main

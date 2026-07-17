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
#   reorg           a depth-10 reorg under a live wallet: node, index, and
#                   listings must converge on the replacement branch; plus a
#                   reorg across a wallet outage (the stored-rows-ahead-of-
#                   cursor window of zallet-issue-reorg-blockconflict.md)
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
#   ZEBRA_RPC_PORT           zebra RPC port (default 18232)
#   ZALLET_RPC_PORT          zallet RPC port (default 28232)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Release binaries by default: mining, proving, and scanning are
# compute-bound, and debug-profile runs are 5-10x slower end to end (a
# debug orchard proof alone takes minutes and widens every startup-race
# window this harness has to defend against).
ZEBRAD_BIN="${ZEBRAD_BIN:-$REPO_ROOT/zebra/target/release/zebrad}"
ZALLET_BIN="${ZALLET_BIN:-$REPO_ROOT/zallet/target/release/zallet}"
MINE_PHASE_BLOCKS="${MINE_PHASE_BLOCKS:-105}"

# Public test constants (regtest-only; never reuse where value can exist).
# Compressed secp256k1 pubkeys and their regtest P2PKH encodings.
PUBKEY_A="0220f133a0751f6a70ce2dc506da68891b827296a0b13fb7883ceea25f7926f5d5"
ADDR_A="tmYGYsZtgazT5DYJaRG47AuEDCexnQyvS2U"
PUBKEY_B="02a340922511d719b08b4d79509909d503fff005a01b72e73e867367f430e1bc00"
ADDR_B="tmVb46GXKmsy4iogqVYaR5ZcmdNr4cXaU3V"
# Syntactically valid regtest address that is never imported: filter queries
# naming it must return empty results, proving no cross-address leakage.
ADDR_UNRELATED="tmJGRY2ME1HZqWbg8wKVQYo6tTrC5WJ9ENv"
# Fixed seed for the spend-poison scenario's signer + recovery wallets (a
# standard BIP-39 test vector; regtest funds only).
MNEMONIC_POISON="abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art"
# The poison seed's account-0 first external transparent address (derivation
# is deterministic; the scenario asserts the live derivation still matches).
# Its coinbase is mined during setup so phase 2 matures it for free.
ADDR_C_POISON="tmBsTi2xWTjUdEXnuTceL7fecEQKeWaPDJd"

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
REGEN_GOLDEN=0
SETUP_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --build) BUILD=1 ;;
    --only) ONLY="$2"; shift ;;
    --regen-golden) REGEN_GOLDEN=1 ;;
    --setup-only) SETUP_ONLY=1 ;;
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

# zebra_rpc <method> [params-json] [per-call timeout]
zebra_rpc() {
  local method="$1" params="${2:-[]}" tmo="${3:-$RPC_CALL_TIMEOUT}"
  curl -sf -m "$tmo" -H 'content-type: application/json' \
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

# Same, against an alternate zallet datadir (scenarios that run their own
# wallet lifecycles).
wallet_db_at() {
  local datadir="$1"; shift
  sqlite3 -cmd '.timeout 5000' "$datadir/wallet.db" "$@"
}

# start_zallet_warm <logfile> <datadir> <expected_tip>: start zallet and wait
# until BOTH its node view and its wallet scan reach the expected tip. The
# embedded chain index has a startup window in which historic fetches and
# treestate queries can wedge outright (pre-existing zaino readiness defect,
# see zaino-issue-readiness.md); one restart clears it, so allow exactly one.
# Returns non-zero (without recording a failure) if the wallet never warms.
start_zallet_warm() {
  local logfile="$1" datadir="$2" expected_tip="$3"
  start_zallet "$logfile" "$datadir"
  local restarted="" deadline=$((SECONDS + expected_tip * 3))
  while true; do
    if [ "$(wallet_rpc getwalletstatus | json_field "['result']['node_tip']['height']" 2>/dev/null)" = "$expected_tip" ]         && fully_synced "$expected_tip" > /dev/null 2>&1; then
      return 0
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      if [ -z "$restarted" ]; then
        restarted=1
        log "zallet warm-up stalled (embedded-index startup race); restarting once"
        stop_zallet
        start_zallet "$logfile" "$datadir"
        deadline=$((SECONDS + expected_tip * 3))
      else
        return 1
      fi
    fi
    sleep 1
  done
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
  # zebrad also reads ZEBRA_*-prefixed environment variables as config
  # overrides, so the harness's own ZEBRA_RPC_PORT would reach it as an
  # unknown `rpc_port` field and abort startup. Strip it; the port is
  # already baked into the generated zebrad.toml.
  env -u ZEBRA_RPC_PORT \
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
  local logfile="$1" datadir="${2:-$WORKDIR/zallet-data}"
  # Same env-config trap as start_zebra: keep the harness's ZALLET_RPC_PORT
  # out of zallet's environment (the port is baked into zallet.toml).
  RUST_LOG="${ZALLET_RUST_LOG:-info}" \
    env -u ZALLET_RPC_PORT \
    "$ZALLET_BIN" -d "$datadir" -c "$WORKDIR/zallet.toml" start \
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

# mine_with_tx <address> <target-height> <rawtx-hex> <txid>: like mine_to,
# but resubmits the given raw transaction as soon as the mining node's RPC
# answers (a zebra restart empties the mempool, so a transaction submitted
# before the restart would otherwise never be mined). Does not stop until the
# transaction is MINED (not merely accepted: a block template built before
# acceptance would leave it in the mempool for the next restart to wipe) AND
# the target height is reached.
mine_with_tx() {
  local address="$1" target="$2" rawtx="$3" txid="$4"
  stop_zebra
  write_zebra_config "$address" true
  start_zebra "zebra-mine.log"
  local sent="" mined="" resp submitted_txid tx_height
  local deadline=$((SECONDS + 1800))
  until [ -n "$mined" ] && [ "$(zebra_height 2>/dev/null || echo 0)" -ge "$target" ]; do
    if [ -z "$sent" ]; then
      resp=$(zebra_rpc sendrawtransaction "[\"$rawtx\"]" 2>/dev/null) || resp=""
      submitted_txid=$(printf '%s' "$resp" | json_field "['result']" 2>/dev/null) || submitted_txid=""
      if printf '%s' "$submitted_txid" | grep -Eiq '^[0-9a-f]{64}$'; then
        sent=1
      fi
    elif [ -z "$mined" ]; then
      tx_height=$(zebra_rpc getrawtransaction "[\"$txid\", 1]" 2>/dev/null \
        | json_field "['result'].get('height', -1)" 2>/dev/null) || tx_height=-1
      if [ "${tx_height:--1}" -ge 0 ]; then
        mined=1
      fi
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "::error::mining to height $target with tx inclusion timed out (sent: ${sent:-no}, mined: ${mined:-no})" >&2
      exit 1
    fi
    sleep 2
  done
  stop_zebra
  write_zebra_config "$address" false
  start_zebra "zebra-serve.log"
}

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }

# zallet_prompt <input-line> <cmd> [args...]: run a command that insists on
# prompting through /dev/tty (rpassword-based zallet subcommands such as
# import-mnemonic), feeding it one line through a pseudo-terminal. Plain
# stdin pipes fail with "Device not configured" because rpassword opens the
# controlling terminal directly.
zallet_prompt() {
  local input="$1"; shift
  ZALLET_PROMPT_INPUT="$input" python3 - "$@" <<'PYEOF'
import os, pty, select, sys, time

args = sys.argv[1:]
data = (os.environ["ZALLET_PROMPT_INPUT"] + "\n").encode()
pid, fd = pty.fork()
if pid == 0:
    os.execvp(args[0], args)
deadline = time.time() + 120
wrote = False
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 1)
    if fd not in r:
        continue
    try:
        chunk = os.read(fd, 4096)
    except OSError:
        break
    if not chunk:
        break
    sys.stdout.buffer.write(chunk)
    if not wrote:
        os.write(fd, data)
        wrote = True
_, status = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(status))
PYEOF
}

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
    && [ "$(wallet_db 'SELECT COUNT(*) FROM transparent_received_outputs;' 2>/dev/null)" = "$((TIP - X_COUNT))" ]
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

# --- Golden chain -----------------------------------------------------------
# Mining and wallet convergence dominate setup (~4 minutes even at release
# profile), and their output is fully determined by the constants below, so
# it is mined once and snapshotted. BUMP GOLDEN_EPOCH whenever a change
# invalidates old snapshots that the key cannot see on its own:
#   - zallet wallet-db migrations (schema changes)
#   - zebra state format changes (cache version)
#   - any change to setup()'s logic, the config templates, or scenario
#     expectations about the mined layout
# A stale snapshot that slips through fails the restore verification below
# and falls back to a full re-mine, so a missed bump costs time, not
# correctness.
GOLDEN_EPOCH=1
GOLDEN_DIR="${GOLDEN_DIR:-$HOME/.cache/z3-harness-golden}"

golden_key() {
  printf 'epoch=%s|blocks=%s|%s|%s|%s|%s|%s'     "$GOLDEN_EPOCH" "$MINE_PHASE_BLOCKS"     "$PUBKEY_A" "$PUBKEY_B" "$ADDR_C_POISON" "$MNEMONIC_POISON" "$ADDR_UNRELATED"     | shasum -a 256 | cut -c1-16
}

# Restore a snapshot into the workdir and start the stack from it. Returns
# non-zero (leaving the workdir dirty for the full path to overwrite) if the
# snapshot is absent or fails verification.
golden_restore() {
  local key gdir
  key=$(golden_key)
  gdir="$GOLDEN_DIR/$key"
  [ -f "$gdir/meta.env" ] || return 1
  log "golden chain: restoring snapshot $key"
  rm -rf "$WORKDIR/zebra-cache" "$WORKDIR/zallet-data"
  cp -R "$gdir/zebra-cache" "$WORKDIR/zebra-cache"
  cp -R "$gdir/zallet-data" "$WORKDIR/zallet-data"
  cp "$gdir/encryption-identity.txt" "$WORKDIR/" 2>/dev/null || true
  # shellcheck source=/dev/null
  . "$gdir/meta.env"
  write_zebra_config "$ADDR_A" false
  start_zebra "zebra-serve.log"
  if [ "$(zebra_height 2>/dev/null || echo 0)" != "$TIP" ]; then
    log "golden chain: zebra height mismatch (want $TIP); discarding snapshot"
    stop_zebra; return 1
  fi
  start_zallet "zallet.log"
  if ! WAIT_TIMEOUT=120 wait_until "restored wallet reports tip $TIP" fully_synced "$TIP"; then
    log "golden chain: restored wallet never verified; discarding snapshot"
    stop_zallet; stop_zebra; return 1
  fi
  if [ "$(wallet_db 'SELECT COUNT(*) FROM transparent_received_outputs;' 2>/dev/null)" != "$((TIP - X_COUNT))" ]; then
    log "golden chain: receipt count mismatch; discarding snapshot"
    stop_zallet; stop_zebra; return 1
  fi
  log "golden chain: restored (tip=$TIP watched-mature=$MATURE_TOTAL)"
}

# Snapshot the converged post-setup state. Daemons must be stopped by the
# caller; writes atomically (tmp dir + rename) so an interrupted save never
# leaves a half snapshot under the key.
golden_save() {
  local key gdir tmp
  key=$(golden_key)
  gdir="$GOLDEN_DIR/$key"
  tmp="$GOLDEN_DIR/.tmp.$key.$$"
  rm -rf "$tmp" "$gdir"
  mkdir -p "$tmp"
  cp -R "$WORKDIR/zebra-cache" "$tmp/zebra-cache"
  cp -R "$WORKDIR/zallet-data" "$tmp/zallet-data"
  cp "$WORKDIR/encryption-identity.txt" "$tmp/" 2>/dev/null || true
  {
    printf 'PHASE1_TIP=%s
' "$PHASE1_TIP"
    printf 'C_TIP=%s
' "$C_TIP"
    printf 'X_COUNT=%s
' "$X_COUNT"
    printf 'TIP=%s
' "$TIP"
    printf 'MATURE_A=%s
' "$MATURE_A"
    printf 'MATURE_B=%s
' "$MATURE_B"
    printf 'MATURE_TOTAL=%s
' "$MATURE_TOTAL"
    printf 'ACCOUNT=%s
' "$ACCOUNT"
  } > "$tmp/meta.env"
  mv "$tmp" "$gdir"
  log "golden chain: snapshot saved as $key"
}

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

  if [ "$REGEN_GOLDEN" = 0 ] && golden_restore; then
    return 0
  fi
  rm -rf "$WORKDIR/zebra-cache" "$WORKDIR/zallet-data"
  mkdir -p "$WORKDIR/zallet-data" "$WORKDIR/zebra-cache"

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
  # One coinbase for the spend-poison scenario's derived address: phase 2's
  # blocks mature it for free, so the scenario never mines a maturity window.
  log "funding poison address: mining to height $((PHASE1_TIP + 1)) for $ADDR_C_POISON"
  mine_to "$ADDR_C_POISON" $((PHASE1_TIP + 1))
  C_TIP=$(zebra_height)
  X_COUNT=$((C_TIP - PHASE1_TIP))
  log "funding phase 2: mining to height $((C_TIP + MINE_PHASE_BLOCKS)) for $ADDR_B"
  mine_to "$ADDR_B" $((C_TIP + MINE_PHASE_BLOCKS))
  TIP=$(zebra_height)

  # Coinbase outputs need 100 confirmations and z_listunspent evaluates
  # spendability at the next block (target height TIP+1), so heights
  # 1..TIP-99 are spendable. Addr A owns heights 1..PHASE1_TIP, addr B owns
  # PHASE1_TIP+1..TIP. (Verified empirically at two chain lengths.)
  # MATURE_TOTAL counts only WATCHED mature coinbases: the poison address's
  # X_COUNT coinbases are unwatched by the main wallet and never list here.
  MATURE_A=$((PHASE1_TIP < TIP - 99 ? PHASE1_TIP : TIP - 99))
  MATURE_TOTAL=$((TIP - 99 - X_COUNT))
  MATURE_B=$((MATURE_TOTAL - MATURE_A))
  log "mined boundaries: phase1=$PHASE1_TIP poison=$X_COUNT tip=$TIP mature=(A=$MATURE_A B=$MATURE_B watched=$MATURE_TOTAL)"
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
  WAIT_TIMEOUT=$((TIP * 5)) wait_until "wallet synced to $TIP with all $((TIP - X_COUNT)) watched coinbase receipts ingested" setup_converged \
    || { echo "::error::setup never converged (receipts: $(wallet_db 'SELECT COUNT(*) FROM transparent_received_outputs;' 2>/dev/null))" >&2; exit 1; }

  # Snapshot the converged state for future runs, then hand back the same
  # running stack the non-golden path always produced.
  stop_zallet
  stop_zebra
  golden_save
  write_zebra_config "$ADDR_A" false
  start_zebra "zebra-serve.log"
  start_zallet "zallet.log"
  WAIT_TIMEOUT=120 wait_until "stack back up after snapshot (tip $TIP)" fully_synced "$TIP" \
    || { echo "::error::stack never came back after golden snapshot" >&2; exit 1; }
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

# External-spend watermark poisoning (the production hot-wallet incident):
# spend-check requests are generated one per output, but completing any one
# raises the observed-unspent watermark for EVERY output at the address. A
# newer receipt's near-tip request could therefore mark an older output's
# not-yet-fetched spend range as checked, leaving the spent output listed as
# unspent forever. The repro builds a real on-chain EXTERNAL spend: a signer
# wallet (fixed mnemonic, its own datadir) shields the setup-funded, already
# mature coinbase on its derived transparent address INTO THE MAIN WALLET's
# account (a different seed: if the spend paid the poison seed, the recovery
# below would decrypt it during scanning and record the spend through the
# enhancement path, bypassing the spend-search code under test). The signer
# then deshields a small amount back to the same address: the newer receipt,
# deliberately NON-coinbase so it lists without a maturity window. The signer
# is destroyed, and a fresh recovery of the poison seed must discover a spend
# it cannot decrypt and did not create. Receipt ingestion and spend-search
# servicing each only run at startup or on tip change, so the flow restarts
# zallet deliberately: once to ingest receipts (which queues the spend-search
# requests), once more to service those requests. Runs LAST: it extends the
# chain, invalidating the mature-count math of earlier scenarios.
scenario_spend_poison() {
  # The shielding target, captured while the main wallet is still up.
  local ua_main
  ua_main=$(wallet_rpc z_listaccounts | python3 -c '
import json, sys
for acct in json.load(sys.stdin)["result"]:
    for a in acct.get("addresses", []):
        if a.get("ua"):
            print(a["ua"])
            raise SystemExit
') || true
  stop_zallet
  local signer_dir="$WORKDIR/zallet-signer" fresh_dir="$WORKDIR/zallet-fresh"
  local resp seedfp addr_c opid opstatus
  local s_height b_height poison_tip
  if [ -z "$ua_main" ]; then
    fail "spend-poison: could not capture the main wallet's shielded address"
    start_zallet "zallet.log"
    return
  fi

  # Signer wallet: fixed mnemonic, default account, derived addresses. Every
  # init step is fail-soft: a broken scenario must report red, not abort the
  # whole harness under errexit.
  mkdir -p "$signer_dir"
  {
    "$ZALLET_BIN" -d "$signer_dir" -c "$WORKDIR/zallet.toml" generate-encryption-identity \
      && "$ZALLET_BIN" -d "$signer_dir" -c "$WORKDIR/zallet.toml" init-wallet-encryption \
      && zallet_prompt "$MNEMONIC_POISON" "$ZALLET_BIN" -d "$signer_dir" -c "$WORKDIR/zallet.toml" import-mnemonic \
      && "$ZALLET_BIN" -d "$signer_dir" -c "$WORKDIR/zallet.toml" regtest generate-account-and-miner-address
  } > "$WORKDIR/signer-init.log" 2>&1 || {
    fail "spend-poison: signer wallet init failed (see signer-init.log)"
    start_zallet "zallet.log"
    return
  }

  start_zallet "zallet-signer.log" "$signer_dir"
  seedfp=$(wallet_rpc z_listaccounts | json_field "['result'][0]['seedfp']") || true
  stop_zallet
  addr_c=$(wallet_db_at "$signer_dir" \
    "SELECT cached_transparent_receiver_address FROM addresses
     WHERE key_scope = 0 AND cached_transparent_receiver_address IS NOT NULL
     ORDER BY id LIMIT 1;") || true
  if [ -z "$addr_c" ] || [ -z "$seedfp" ]; then
    fail "spend-poison: signer wallet setup failed (addr='$addr_c' seedfp='$seedfp')"
    start_zallet "zallet.log"
    return
  fi
  # Derivation-drift tripwire: setup funded ADDR_C_POISON on the assumption
  # that this signer derives exactly that address.
  if [ "$addr_c" != "$ADDR_C_POISON" ]; then
    fail "spend-poison: derived address $addr_c != funded constant $ADDR_C_POISON"
    start_zallet "zallet.log"
    return
  fi

  # Signer: sync, then restart once so the startup sweep ingests the setup's
  # X_COUNT coinbase receipts (receipts are written by the enhancement pass,
  # which runs only at startup or on tip change, and this chain is frozen).
  start_zallet "zallet-signer.log" "$signer_dir"
  local signer_tip signer_restarted="" sync_deadline
  signer_tip=$(zebra_height)
  sync_deadline=$((SECONDS + signer_tip * 3))
  until fully_synced "$signer_tip" > /dev/null 2>&1; do
    if [ "$SECONDS" -ge "$sync_deadline" ]; then
      if [ -z "$signer_restarted" ]; then
        signer_restarted=1
        log "signer sync stalled (embedded-index startup race); restarting signer once"
        stop_zallet
        start_zallet "zallet-signer.log" "$signer_dir"
        sync_deadline=$((SECONDS + signer_tip * 3))
      else
        fail "spend-poison: signer never synced"
        stop_zallet; start_zallet "zallet.log"; return
      fi
    fi
    sleep 1
  done
  stop_zallet
  start_zallet "zallet-signer.log" "$signer_dir"
  signer_receipts() {
    [ "$(wallet_db_at "$signer_dir" "SELECT COUNT(*) FROM transparent_received_outputs;" 2>/dev/null)" = "$X_COUNT" ]
  }
  WAIT_TIMEOUT=180 wait_until "signer ingested $X_COUNT coinbase receipts" signer_receipts || {
    fail "spend-poison: signer never ingested its coinbase receipts"
    stop_zallet; start_zallet "zallet.log"; return
  }
  # Receipt ingestion proves the index serves transaction fetches, but the
  # transaction BUILDER also needs treestate queries, which warm up
  # independently after a restart (observed: z_shieldcoinbase stuck in
  # "executing"). Gate on the node view reporting the real tip.
  signer_node_ready() {
    [ "$(wallet_rpc getwalletstatus | json_field "['result']['node_tip']['height']" 2>/dev/null)" = "$signer_tip" ]
  }
  WAIT_TIMEOUT=180 wait_until "signer node view at $signer_tip" signer_node_ready || {
    fail "spend-poison: signer chain view never warmed for shielding"
    stop_zallet; start_zallet "zallet.log"; return
  }

  # Spend X externally (from the poison seed's point of view): shield every
  # matured coinbase on the derived address into the MAIN wallet's account.
  opid=$(wallet_rpc z_shieldcoinbase "[\"$addr_c\", \"$ua_main\"]" 60 | json_field "['result']['opid']") || true
  opstatus=""
  if [ -n "$opid" ]; then
    local op_deadline=$((SECONDS + 240))
    while [ "$SECONDS" -lt "$op_deadline" ]; do
      opstatus=$(wallet_rpc z_getoperationstatus "[[\"$opid\"]]" | json_field "['result'][0]['status']") || true
      case "$opstatus" in success|failed) break ;; esac
      sleep 2
    done
  fi
  local shield_txid="" shield_rawtx="" op_error=""
  if [ "$opstatus" = "success" ]; then
    shield_txid=$(wallet_rpc z_getoperationresult "[[\"$opid\"]]" | json_field "['result'][0]['result']['txid']") || true
  else
    op_error=$(wallet_rpc z_getoperationresult "[[\"$opid\"]]" | json_field "['result'][0].get('error', {}).get('message', '')") || true
    # The operation can report failure for a post-broadcast bookkeeping step
    # (its own store races the wallet's mempool scan of the same tx, which
    # the slow debug-build proving window makes likely). The chain is the
    # ground truth: on this private regtest the mempool can only contain our
    # transaction, so recover the txid from there.
    shield_txid=$(zebra_rpc getrawmempool | json_field "['result'][0]" 2>/dev/null) || true
    if [ "$shield_txid" = "None" ]; then shield_txid=""; fi
    if [ -n "$shield_txid" ]; then
      log "shielding op reported '$opstatus' (${op_error:-no error}); recovered tx $shield_txid from the mempool"
    fi
  fi
  # Capture the raw bytes from the mempool: the mining restart below wipes
  # the mempool, so the transaction must be resubmitted after it.
  if [ -n "$shield_txid" ]; then
    shield_rawtx=$(zebra_rpc getrawtransaction "[\"$shield_txid\", 0]" | json_field "['result']") || true
  fi
  stop_zallet
  if [ -z "$shield_rawtx" ] || [ "$shield_rawtx" = "None" ]; then
    fail "spend-poison: shielding tx not built/captured (status: ${opstatus:-none}, error: ${op_error:-none}, txid: ${shield_txid:-none})"
    start_zallet "zallet.log"
    return
  fi

  # Mine the shielding tx (spend height S), then bury it ten blocks so the
  # recipient's new orchard note clears any default spend-confirmation policy.
  # The signer's role ends here.
  rm -rf "$signer_dir"
  mine_with_tx "$ADDR_B" $(( $(zebra_height) + 1 )) "$shield_rawtx" "$shield_txid"
  s_height=$(zebra_height)
  mine_to "$ADDR_B" $((s_height + 10))

  # The newer receipt on the address: the MAIN wallet (which received the
  # shielded value; the poison seed cannot decrypt that tx by design)
  # deshields a small amount back to the derived address. Non-coinbase, so no
  # maturity window is needed for listing.
  local deshield_tip
  deshield_tip=$(zebra_height)
  start_zallet_warm "zallet.log" "$WORKDIR/zallet-data" "$deshield_tip" || {
    fail "spend-poison: main wallet never caught up for the deshield"
    return
  }
  opid=$(wallet_rpc z_sendmany "[\"$ua_main\", [{\"address\": \"$ADDR_C_POISON\", \"amount\": 0.5}], 1, null, \"AllowRevealedRecipients\"]" 60 | json_field "['result']") || true
  opstatus=""
  if [ -n "$opid" ] && [ "$opid" != "None" ]; then
    local op2_deadline=$((SECONDS + 240))
    while [ "$SECONDS" -lt "$op2_deadline" ]; do
      opstatus=$(wallet_rpc z_getoperationstatus "[[\"$opid\"]]" | json_field "['result'][0]['status']") || true
      case "$opstatus" in success|failed) break ;; esac
      sleep 2
    done
  fi
  local deshield_txid="" deshield_rawtx=""
  if [ "$opstatus" = "success" ]; then
    deshield_txid=$(wallet_rpc z_getoperationresult "[[\"$opid\"]]" | json_field "['result'][0]['result']['txid']") || true
    if [ -n "$deshield_txid" ]; then
      deshield_rawtx=$(zebra_rpc getrawtransaction "[\"$deshield_txid\", 0]" | json_field "['result']") || true
    fi
  fi
  stop_zallet
  if [ "$opstatus" != "success" ] || [ -z "$deshield_rawtx" ] || [ "$deshield_rawtx" = "None" ]; then
    fail "spend-poison: deshield tx not built/captured (status: ${opstatus:-none}, txid: ${deshield_txid:-none})"
    start_zallet "zallet.log"
    return
  fi

  # Mine the deshield (the newer receipt), then a small margin tolerant of
  # zebra dropping a non-finalized block across the serve restart.
  mine_with_tx "$ADDR_B" $(( $(zebra_height) + 1 )) "$deshield_rawtx" "$deshield_txid"
  b_height=$(zebra_height)
  mine_to "$ADDR_B" $((b_height + 3))
  poison_tip=$(zebra_height)

  # Sanity: the chain agrees X is spent; only the deshielded receipt remains.
  local chain_unspent
  chain_unspent=$(zebra_rpc getaddressutxos "[{\"addresses\": [\"$addr_c\"]}]" \
    | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["result"]))') || true
  assert "spend-poison: chain shows only the deshielded receipt unspent (got $chain_unspent)" \
    [ "$chain_unspent" = "1" ]

  # Fresh recovery of the poison seed: it must discover the spend it never made.
  mkdir -p "$fresh_dir"
  {
    "$ZALLET_BIN" -d "$fresh_dir" -c "$WORKDIR/zallet.toml" generate-encryption-identity \
      && "$ZALLET_BIN" -d "$fresh_dir" -c "$WORKDIR/zallet.toml" init-wallet-encryption \
      && zallet_prompt "$MNEMONIC_POISON" "$ZALLET_BIN" -d "$fresh_dir" -c "$WORKDIR/zallet.toml" import-mnemonic
  } > "$WORKDIR/fresh-init.log" 2>&1 || {
    fail "spend-poison: fresh wallet init failed (see fresh-init.log)"
    start_zallet "zallet.log"
    return
  }

  start_zallet "zallet-fresh.log" "$fresh_dir"
  # The embedded chain index reports a bogus tip (0 / finalized floor) until
  # its first sync completes, and historic block-range fetches made in that
  # window can hang outright (observed: Historic(1..N) wedged for 30+
  # minutes). Recovering the account queues exactly such fetches, so wait for
  # the index to report the real tip first.
  node_tip_ready() {
    [ "$(wallet_rpc getwalletstatus | json_field "['result']['node_tip']['height']" 2>/dev/null)" = "$poison_tip" ]
  }
  WAIT_TIMEOUT=180 wait_until "fresh wallet's node view at $poison_tip" node_tip_ready || {
    fail "spend-poison: fresh wallet chain view never warmed"
    stop_zallet; start_zallet "zallet.log"; return
  }
  # A fresh wallet answers RPC before its chain view has warmed up and
  # rejects account recovery with -28 until then; retry that case only.
  local rec_deadline=$((SECONDS + 120))
  resp="(not attempted)"
  while [ "$SECONDS" -lt "$rec_deadline" ]; do
    resp=$(wallet_rpc z_recoveraccounts "[[{\"name\": \"recovered\", \"seedfp\": \"$seedfp\", \"zip32_account_index\": 0, \"birthday_height\": 1}]]" 60) || resp="(rpc failure)"
    contains "$resp" '"account_uuid"' && break
    contains "$resp" '"code":-28' || break
    sleep 3
  done
  contains "$resp" '"account_uuid"' || {
    fail "spend-poison: z_recoveraccounts failed: $resp"
    stop_zallet; start_zallet "zallet.log"; return
  }
  local fresh_restarted=""
  sync_deadline=$((SECONDS + poison_tip * 5))
  until fully_synced "$poison_tip" > /dev/null 2>&1; do
    if [ "$SECONDS" -ge "$sync_deadline" ]; then
      if [ -z "$fresh_restarted" ]; then
        fresh_restarted=1
        log "recovery sync stalled (embedded-index startup race); restarting once"
        stop_zallet
        start_zallet "zallet-fresh.log" "$fresh_dir"
        sync_deadline=$((SECONDS + poison_tip * 5))
      else
        fail "spend-poison: recovery never synced"
        stop_zallet; start_zallet "zallet.log"; return
      fi
    fi
    sleep 1
  done

  # Restart 1: the startup sweep's enhancement pass writes the receipt rows
  # for the derived address and, as it stores them, queues the spend-search
  # requests for the next pass.
  stop_zallet
  start_zallet "zallet-fresh.log" "$fresh_dir"
  fresh_tracked() {
    [ "$(wallet_db_at "$fresh_dir" \
      "SELECT COUNT(*) FROM transparent_received_outputs tro
       JOIN addresses a ON a.id = tro.address_id
       WHERE a.cached_transparent_receiver_address = '$addr_c';" 2>/dev/null)" = "$((X_COUNT + 1))" ]
  }
  WAIT_TIMEOUT=180 wait_until "recovery ingested $((X_COUNT + 1)) receipts" fresh_tracked || true
  local tracked spent
  tracked=$(wallet_db_at "$fresh_dir" \
    "SELECT COUNT(*) FROM transparent_received_outputs tro
     JOIN addresses a ON a.id = tro.address_id
     WHERE a.cached_transparent_receiver_address = '$addr_c';" 2>/dev/null) || true
  assert "spend-poison: recovery tracked $((X_COUNT + 1)) outputs on the derived address (got $tracked)" \
    [ "$tracked" = "$((X_COUNT + 1))" ]

  # Restart 2: a fresh startup sweep whose request batch now contains the
  # queued spend-search requests; this pass must discover the external spend.
  stop_zallet
  start_zallet "zallet-fresh.log" "$fresh_dir"
  spend_recorded() {
    [ "$(wallet_db_at "$fresh_dir" \
      "SELECT COUNT(*) FROM transparent_received_output_spends s
       JOIN transparent_received_outputs tro ON tro.id = s.transparent_received_output_id
       JOIN addresses a ON a.id = tro.address_id
       WHERE a.cached_transparent_receiver_address = '$addr_c';" 2>/dev/null)" = "$X_COUNT" ]
  }
  WAIT_TIMEOUT=180 wait_until "external spend of $X_COUNT outputs recorded" spend_recorded || true
  spent=$(wallet_db_at "$fresh_dir" \
    "SELECT COUNT(*) FROM transparent_received_output_spends s
     JOIN transparent_received_outputs tro ON tro.id = s.transparent_received_output_id
     JOIN addresses a ON a.id = tro.address_id
     WHERE a.cached_transparent_receiver_address = '$addr_c';" 2>/dev/null) || true
  assert "spend-poison: external spend recorded for all $X_COUNT shielded coinbases (got $spent)" \
    [ "$spent" = "$X_COUNT" ]

  # RPC-level contract: the spent coinbases are gone; the deshielded receipt
  # lists immediately (non-coinbase: no maturity requirement).
  local listed
  listed=$(wallet_rpc z_listunspent "[1, 9999999, true, [\"$addr_c\"]]" 60 | python3 -c '
import json, sys
rows = [u for u in json.load(sys.stdin)["result"] if u["pool"] == "transparent"]
print(len(rows))') || true
  assert "spend-poison: z_listunspent shows exactly the deshielded receipt (got $listed)" \
    [ "$listed" = "1" ]

  # Restore the main wallet for any scenario that might follow.
  stop_zallet
  rm -rf "$fresh_dir"
  start_zallet "zallet.log"
}

# A reorg is normal chain behavior the whole stack must survive while live:
# the 2026-07-16 testnet incident (zebra-issue-fork-recovery.md) ended in a
# wedged node precisely because fork recovery is never exercised. Depth 10
# stays well inside zebra's non-finalized window (100 blocks on regtest), so
# invalidateblock can always reach the branch point, while going beyond the
# trivial depth-1 tip swap. The whole reorg runs on the LIVE serving node:
# `generate` needs no miner restart, so the in-memory invalidation state
# cannot be lost to a restart, and zallet stays up throughout (production
# wallets live through reorgs; a crash here is a finding, not harness noise).
scenario_reorg() {
  local depth=10 depth_offline=13
  local old_tip inv_height inv_hash new_tip new_hash pre_count post_count want_count resp

  at_height() { [ "$(zebra_height 2>/dev/null || echo -1)" = "$1" ]; }

  # Every UTXO the wallet lists must exist on the canonical chain: an
  # orphaned output still listed as unspent is the production false-unspent
  # class (hot-wallet balance over-report).
  utxos_canonical() {
    wallet_rpc z_listunspent '[]' 60 | python3 -c '
import json, sys, urllib.request
rows = [u for u in json.load(sys.stdin)["result"] if u["pool"] == "transparent"]
bad = 0
for u in rows:
    req = urllib.request.Request(
        "http://127.0.0.1:'"$ZEBRA_RPC_PORT"'",
        data=json.dumps({"jsonrpc": "2.0", "id": "h",
                         "method": "getrawtransaction",
                         "params": [u["txid"], 1]}).encode(),
        headers={"content-type": "application/json"})
    try:
        resp = json.load(urllib.request.urlopen(req, timeout=15))
        height = (resp.get("result") or {}).get("height", -1)
    except Exception:
        height = -1
    if height is None or height < 0:
        bad += 1
        print("non-canonical utxo: %s:%s" % (u["txid"], u["outindex"]), file=sys.stderr)
sys.exit(1 if bad else 0)'
  }

  old_tip=$(zebra_height)
  inv_height=$((old_tip - depth + 1))
  inv_hash=$(zebra_rpc getblockhash "[$inv_height]" | json_field "['result']")
  pre_count=$(transparent_utxos '[]' 60 | count_lines) || true

  resp=$(zebra_rpc invalidateblock "[\"$inv_hash\"]") || resp="(rpc failure)"
  assert "reorg: invalidateblock accepted" contains "$resp" '"result"'
  wait_until "tip rolled back to $((inv_height - 1))" at_height "$((inv_height - 1))"

  # Replacement branch: depth + 2 blocks, strictly more work than the
  # invalidated branch, paying the serving config's miner address (ADDR_B).
  resp=$(zebra_rpc generate "[$((depth + 2))]" 120) || resp="(rpc failure)"
  assert "reorg: generate mined the replacement branch" contains "$resp" '"result"'
  new_tip=$(zebra_height)
  assert "reorg: tip advanced past the old branch ($old_tip -> $new_tip)" \
    [ "$new_tip" = "$((old_tip + 2))" ]
  new_hash=$(zebra_rpc getblockhash "[$inv_height]" | json_field "['result']")
  assert "reorg: block at height $inv_height replaced" [ "$new_hash" != "$inv_hash" ]

  # The operator's fork tools must not wedge the node: reconsidering the
  # orphaned branch reinstates it as a candidate, but the replacement branch
  # has more work and must remain best.
  resp=$(zebra_rpc reconsiderblock "[\"$inv_hash\"]") || resp="(rpc failure)"
  assert "reorg: reconsiderblock accepted" contains "$resp" '"result"'
  assert "reorg: replacement branch stays best after reconsiderblock" \
    [ "$(zebra_rpc getblockhash "[$inv_height]" | json_field "['result']")" != "$inv_hash" ]

  assert "reorg: zallet survived the live reorg" zallet_alive
  WAIT_TIMEOUT=180 wait_until "wallet synced to post-reorg tip $new_tip" fully_synced "$new_tip" || true
  assert "reorg: wallet fully synced at post-reorg tip $new_tip" fully_synced "$new_tip"

  # The reorg window (top $depth blocks) is entirely immature coinbase on
  # both branches, so the only legitimate listing change is the maturity
  # horizon advancing by the net height gain; those newly mature heights are
  # all phase-2-or-later blocks paying watched ADDR_B (requires
  # TIP - 99 > C_TIP, which MINE_PHASE_BLOCKS=105 guarantees with margin).
  want_count=$((pre_count + new_tip - old_tip))
  post_count=$(transparent_utxos '[]' 60 | count_lines) || true
  assert "reorg: listing tracks the canonical branch ($pre_count -> $post_count, want $want_count)" \
    [ "$post_count" = "$want_count" ]
  assert "reorg: every listed UTXO is on the canonical chain" utxos_canonical
  assert "reorg: getwalletstatus answers within ${RPC_CALL_TIMEOUT}s" \
    wallet_rpc getwalletstatus

  # Restart variant: the reorged state must also be correct from a fresh
  # startup sweep (persistence-level guard; a stale orphaned row surviving
  # restart is the wedge class from the incident notes).
  stop_zallet
  start_zallet "zallet.log"
  WAIT_TIMEOUT=180 wait_until "restarted wallet re-synced to $new_tip" fully_synced "$new_tip" || true
  assert "reorg: restarted wallet fully synced at $new_tip" fully_synced "$new_tip"
  post_count=$(transparent_utxos '[]' 60 | count_lines) || true
  assert "reorg: restarted wallet listing unchanged (got $post_count, want $want_count)" \
    [ "$post_count" = "$want_count" ]
  assert "reorg: restarted wallet lists only canonical UTXOs" utxos_canonical

  # Offline-reorg variant: the incident window from
  # zallet-issue-reorg-blockconflict.md. The wallet misses the fork because it
  # is down when it happens, and comes back while the node still sits at the
  # rolled-back tip: its scan cursor re-pins there, BELOW the fork point,
  # while its stored block rows still reach the old tip ABOVE it. Cursor-based
  # fork detection alone cannot see the reorg from there, so when the
  # replacement branch is then mined under it, the wallet must treat the
  # stored-row hash mismatch as the reorg it is (truncate and rescan) and
  # converge; the pre-fix behavior was a fatal BlockConflict at the first
  # replaced height, repeating on every start.
  #
  # This leg must cut BELOW the first leg's fork point: reconsiderblock above
  # left the original branch as a live candidate, so invalidating only the
  # top of the replacement branch would not roll the tip back (zebra falls
  # over to the reconsidered branch instead). Invalidating a block the two
  # branches share kills both, making the rollback deterministic; hence
  # depth_offline (13) > depth + 2.
  old_tip=$new_tip
  inv_height=$((old_tip - depth_offline + 1))
  inv_hash=$(zebra_rpc getblockhash "[$inv_height]" | json_field "['result']")
  pre_count=$want_count
  stop_zallet
  resp=$(zebra_rpc invalidateblock "[\"$inv_hash\"]") || resp="(rpc failure)"
  assert "reorg-offline: invalidateblock accepted" contains "$resp" '"result"'
  wait_until "tip rolled back to $((inv_height - 1))" at_height "$((inv_height - 1))"

  # Restart against the rolled-back chain so the cursor re-pins below the
  # stored rows. Settle signal: the wallet's own view of the node tip reaches
  # the rolled-back height (the wallet's scan state cannot signal this:
  # stored rows sit above the node tip, so fully_synced never equals it).
  # Deliberately not asserted: how far the boot gets before the replacement
  # branch lands is timing, not contract; convergence below is the contract.
  start_zallet "zallet.log"
  local settle_deadline=$((SECONDS + 60))
  until [ "$(wallet_rpc getwalletstatus 2>/dev/null \
      | json_field "['result']['node_tip']['height']" 2>/dev/null)" = "$((inv_height - 1))" ]; do
    if [ "$SECONDS" -ge "$settle_deadline" ]; then
      log "settle window elapsed without the wallet reporting node tip $((inv_height - 1))"
      break
    fi
    sleep 1
  done

  resp=$(zebra_rpc generate "[$((depth_offline + 2))]" 120) || resp="(rpc failure)"
  assert "reorg-offline: generate mined the replacement branch" contains "$resp" '"result"'
  new_tip=$(zebra_height)
  assert "reorg-offline: tip advanced past the old branch ($old_tip -> $new_tip)" \
    [ "$new_tip" = "$((old_tip + 2))" ]
  assert "reorg-offline: block at height $inv_height replaced" \
    [ "$(zebra_rpc getblockhash "[$inv_height]" | json_field "['result']")" != "$inv_hash" ]

  WAIT_TIMEOUT=180 wait_until "offline-reorged wallet synced to $new_tip" fully_synced "$new_tip" || true
  assert "reorg-offline: zallet survived the reorg it was down for" zallet_alive
  assert "reorg-offline: wallet fully synced at $new_tip" fully_synced "$new_tip"
  want_count=$((pre_count + new_tip - old_tip))
  post_count=$(transparent_utxos '[]' 60 | count_lines) || true
  assert "reorg-offline: listing tracks the canonical branch ($pre_count -> $post_count, want $want_count)" \
    [ "$post_count" = "$want_count" ]
  assert "reorg-offline: every listed UTXO is on the canonical chain" utxos_canonical
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
    log "building zebrad (internal-miner) and zallet (zaino backend), release profile"
    (cd "$REPO_ROOT/zebra" && cargo build --release -p zebrad --features internal-miner)
    (cd "$REPO_ROOT/zallet" && cargo build --release -p zallet --no-default-features --features zaino,rpc-cli,zcashd-import)
  fi

  setup
  if [ "$SETUP_ONLY" = 1 ]; then
    log "--setup-only: golden chain ready; skipping scenarios"
    log "=== results: $PASSES passed, $FAILURES failed ==="
    [ "$FAILURES" = 0 ]
    return
  fi
  run_scenario baseline
  run_scenario dust
  run_scenario filter
  run_scenario union
  run_scenario hang-guard
  run_scenario poison-heal
  run_scenario spend-poison
  # reorg mutates the chain irreversibly; it must stay last.
  run_scenario reorg

  log "=== results: $PASSES passed, $FAILURES failed ==="
  [ "$FAILURES" = 0 ]
}

main

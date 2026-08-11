#!/usr/bin/env bash
# zeronym Nym localnet harness.
#
# Stands up a complete Nym mixnet on localhost -- three mixnodes and one entry
# gateway, no nyxd chain, no nym-api, no credentials -- so real nym-sdk clients
# (see probe/) can be driven against real mixnet behaviour: SURB accounting,
# sender-tag lifecycle, reconnect/rebuild, packet shaping. Adapted from
# upstream scripts/localnet_start.sh at the pinned release, with two changes:
# background processes instead of tmux, and the topology file assembled by
# probe/ with the SDK's own serde types (upstream's build_topology.py emits a
# stale format the current loader cannot read).
#
# Usage: ./localnet.sh up|down|status|smoke|lookup [surbs]|clean|env

set -euo pipefail

# The nym release everything is pinned to. MUST match the tag in
# probe/Cargo.toml. Post-2024.12 per NYM_PLAN D12 (gateway-handshake CVE).
PIN="nym-binaries-v2026.15-bydgoszcz"

NYMNET_HOME="${NYMNET_HOME:-$HOME/.cache/zeronym-nymnet}"
REPO="$NYMNET_HOME/nym"
NODE_BIN="$REPO/target/release/nym-node"
RUN_DIR="$NYMNET_HOME/localnet"
NETWORK_JSON="$RUN_DIR/network.json"
ENV_FILE="$RUN_DIR/harness.env"
ID_PREFIX="zeronym-ln"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE_DIR="$SCRIPT_DIR/probe"
PROBE_BIN="$PROBE_DIR/target/release/zeronym-nymnet-probe"

# name -> index: mix1=1 mix2=2 mix3=3 gateway=4. Ports derive from the index:
# mixnet 1000N, verloc 2000N, http 3000N; the gateway's client websocket is 9000.
NAMES=(mix1 mix2 mix3 gateway)

log() { echo "[nymnet] $*"; }
die() { echo "[nymnet] ERROR: $*" >&2; exit 1; }

node_index() {
  case "$1" in
    mix1) echo 1 ;; mix2) echo 2 ;; mix3) echo 3 ;; gateway) echo 4 ;;
    *) die "unknown node name $1" ;;
  esac
}

ensure_repo() {
  if [ ! -d "$REPO/.git" ]; then
    log "cloning nym at $PIN into $REPO (shallow)..."
    mkdir -p "$NYMNET_HOME"
    git clone --depth 1 --branch "$PIN" https://github.com/nymtech/nym.git "$REPO"
  fi
  local at
  at=$(git -C "$REPO" describe --tags --exact-match 2>/dev/null || echo unknown)
  if [ "$at" != "$PIN" ]; then
    log "WARNING: $REPO is at '$at', expected '$PIN'"
  fi
}

ensure_node_bin() {
  if [ ! -x "$NODE_BIN" ]; then
    log "building nym-node (release; first build takes a few minutes)..."
    (cd "$REPO" && cargo build --release --bin nym-node)
  fi
}

ensure_probe_bin() {
  if [ ! -x "$PROBE_BIN" ]; then
    log "building probe (release; first build takes several minutes)..."
    # PROTOC must be unset: the probe links the shim/hub crates, whose
    # zaino-proto dep would otherwise try to regenerate protos in the
    # read-only vendored tree (see NYM_PLAN.md, build notes).
    (cd "$PROBE_DIR" && env -u PROTOC -u protoc cargo build --release)
  fi
}

init_node() {
  local name="$1" i id
  i=$(node_index "$name")
  id="$ID_PREFIX-$name"
  [ -f "$HOME/.nym/nym-nodes/$id/config/config.toml" ] && return 0
  log "initialising $id..."
  local mode_args=()
  if [ "$name" = gateway ]; then
    mode_args=(--mode entry-gateway --entry-bind-address "127.0.0.1:9000")
  else
    mode_args=(--mode mixnode)
  fi
  "$NODE_BIN" run --id "$id" --init-only --local \
    --public-ips 127.0.0.1 \
    --mixnet-bind-address "127.0.0.1:1000$i" \
    --verloc-bind-address "127.0.0.1:2000$i" \
    --http-bind-address "127.0.0.1:3000$i" \
    --http-access-token=zeronym \
    "${mode_args[@]}" \
    --output json >/dev/null
}

start_node() {
  local name="$1" id
  id="$ID_PREFIX-$name"
  log "starting $id..."
  nohup "$NODE_BIN" run --id "$id" --local \
    >"$RUN_DIR/$name.log" 2>&1 &
  echo $! >"$RUN_DIR/$name.pid"
}

wait_port() {
  local port="$1" label="$2" tries=60
  while ! nc -z 127.0.0.1 "$port" 2>/dev/null; do
    tries=$((tries - 1))
    [ "$tries" -gt 0 ] || die "$label did not open port $port"
    sleep 1
  done
}

node_running() {
  local pid_file="$RUN_DIR/$1.pid"
  [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null
}

cmd_up() {
  for name in "${NAMES[@]}"; do
    node_running "$name" && die "$name already running; './localnet.sh down' first"
  done
  ensure_repo
  ensure_node_bin
  ensure_probe_bin
  mkdir -p "$RUN_DIR"

  for name in "${NAMES[@]}"; do
    init_node "$name"
    start_node "$name"
  done

  for name in "${NAMES[@]}"; do
    wait_port "3000$(node_index "$name")" "$name http api"
  done
  wait_port 9000 "gateway client websocket"

  # Snapshot each node's self-reported keys (identity, and the sphinx key of
  # the CURRENT rotation) and assemble the topology from them.
  for name in "${NAMES[@]}"; do
    curl -sf "http://127.0.0.1:3000$(node_index "$name")/api/v1/host-information" \
      >"$RUN_DIR/$name.host.json" || die "no host-information from $name"
  done
  "$PROBE_BIN" topology "$RUN_DIR" "$NETWORK_JSON"

  local gateway_id rotation
  gateway_id=$(jq -r .data.keys.ed25519_identity "$RUN_DIR/gateway.host.json")
  rotation=$(jq -r .data.keys.primary_x25519_sphinx_key.rotation_id "$RUN_DIR/gateway.host.json")
  cat >"$ENV_FILE" <<EOF
NYMNET_NETWORK_JSON=$NETWORK_JSON
NYMNET_GATEWAY_ID=$gateway_id
NYMNET_KEY_ROTATION=$rotation
EOF

  log "localnet is up"
  log "  topology:  $NETWORK_JSON (sphinx rotation $rotation)"
  log "  gateway:   $gateway_id (ws://127.0.0.1:9000)"
  log "  logs/pids: $RUN_DIR"
  log "note: sphinx keys rotate roughly daily; if clients stop decrypting"
  log "after a long-running session, cycle down/up to refresh the topology."
}

cmd_down() {
  local killed=0
  for name in "${NAMES[@]}"; do
    local pid_file="$RUN_DIR/$name.pid"
    if [ -f "$pid_file" ]; then
      local pid
      pid=$(cat "$pid_file")
      if kill "$pid" 2>/dev/null; then
        killed=$((killed + 1))
      fi
      rm -f "$pid_file"
    fi
  done
  log "stopped $killed node(s)"
}

cmd_status() {
  for name in "${NAMES[@]}"; do
    if node_running "$name"; then
      echo "$name: running (pid $(cat "$RUN_DIR/$name.pid"), http 3000$(node_index "$name"))"
    else
      echo "$name: stopped"
    fi
  done
  [ -f "$NETWORK_JSON" ] && echo "topology: $NETWORK_JSON"
}

cmd_clean() {
  cmd_down
  rm -rf "$RUN_DIR"
  for name in "${NAMES[@]}"; do
    rm -rf "$HOME/.nym/nym-nodes/$ID_PREFIX-$name"
  done
  log "removed run dir and node configs (repo and build cache kept)"
}

cmd_env() {
  [ -f "$ENV_FILE" ] || die "no harness.env; run './localnet.sh up' first"
  cat "$ENV_FILE"
}

require_up() {
  [ -f "$NETWORK_JSON" ] || die "no topology; run './localnet.sh up' first"
  node_running gateway || die "gateway is not running; run './localnet.sh up' first"
  ensure_probe_bin
}

cmd_smoke() {
  require_up
  "$PROBE_BIN" smoke "$NETWORK_JSON"
}

cmd_lookup() {
  require_up
  "$PROBE_BIN" lookup "$NETWORK_JSON" "${1:-13}"
}

cmd_wire() {
  require_up
  "$PROBE_BIN" wire "$NETWORK_JSON" "$SCRIPT_DIR/../shim/tests/fixtures/wire_v1_vectors.bin"
}

cmd_e2e() {
  require_up
  "$PROBE_BIN" e2e "$NETWORK_JSON"
}

case "${1:-}" in
  up) cmd_up ;;
  down) cmd_down ;;
  status) cmd_status ;;
  smoke) cmd_smoke ;;
  lookup) cmd_lookup "${2:-}" ;;
  wire) cmd_wire ;;
  e2e) cmd_e2e ;;
  clean) cmd_clean ;;
  env) cmd_env ;;
  *) die "usage: $0 up|down|status|smoke|lookup [surbs]|wire|e2e|clean|env" ;;
esac

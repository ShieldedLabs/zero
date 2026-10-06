#!/usr/bin/env bash
# [zero] @claude Build the librustzcash crates for a WebAssembly target.
#
# The workspace itself cannot be built for wasm directly: `cargo build --workspace`
# pulls in dev-dependencies and `zcash_client_sqlite`, neither of which cross-compile.
# Instead this builds each supported crate/feature combination through a synthetic
# consumer crate, which is also how upstream CI exercises `wasm32-wasip1`
# (.github/workflows/ci.yml, job `build-nodefault`).
#
# Usage:
#   scripts/wasm-check.sh [target]
#
# `target` defaults to wasm32-unknown-unknown (the browser target). wasm32-wasip1
# is also supported.
#
# See WASM.md for what is expected to pass and what is known not to.

set -euo pipefail

TARGET="${1:-wasm32-unknown-unknown}"
CRATES="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

case "$TARGET" in
  wasm32-unknown-unknown)
    # getrandom 0.3 has no default backend for OS-less wasm; both the feature and
    # the cfg are required. getrandom 0.2 and 0.4 only need the feature.
    export RUSTFLAGS="${RUSTFLAGS:-} --cfg getrandom_backend=\"wasm_js\""
    RNG_DEPS='getrandom = { version = "0.2", features = ["js"] }
getrandom_03 = { package = "getrandom", version = "0.3", features = ["wasm_js"] }
getrandom_04 = { package = "getrandom", version = "0.4", features = ["wasm_js"] }'
    ;;
  wasm32-wasip1)
    RNG_DEPS=''
    ;;
  *)
    echo "unsupported target: $TARGET" >&2
    exit 2
    ;;
esac

rustup target add "$TARGET" >/dev/null

# name<TAB>dependency lines
probe() {
  local name="$1" deps="$2"
  local dir="$WORK/$name"
  mkdir -p "$dir/src"
  echo 'pub fn probe() {}' > "$dir/src/lib.rs"
  cp "$CRATES/rust-toolchain.toml" "$dir/"
  {
    echo '[package]'
    echo "name = \"$name\""
    echo 'version = "0.0.0"'
    echo 'edition = "2021"'
    echo
    echo '[dependencies]'
    echo "$deps"
    [ -n "$RNG_DEPS" ] && echo "$RNG_DEPS"
  } > "$dir/Cargo.toml"

  printf '%-28s ' "$name"
  if (cd "$dir" && cargo build --quiet --target "$TARGET" 2>"$dir/err"); then
    echo "ok"
  else
    echo "FAILED"
    sed 's/^/    /' "$dir/err" | head -20
    FAILURES=$((FAILURES + 1))
  fi
}

FAILURES=0
echo "librustzcash -> $TARGET"
echo

probe core "\
zcash_primitives = { path = \"$CRATES/zcash_primitives\", default-features = false }
zcash_keys = { path = \"$CRATES/zcash_keys\", default-features = false, features = [\"sapling\", \"orchard\", \"transparent-inputs\"] }
zcash_address = { path = \"$CRATES/components/zcash_address\", default-features = false }
zcash_protocol = { path = \"$CRATES/components/zcash_protocol\", default-features = false }
pczt = { path = \"$CRATES/pczt\", default-features = false }"

probe backend "\
zcash_client_backend = { path = \"$CRATES/zcash_client_backend\", features = [\"transparent-inputs\", \"pczt\"] }"

# tonic compiles only because its `transport` feature stays off; the wasm host has
# to supply its own transport (grpc-web).
probe backend-tonic-sync "\
zcash_client_backend = { path = \"$CRATES/zcash_client_backend\", features = [\"lightwalletd-tonic\", \"sync\"] }"

# `prover` rather than `local-prover`: see WASM.md.
probe proofs "\
zcash_proofs = { path = \"$CRATES/zcash_proofs\", default-features = false, features = [\"prover\"] }"

# On wasm32-unknown-unknown `rusqlite` links against `sqlite-wasm-rs` rather than
# building SQLite from C, so this needs no sysroot. `uuid` needs its own randomness
# feature, exactly as `getrandom` does.
probe sqlite "\
zcash_client_sqlite = { path = \"$CRATES/zcash_client_sqlite\", default-features = false, features = [\"orchard\", \"transparent-inputs\"] }
uuid = { version = \"1\", features = [\"js\"] }"

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "all probes passed"
else
  echo "$FAILURES probe(s) failed"
  exit 1
fi

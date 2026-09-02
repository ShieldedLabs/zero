#!/usr/bin/env bash
# [zero] @claude Can the wasm build be made multi-threaded?
#
# Threads are the only lever that moves both Orchard proving numbers (see WASM.md), but
# they cost a second toolchain: `wasm-bindgen-rayon` needs `std` rebuilt with
# `+atomics,+bulk-memory,+mutable-globals`, which means `-Z build-std`, which is
# nightly-only and cannot be reconciled with this workspace's stable `rust-toolchain.toml`
# pin. A threaded wallet is therefore a separate crate on a separate toolchain.
#
# This script answers the question that gates that decision: does the dependency stack
# *build* that way at all? Historically it has not always — a `std` refactor broke the
# atomics build outright in August 2025 (rust-lang/rust#145101) — and the pieces most
# likely to break are the ones that are not pure Rust: `sqlite-wasm-rs` ships prebuilt
# SQLite, and a non-atomics object cannot be linked into an atomics module.
#
# It does not answer whether threads *work*: `wasm-bindgen-rayon` supports only
# `--target web`, so its thread pool needs a browser with cross-origin isolation
# (COOP/COEP) and Web Workers. Measuring a speedup needs a browser driver in CI.
#
# Usage:
#   scripts/wasm-atomics-check.sh [toolchain]     # default: nightly

set -euo pipefail

TOOLCHAIN="${1:-nightly}"
CRATES="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if ! rustup component list --toolchain "$TOOLCHAIN" 2>/dev/null | grep -q '^rust-src (installed)'; then
  echo "the $TOOLCHAIN toolchain needs rust-src: rustup component add rust-src --toolchain $TOOLCHAIN" >&2
  exit 2
fi

mkdir -p "$WORK/src"
cat > "$WORK/src/lib.rs" <<'RUST'
// The threaded build must cover the whole wallet, not just the part that would use the
// threads: everything links into one module.
pub use wasm_bindgen_rayon::init_thread_pool;

pub fn build_proving_key() -> orchard::circuit::ProvingKey {
    orchard::circuit::ProvingKey::build(orchard::circuit::OrchardCircuitVersion::FixedPostNu6_2)
}
RUST

cat > "$WORK/Cargo.toml" <<EOF
[package]
name = "atomics-probe"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
# \`multicore\` is the whole point: it is what \`halo2_proofs\` refuses to compile for
# wasm32 *without* atomics, and what parallelises proving with them.
orchard = { version = "0.15", features = ["circuit", "multicore"] }
rayon = "1"
wasm-bindgen-rayon = "1.3"
zcash_client_sqlite = { path = "$CRATES/zcash_client_sqlite", default-features = false, features = ["orchard", "transparent-inputs"] }
uuid = { version = "1", features = ["js"] }
getrandom = { version = "0.2", features = ["js"] }
getrandom_03 = { package = "getrandom", version = "0.3", features = ["wasm_js"] }
EOF

echo "librustzcash + rayon -> wasm32-unknown-unknown with atomics ($TOOLCHAIN)"
echo

cd "$WORK"
RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals --cfg getrandom_backend="wasm_js"' \
  cargo "+$TOOLCHAIN" build --quiet --target wasm32-unknown-unknown -Z build-std=std,panic_abort

echo "atomics build ok — threads are reachable on this toolchain"
echo "note: this proves it compiles, not that the thread pool runs; see the header."

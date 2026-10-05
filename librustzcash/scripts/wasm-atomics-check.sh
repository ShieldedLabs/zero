#!/usr/bin/env bash
# [zero] @claude Does the threaded wasm build still link?
#
# Builds `zero-wasm-threads`, whose own `rust-toolchain.toml` and `.cargo/config.toml`
# carry everything a threaded build needs: a pinned nightly, `std` rebuilt with atomics,
# the linker arguments that make memory shared, and SQLite compiled for that memory.
# Building its tests links the whole wallet stack into one shared-memory module, which is
# where a threaded build fails: wasm-ld refuses `--shared-memory` if any object, Rust or C,
# was built without atomics.
#
# This proves the build. Whether the pool runs, and how fast it proves, needs a browser:
#
#   cd zero-wasm-threads && wasm-pack test --headless --chrome --release
#
# Usage:
#   scripts/wasm-atomics-check.sh

set -euo pipefail

CRATE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../zero-wasm-threads" && pwd)"

echo "zero-wasm-threads -> wasm32-unknown-unknown with shared memory"
cd "$CRATE"
# rustc warns once per crate that `atomics` is an unstable target feature; that is
# expected here and drowns everything else, so it is filtered out of the output.
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT
if ! cargo build --quiet --release --target wasm32-unknown-unknown --tests >"$LOG" 2>&1; then
  grep -v -E 'unstable feature specified for|not stably supported|generated 1 warning' "$LOG" >&2
  echo "threaded build FAILED" >&2
  exit 1
fi

echo "threaded build ok"

# `schemerz-rusqlite` (Zero-local build)

This is a `[zero]`-local build of [`schemerz-rusqlite`], vendored from the published
0.370.0 release.

Two changes from what was published:

- **`Cargo.toml`:** the `rusqlite` requirement is widened from `0.37` to
  `>=0.37, <0.40`. This is the reason the crate is vendored at all.
- **`src/lib.rs`:** one trailing comma removed from the `test_schemerz_adapter!`
  invocation in the test module. See "The test suite" below. No non-test code is
  modified.

## Why

Upstream pins one `rusqlite` minor per release — 0.350.0 for rusqlite 0.35, 0.360.0 for
0.36, 0.370.0 for 0.37 — and the newest release is 0.370.0. `zcash_client_sqlite` needs
to move to `rusqlite 0.38` or later, because that is where `rusqlite` gained
`wasm32-unknown-unknown` support (it substitutes `sqlite-wasm-rs` for `libsqlite3-sys` on
that target). No published `schemerz-rusqlite` permits that, so the workspace could not
take the bump.

The adapter is 230 lines and touches only `Connection`, `Transaction` and `Error`, none of
which changed across 0.37 to 0.39, so it compiles against the whole range unmodified. A
single build spanning the range lets `zcash_client_sqlite` move without a lockstep release
here.

## When to delete this

When `zcash/schemerz` publishes a release supporting the `rusqlite` version this workspace
uses. At that point, drop this directory, remove it from `[workspace] members`, and point
`schemerz-rusqlite` in the workspace `Cargo.toml` back at the registry.

[`schemerz-rusqlite`]: https://github.com/zcash/schemerz

## The test suite

The published 0.370.0 runs **zero** adapter tests, and reports success while doing so.
`test_schemerz_adapter!` has a three-argument arm that expands to the five real tests, and
a variadic arm `($setup, $constructor, $id_ter, $($test_fn:ident),* $(,)*)`. The published
call site passes three arguments *with a trailing comma*, which the three-argument arm
cannot match; it falls through to the variadic arm, where `$(,)*` absorbs the comma, the
test-name list matches empty, and the macro expands to nothing. Nothing warns — the suite
simply reports "running 0 tests".

Dropping that comma restores all five. They pass against rusqlite 0.37, 0.38 and 0.39,
which is what licenses the widened requirement above. This is an upstream bug and is worth
reporting to `zcash/schemerz`.

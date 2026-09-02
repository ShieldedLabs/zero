//! [zero] @claude What proving costs on wasm.
//!
//! Sapling and Orchard are not comparable here. Sapling's Groth16 parameters are
//! downloaded once (see `tests/params.rs`); Orchard's Halo2 proving key has no trusted
//! setup and is **computed**, so every session that wants to spend Orchard funds pays for
//! it before it can start. On a phone or a laptop tab that cost is the architecture
//! decision, which is why it is measured first.
//!
//! These are wall-clock numbers under Node on one machine, not a benchmark suite. They are
//! here to answer "is this viable in a browser at all" and to fail loudly if it stops
//! being so.
//!
//! Observed on an M-series Mac, single-threaded (`multicore` cannot be enabled on wasm
//! without atomics):
//!
//! | operation                            | time    |
//! |--------------------------------------|---------|
//! | Sapling parameters, verify and parse | 0.4 s   |
//! | Orchard `VerifyingKey::build`        | 7.2 s   |
//! | Orchard `ProvingKey::build`          | 8.9 s   |
//! | Orchard 2-action bundle proof        | 12.1 s  |
//!
//! The Sapling row is from `tests/params.rs` and is included for contrast: a 51 MiB
//! download parses in under half a second, while Orchard's key — which cannot be
//! downloaded at all — costs nine seconds of arithmetic every session.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use orchard::circuit::{OrchardCircuitVersion, ProvingKey, VerifyingKey};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

/// Times `f`, returning its result and the elapsed milliseconds.
fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let started = js_sys::Date::now();
    let value = f();
    (value, js_sys::Date::now() - started)
}

/// Building the Orchard verifying key.
///
/// Cheaper than the proving key, and needed by anything that only verifies.
#[wasm_bindgen_test]
fn orchard_verifying_key_build() {
    let (_vk, elapsed) = timed(|| VerifyingKey::build(OrchardCircuitVersion::PostNu6_3));
    console_log!("orchard VerifyingKey::build: {elapsed:.0} ms");
    // Observed at 7.2 s. The ceiling guards against a pathological regression, not
    // against drift; see the module docs for why these are not a benchmark suite.
    assert!(elapsed < 120_000.0, "took {elapsed:.0} ms");
}

/// Building the Orchard proving key — the number that decides the architecture.
///
/// There is nothing to download and nothing to cache across sessions from Rust's side:
/// `ProvingKey::build` runs the whole key generation, single-threaded here because
/// `multicore` cannot be enabled on wasm without atomics. If this is slow, a browser
/// wallet cannot simply prove on demand — it has to build the key ahead of time in a
/// worker, or move proving off-device entirely.
#[wasm_bindgen_test]
fn orchard_proving_key_build() {
    let (_pk, elapsed) = timed(|| ProvingKey::build(OrchardCircuitVersion::PostNu6_3));
    console_log!("orchard ProvingKey::build: {elapsed:.0} ms");
    // Observed at 8.9 s.
    assert!(elapsed < 120_000.0, "took {elapsed:.0} ms");
}

/// Proving one Orchard bundle.
///
/// An output-only bundle, which the builder pads to the default two actions — the smallest
/// realistic unit of Orchard proving work, and the shape of a shielding transaction. The
/// proving key is built inside the measured section's setup rather than shared, because in
/// a browser it would be: there is no process to keep it warm across page loads.
#[wasm_bindgen_test]
fn orchard_bundle_proof() {
    use orchard::{
        builder::{Builder, BundleType},
        bundle::{BundleVersion, Flags},
        keys::{FullViewingKey, Scope, SpendingKey},
        tree::Anchor,
        value::NoteValue,
    };
    use rand_chacha::{rand_core::SeedableRng, ChaChaRng};

    let mut rng = ChaChaRng::seed_from_u64(0);
    let sk = SpendingKey::from_bytes([1u8; 32]).unwrap();
    let recipient = FullViewingKey::from(&sk).address_at(0u32, Scope::External);

    // `orchard_v2` with `SPENDS_DISABLED` is the shielding shape: real outputs, dummy
    // spends, cross-address transfers permitted. `orchard_v3` disallows cross-address
    // transfers, so ordinary outputs cannot be constructed under it and a bundle would
    // need a real spend to take change from — more setup for the same circuit work.
    let (pk, pk_ms) = timed(|| ProvingKey::build(OrchardCircuitVersion::FixedPostNu6_2));

    let mut builder = Builder::new(
        BundleType::Transactional {
            bundle_required: false,
            pad_to_minimum: None,
        },
        BundleVersion::orchard_v2(),
        Flags::SPENDS_DISABLED,
        Anchor::empty_tree(),
    )
    .expect("the bundle type and flags are consistent");
    builder
        .add_output(None, recipient, NoteValue::from_raw(100_000), [0u8; 512])
        .expect("outputs are enabled");
    let (unauthorized, _meta) = builder
        .build::<i64>(&mut rng)
        .expect("the bundle builds")
        .expect("the bundle is not empty");

    let (proved, proof_ms) = timed(|| {
        unauthorized
            .create_proof(&pk, &mut rng)
            .expect("the proof is created")
    });
    let _ = proved;

    console_log!(
        "orchard 2-action bundle: ProvingKey::build {pk_ms:.0} ms, proof {proof_ms:.0} ms"
    );
    // Observed at 12.1 s for two actions.
    assert!(proof_ms < 180_000.0, "proving took {proof_ms:.0} ms");
}

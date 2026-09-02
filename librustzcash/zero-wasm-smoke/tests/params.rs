//! [zero] @claude Loading real Sapling parameters inside a wasm host.
//!
//! Set `ZCASH_PARAMS_DIR` at build time to a directory holding `sapling-spend.params` and
//! `sapling-output.params`; without it the parameter tests skip rather than fail, since
//! the files are a 51 MiB download that does not belong in the repository:
//!
//! ```sh
//! ZCASH_PARAMS_DIR=$HOME/.zcash-params \
//!   RUSTFLAGS='--cfg getrandom_backend="wasm_js"' wasm-pack test --node
//! ```
//!
//! Reading them through Node's `fs` is a stand-in for the `fetch` a browser would do. What
//! matters is that the bytes arrive from outside the module, which is the shape of the
//! real thing; where they came from is not.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use wasm_bindgen::prelude::*;
use wasm_bindgen_test::{console_log, wasm_bindgen_test};
use zero_wasm_smoke::params::{
    verify_sapling_parameters, ParamsError, SAPLING_OUTPUT_LEN, SAPLING_SPEND_LEN,
};

#[wasm_bindgen(module = "fs")]
extern "C" {
    #[wasm_bindgen(js_name = readFileSync)]
    fn read_file_sync(path: &str) -> Vec<u8>;
    #[wasm_bindgen(js_name = existsSync)]
    fn exists_sync(path: &str) -> bool;
}

/// The directory named by `ZCASH_PARAMS_DIR` at build time, if it was set.
const PARAMS_DIR: Option<&str> = option_env!("ZCASH_PARAMS_DIR");

/// Reads both parameter files, or `None` if they are not available.
fn read_params() -> Option<(Vec<u8>, Vec<u8>)> {
    let dir = PARAMS_DIR?;
    let spend = format!("{dir}/sapling-spend.params");
    let output = format!("{dir}/sapling-output.params");
    if !(exists_sync(&spend) && exists_sync(&output)) {
        return None;
    }
    Some((read_file_sync(&spend), read_file_sync(&output)))
}

/// Real Sapling parameters validate and build a prover, inside wasm.
///
/// This is the whole of parameter delivery: 51 MiB crosses into the module from the host,
/// gets hashed, and gets parsed into Groth16 proving keys. If wasm could not do it, a
/// browser wallet could not spend.
#[wasm_bindgen_test]
fn real_parameters_load() {
    // Distinguish "no parameters configured" from "configured and missing". The first is
    // a skip; the second is a mistake that should not pass quietly.
    let Some(dir) = PARAMS_DIR else {
        console_log!("SKIP: ZCASH_PARAMS_DIR was not set at build time");
        return;
    };
    let Some((spend, output)) = read_params() else {
        panic!("ZCASH_PARAMS_DIR is set to {dir}, but the parameter files are not there");
    };

    assert_eq!(spend.len(), SAPLING_SPEND_LEN);
    assert_eq!(output.len(), SAPLING_OUTPUT_LEN);

    let started = js_sys::Date::now();
    let prover = verify_sapling_parameters(&spend, &output).expect("the real parameters verify");
    let elapsed = js_sys::Date::now() - started;

    // `verifying_keys` touches both parsed parameter sets, so it fails if either was
    // decoded into something unusable rather than merely allocated.
    let _keys = prover.verifying_keys();

    console_log!("verified and parsed 51 MiB of Sapling parameters in {elapsed:.0} ms");

    // Observed at 382 ms under Node on an M-series Mac (SHA-256 over 51 MiB plus Groth16
    // deserialisation of both parameter sets). The ceiling is deliberately far above that:
    // this is a guard against a change that makes parameter loading pathological, not a
    // benchmark. The useful conclusion is that parsing is not what makes parameters
    // expensive in a browser — the 47 MiB download is.
    assert!(
        elapsed < 30_000.0,
        "loading the Sapling parameters took {elapsed:.0} ms"
    );
}

/// Truncated parameters are rejected, not fatal.
///
/// A browser caches this download; caches get evicted mid-write and serve short reads.
/// `LocalTxProver::from_bytes` would panic and take the wasm module with it.
#[wasm_bindgen_test]
fn truncated_parameters_are_rejected() {
    // `LocalTxProver` is not `Debug`, so match rather than `unwrap_err`.
    let Err(err) = verify_sapling_parameters(&[0u8; 16], &[0u8; 16]) else {
        panic!("16 zero bytes are not the Sapling parameters");
    };
    assert_eq!(
        err,
        ParamsError::WrongLength {
            which: "spend",
            got: 16,
            expected: SAPLING_SPEND_LEN,
        }
    );
}

/// Right-sized but wrong bytes are rejected on the hash.
///
/// Allocating 51 MiB of zeroes only to reject it is the point: this is the case a length
/// check alone would wave through.
#[wasm_bindgen_test]
fn corrupted_parameters_are_rejected() {
    let spend = vec![0u8; SAPLING_SPEND_LEN];
    let output = vec![0u8; SAPLING_OUTPUT_LEN];
    let Err(err) = verify_sapling_parameters(&spend, &output) else {
        panic!("zeroed parameters of the right length are still not the parameters");
    };
    assert_eq!(err, ParamsError::WrongHash { which: "spend" });
}

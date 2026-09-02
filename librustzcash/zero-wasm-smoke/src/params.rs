//! [zero] @claude Sapling parameter delivery for a wasm host.
//!
//! A browser cannot read the parameters off disk, so it fetches them — about 47 MiB for
//! the spend parameters and 3.5 MiB for the output ones — and hands the bytes to
//! [`LocalTxProver::from_bytes`]. That download is far too large to repeat per session, so
//! it belongs in the Cache API or IndexedDB, which makes *validating what came back* this
//! module's reason to exist:
//!
//! - [`LocalTxProver::from_bytes`] **panics** on parameters that do not have the expected
//!   hashes. In a browser a panic aborts the wasm module and takes the wallet with it, and
//!   the input here is a network download that has then sat in a cache the user's browser
//!   is free to evict, truncate, or serve stale. That is not a panic-worthy input.
//! - So check first. [`verify_sapling_parameters`] returns an error where `from_bytes`
//!   would abort, which lets a wallet discard the cache entry and re-fetch instead of
//!   dying.
//!
//! Hashing 51 MiB is not free, but it is the same order as the parsing that follows it and
//! trivial next to the download it guards.

use sha2::{Digest, Sha256};
use zcash_proofs::prover::LocalTxProver;

/// Expected size of `sapling-spend.params`, in bytes.
pub const SAPLING_SPEND_LEN: usize = 47_958_396;

/// Expected size of `sapling-output.params`, in bytes.
pub const SAPLING_OUTPUT_LEN: usize = 3_592_860;

/// SHA-256 of `sapling-spend.params`.
///
/// `8e48ffd23abb3a5fd9c5589204f32d9c31285a04b78096ba40a79b75677efc13`
pub const SAPLING_SPEND_SHA256: [u8; 32] = [
    0x8e, 0x48, 0xff, 0xd2, 0x3a, 0xbb, 0x3a, 0x5f, 0xd9, 0xc5, 0x58, 0x92, 0x04, 0xf3, 0x2d, 0x9c,
    0x31, 0x28, 0x5a, 0x04, 0xb7, 0x80, 0x96, 0xba, 0x40, 0xa7, 0x9b, 0x75, 0x67, 0x7e, 0xfc, 0x13,
];

/// SHA-256 of `sapling-output.params`.
///
/// `2f0ebbcbb9bb0bcffe95a397e7eba89c29eb4dde6191c339db88570e3f3fb0e4`
pub const SAPLING_OUTPUT_SHA256: [u8; 32] = [
    0x2f, 0x0e, 0xbb, 0xcb, 0xb9, 0xbb, 0x0b, 0xcf, 0xfe, 0x95, 0xa3, 0x97, 0xe7, 0xeb, 0xa8, 0x9c,
    0x29, 0xeb, 0x4d, 0xde, 0x61, 0x91, 0xc3, 0x39, 0xdb, 0x88, 0x57, 0x0e, 0x3f, 0x3f, 0xb0, 0xe4,
];

/// Why a set of candidate parameter bytes was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamsError {
    /// The blob is not the expected length. Almost always a truncated download or a
    /// partially-evicted cache entry, so it is worth distinguishing from a hash mismatch:
    /// this one is retryable without suspecting the source.
    WrongLength {
        /// Which parameter file.
        which: &'static str,
        /// The length that arrived.
        got: usize,
        /// The length that was expected.
        expected: usize,
    },
    /// The blob is the right length but hashes to something else.
    WrongHash {
        /// Which parameter file.
        which: &'static str,
    },
}

impl core::fmt::Display for ParamsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ParamsError::WrongLength {
                which,
                got,
                expected,
            } => write!(f, "{which} parameters are {got} bytes, expected {expected}"),
            ParamsError::WrongHash { which } => {
                write!(f, "{which} parameters do not have the expected SHA-256")
            }
        }
    }
}

impl std::error::Error for ParamsError {}

fn check(which: &'static str, bytes: &[u8], len: usize, hash: [u8; 32]) -> Result<(), ParamsError> {
    if bytes.len() != len {
        return Err(ParamsError::WrongLength {
            which,
            got: bytes.len(),
            expected: len,
        });
    }
    if Sha256::digest(bytes).as_slice() != hash {
        return Err(ParamsError::WrongHash { which });
    }
    Ok(())
}

/// Validates fetched Sapling parameters and builds a prover from them.
///
/// This is the wasm equivalent of `LocalTxProver::with_default_location`, for a host whose
/// "location" is a network fetch backed by a cache. On `Err` the caller should drop
/// whatever it cached and fetch again; calling [`LocalTxProver::from_bytes`] with the same
/// bytes would panic instead.
pub fn verify_sapling_parameters(
    spend: &[u8],
    output: &[u8],
) -> Result<LocalTxProver, ParamsError> {
    check("spend", spend, SAPLING_SPEND_LEN, SAPLING_SPEND_SHA256)?;
    check("output", output, SAPLING_OUTPUT_LEN, SAPLING_OUTPUT_SHA256)?;
    Ok(LocalTxProver::from_bytes(spend, output))
}

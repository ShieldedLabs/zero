//! What the hub learns about a submission from its bytes alone, and the one
//! check that makes a junk submission cost its sender something: a Halo2 proof.
//!
//! Admission used to read the bytes for telemetry only (REVIEW #5), so junk was
//! free: 1024 payloads of 64 bytes filled the queue's entry cap for a whole
//! flush window, and forged parseable bytes did the same job. This module sorts
//! every submission into one of three **lanes**, each with its own share of the
//! cap in [`crate::queue`]:
//!
//! * [`Lane::Proven`]: the transaction carries an Orchard bundle, and every
//!   Orchard and Ironwood proof in it verifies under the post-NU6.3 key. The
//!   signatures are checked too when the sighash needs no chain state, which is
//!   when the transaction has no transparent inputs (ZIP 244 commits the
//!   shielded sighash to every prevout's amount and script). The signatures add
//!   no cost to an attacker, who holds the keys of their own bundle; the proof
//!   is the cost. A proof that fails is a refusal.
//! * [`Lane::Transparent`]: a transparent-only spend, the shape of a TEX
//!   payment's second leg, which the shim diverts and which carries no proof.
//!   The queue promotes one into the proven lane when its parent is held there.
//! * [`Lane::Unverified`]: everything the hub cannot price. Bytes that do not
//!   parse (REVIEW #5 still queues them), a transaction from a circuit era this
//!   build has no key for, and a class an honest shim never sends.
//!
//! The anchor is the one public input left unchecked: whether the tree root the
//! proof names is a root the chain ever had. That needs chain state and stays the
//! node's job, so a proof against an attacker's own tree verifies here. It still
//! costs a proof, about 2.6 core-seconds for the smallest bundle, which is the
//! point.
//!
//! **One proof buys one entry.** Signatures are randomized and the attacker can
//! re-sign their own bundle at will, so the payload hash, which keys the queue,
//! would let one proof fill the cap. Each bundle therefore also has a
//! [`bundle key`](Inspected::bundle_keys): a hash of the proof's public inputs
//! (the anchor, the flags, and every action's nullifier, `rk`, `cmx` and
//! `cv_net`). Changing any of them needs a new proof, and an honest rebuild
//! re-randomizes them all. The queue holds at most one entry per bundle key,
//! and refuses a key it has already published to a verdict.

// A panic in a diskless enclave destroys the queue (see `queue.rs`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Cursor;
use std::sync::{Arc, LazyLock};

use orchard::bundle::BatchValidator;
use orchard::circuit::{OrchardCircuitVersion, VerifyingKey};
use sha2::{Digest, Sha256};
use zcash_protocol::value::ZatBalance;
use zebra_chain::parameters::NetworkUpgrade;
use zebra_chain::serialization::ZcashDeserialize;
use zebra_chain::transaction::{HashType, Transaction};
use zebra_chain::transparent::OutPoint;

/// The verifying key for the NU6.3 Action circuit, which every Orchard and
/// Ironwood bundle from NU6.3 onward commits to (`zebra-consensus`'s
/// `orchard_v6_verifier`).
///
/// About 0.8 s to build on four cores and deterministic, so it has no bearing on
/// attestation. `main` forces it before the listeners open, so no submission
/// pays for it.
pub static VERIFYING_KEY: LazyLock<VerifyingKey> =
    LazyLock::new(|| VerifyingKey::build(OrchardCircuitVersion::PostNu6_3));

/// Which share of the queue's entry cap a submission draws on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Proven,
    Transparent,
    Unverified,
}

type OrchardBundle = orchard::bundle::Bundle<orchard::bundle::Authorized, ZatBalance>;

/// The bundles of a [`Lane::Proven`] candidate, held until [`Inspected::verify`].
struct Pending {
    bundles: Vec<OrchardBundle>,
    /// `None` when the transaction has transparent inputs: the proofs are then
    /// verified alone.
    sighash: Option<[u8; 32]>,
}

/// One submission, read.
pub struct Inspected {
    /// Computed from the bytes. `None` when they do not parse.
    pub txid: Option<String>,
    /// `None` means no expiry (ZIP 203 `nExpiryHeight == 0`) or no parse.
    pub expiry: Option<u32>,
    pub lane: Lane,
    /// One per Orchard-protocol bundle, for a [`Lane::Proven`] candidate only.
    pub bundle_keys: Vec<[u8; 32]>,
    /// The Orchard bundle's nullifiers, for a [`Lane::Proven`] candidate only:
    /// what [`crate::spent`] checks against the chain.
    pub orchard_nullifiers: Vec<[u8; 32]>,
    /// The outpoints a [`Lane::Transparent`] spend consumes.
    pub spends: Vec<OutPoint>,
    /// How many transparent outputs the transaction has: how many second legs
    /// it could fund.
    pub transparent_outputs: usize,
    pending: Option<Pending>,
}

impl Inspected {
    /// Whether [`Self::verify`] has work to do.
    pub fn needs_proof(&self) -> bool {
        self.pending.is_some()
    }

    /// Verify every proof, and every signature when the sighash is known. About
    /// 9 ms for a two-bundle migration on four cores, 30 ms on one. `true` when
    /// there is nothing to verify.
    pub fn verify(&self) -> bool {
        let Some(pending) = &self.pending else {
            return true;
        };
        let vk = &*VERIFYING_KEY;
        match pending.sighash {
            Some(sighash) => {
                // Signatures first and proofs second, inside one validator: a
                // bad signature costs 0.4 ms, and both bundles share the proof
                // batch's final multi-exponentiation.
                let mut batch = BatchValidator::new(vk);
                for bundle in &pending.bundles {
                    if batch.add_bundle(bundle, sighash).is_err() {
                        return false;
                    }
                }
                batch.validate(rand08::rngs::OsRng)
            }
            None => pending
                .bundles
                .iter()
                .all(|bundle| bundle.verify_proof(vk).is_ok()),
        }
    }
}

/// Read a submission. Cheap, about 0.1 ms: the proof check is [`Inspected::verify`],
/// which the queue runs only after its cheaper refusals.
///
/// `nExpiryHeight == 0` means the transaction has no expiry (ZIP 203), so it is
/// folded to `None` here: infinitely far away.
pub fn inspect(tx_bytes: &[u8]) -> Inspected {
    let mut inspected = Inspected {
        txid: None,
        expiry: None,
        lane: Lane::Unverified,
        bundle_keys: Vec::new(),
        orchard_nullifiers: Vec::new(),
        spends: Vec::new(),
        transparent_outputs: 0,
        pending: None,
    };

    let mut cursor = Cursor::new(tx_bytes);
    let Ok(tx) = Transaction::zcash_deserialize(&mut cursor) else {
        return inspected;
    };
    inspected.txid = Some(tx.hash().to_string());
    inspected.expiry = tx
        .expiry_height()
        .map(|height| height.0)
        .filter(|height| *height != 0);
    inspected.transparent_outputs = tx.outputs().len();

    // Trailing bytes after a transaction: the node acts on bytes the hub's
    // parse never read, so nothing here can vouch for them. The shim calls the
    // same bytes unparseable.
    if usize::try_from(cursor.position()).ok() != Some(tx_bytes.len()) {
        return inspected;
    }

    if tx.orchard_shielded_data().is_some() {
        if let Some(pending) = prove_candidate(&tx) {
            inspected.bundle_keys = pending.bundles.iter().map(bundle_key).collect();
            // The Orchard bundle comes first, and is present: this arm needs it.
            inspected.orchard_nullifiers = pending
                .bundles
                .first()
                .map(|bundle| {
                    bundle
                        .actions()
                        .iter()
                        .map(|action| action.nullifier().to_bytes())
                        .collect()
                })
                .unwrap_or_default();
            inspected.lane = Lane::Proven;
            inspected.pending = Some(pending);
        }
    } else if is_transparent_only_spend(&tx) {
        inspected.lane = Lane::Transparent;
        inspected.spends = tx.spent_outpoints().collect();
    }
    inspected
}

/// The bundles and sighash of an Orchard-touching transaction, or `None` when
/// this build cannot verify it.
///
/// The circuit era comes from the transaction's own consensus branch id. An era
/// this build has no key for goes to the unverified lane. Over the mixnet a
/// refusal is silent, so a hub that lags an upgrade must degrade to the old
/// free admission and keep queueing every migration.
fn prove_candidate(tx: &Transaction) -> Option<Pending> {
    match tx.network_upgrade()? {
        NetworkUpgrade::Nu6_3 | NetworkUpgrade::Nu7 => {}
        _ => return None,
    }
    let nu = tx.network_upgrade()?;
    // Prevouts are only read by the sighash, which is skipped below whenever
    // the transaction has transparent inputs.
    let sighasher = tx.sighasher(nu, Arc::new(Vec::new())).ok()?;
    let sighash = tx
        .inputs()
        .is_empty()
        .then(|| sighasher.sighash(HashType::ALL, None).0);
    let bundles: Vec<_> = sighasher
        .orchard_bundle()
        .into_iter()
        .chain(sighasher.ironwood_bundle())
        .collect();
    (!bundles.is_empty()).then_some(Pending { bundles, sighash })
}

/// The hash of one bundle's proof inputs. Equal keys mean the same proof; a
/// different key costs a new one.
fn bundle_key(bundle: &OrchardBundle) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"zero-indexer-hub bundle key v1");
    hasher.update(bundle.anchor().to_bytes());
    let flags = bundle.flags();
    hasher.update([u8::from(flags.spends_enabled())
        | u8::from(flags.outputs_enabled()) << 1
        | u8::from(flags.cross_address_enabled()) << 2]);
    for action in bundle.actions() {
        hasher.update(action.nullifier().to_bytes());
        hasher.update(<[u8; 32]>::from(action.rk()));
        hasher.update(action.cmx().to_bytes());
        hasher.update(action.cv_net().to_bytes());
    }
    hasher.finalize().into()
}

/// The shim's second arm, verbatim: the transaction spends a transparent output
/// and has transparent parts only.
fn is_transparent_only_spend(tx: &Transaction) -> bool {
    tx.spent_outpoints().next().is_some()
        && tx.joinsplit_count() == 0
        && !tx.has_sapling_shielded_data()
        && tx.orchard_shielded_data().is_none()
        && !tx.has_ironwood_shielded_data()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const PROVEN_A: &[u8] = include_bytes!("../tests/fixtures/v6_proven_a.bin");
    const PROVEN_A_RESIGNED: &[u8] = include_bytes!("../tests/fixtures/v6_proven_a_resigned.bin");
    const PROVEN_A_BAD_SIG: &[u8] = include_bytes!("../tests/fixtures/v6_proven_a_bad_sig.bin");
    const PROVEN_B: &[u8] = include_bytes!("../tests/fixtures/v6_proven_b.bin");
    // The shim's fixtures parse, and their proofs are zero-filled.
    const ZERO_PROOF: &[u8] = include_bytes!("../../shim/tests/fixtures/v6_migration.bin");
    const IRONWOOD_ONLY: &[u8] = include_bytes!("../../shim/tests/fixtures/v6_ironwood_only.bin");
    const TRANSPARENT_ONLY: &[u8] =
        include_bytes!("../../shim/tests/fixtures/v6_transparent_only.bin");

    #[test]
    fn a_proven_migration_verifies_in_the_proven_lane() {
        let inspected = inspect(PROVEN_A);
        assert_eq!(inspected.lane, Lane::Proven);
        assert_eq!(
            inspected.bundle_keys.len(),
            2,
            "an Orchard and an Ironwood bundle"
        );
        assert_eq!(inspected.transparent_outputs, 1);
        assert!(inspected.txid.is_some());
        assert!(inspected.verify());
    }

    #[test]
    fn the_orchard_nullifiers_are_read_from_the_orchard_bundle_alone() {
        // One spend padded to two actions; the Ironwood bundle's are not here.
        let inspected = inspect(PROVEN_A);
        assert_eq!(inspected.orchard_nullifiers.len(), 2);
        assert_eq!(
            inspected.orchard_nullifiers,
            inspect(PROVEN_A_RESIGNED).orchard_nullifiers
        );
        let b = inspect(PROVEN_B).orchard_nullifiers;
        assert!(inspected
            .orchard_nullifiers
            .iter()
            .all(|nf| !b.contains(nf)));
    }

    #[test]
    fn zero_filled_proofs_fail() {
        let inspected = inspect(ZERO_PROOF);
        assert_eq!(inspected.lane, Lane::Proven);
        assert!(!inspected.verify());
    }

    #[test]
    fn a_signature_over_the_wrong_sighash_fails() {
        // The proofs are the proven fixture's own; only the signatures are bad.
        let inspected = inspect(PROVEN_A_BAD_SIG);
        assert_eq!(inspected.bundle_keys, inspect(PROVEN_A).bundle_keys);
        assert!(!inspected.verify());
    }

    #[test]
    fn re_signing_keeps_the_bundle_keys() {
        // The attack bundle keys exist for: new bytes from one proof.
        assert_ne!(PROVEN_A, PROVEN_A_RESIGNED);
        let resigned = inspect(PROVEN_A_RESIGNED);
        assert!(resigned.verify());
        assert_eq!(resigned.bundle_keys, inspect(PROVEN_A).bundle_keys);
        assert_ne!(resigned.txid, None);
    }

    #[test]
    fn independent_proofs_have_independent_keys() {
        let a = inspect(PROVEN_A).bundle_keys;
        let b = inspect(PROVEN_B).bundle_keys;
        assert!(a.iter().all(|key| !b.contains(key)));
    }

    #[test]
    fn each_class_lands_in_its_lane() {
        assert_eq!(inspect(&[0x42; 64]).lane, Lane::Unverified);
        assert_eq!(inspect(IRONWOOD_ONLY).lane, Lane::Unverified);
        let transparent = inspect(TRANSPARENT_ONLY);
        assert_eq!(transparent.lane, Lane::Transparent);
        assert_eq!(transparent.spends.len(), 1);
        assert!(!transparent.needs_proof() && transparent.verify());
    }

    #[test]
    fn trailing_bytes_are_unverified_whatever_precedes_them() {
        let mut padded = PROVEN_A.to_vec();
        padded.extend_from_slice(&[0; 16]);
        let inspected = inspect(&padded);
        assert_eq!(inspected.lane, Lane::Unverified);
        assert!(inspected.bundle_keys.is_empty());
    }
}

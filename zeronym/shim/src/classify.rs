//! The turnstile classifier: a pure function from raw transaction bytes to a verdict.
//!
//! This is the highest-stakes code in the shim. A false negative (a migration
//! classified as `PassThrough`) is a privacy leak, because the transaction is
//! then broadcast through the operator's own indexer, linking the migrating
//! wallet to the operator's view of the network. A false positive is merely a
//! wasted diversion.
//!
//! Two properties keep this auditable:
//!
//! * **Pure.** No I/O, no state, no clock, no config. The verdict is a total
//!   function of the bytes. Everything here can be exercised by a byte-vector
//!   test.
//! * **Fail-safe for privacy.** Anything that does not parse cleanly is
//!   [`Class::Unparseable`], and the CALLER treats `Unparseable` exactly like
//!   `Migration` (in the PoC it logs `MIGRATION-FAILSAFE`; in production it
//!   diverts to the hub). The fail-safe policy deliberately lives at the call
//!   site, so this module stays a plain classifier with no policy in it. Use
//!   [`Class::treat_as_migration`] so that policy is written once.
//!
//! Scope note: this module classifies the INNER transaction bytes, that is the
//! `data` field of a decoded `RawTransaction`. gRPC length-prefix framing,
//! `grpc-encoding` compression, and protobuf decoding all happen in the caller.
//! A compressed or malformed gRPC frame never reaches here; the caller must
//! treat those as migrations too, for the same fail-safe reason.

use std::io::Cursor;

use zebra_chain::{serialization::ZcashDeserialize, transaction::Transaction};

/// The verdict for one `SendTransaction` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// An Orchard -> Ironwood migration. Privacy-critical: must not be
    /// broadcast through the operator's indexer.
    Migration,
    /// Any other transaction. Safe to forward to the backing indexer.
    PassThrough,
    /// The bytes did not parse as a Zcash transaction. The caller treats this
    /// as a migration (fail-safe for privacy), never as a pass-through.
    Unparseable,
}

impl Class {
    /// The routing decision, with the fail-safe folded in exactly once.
    ///
    /// `true` means "do not hand this to the backing indexer" (in production:
    /// divert to the hub). `Unparseable` is `true` on purpose: we would rather
    /// divert a transaction we could not read than leak one we could not read.
    pub fn treat_as_migration(self) -> bool {
        matches!(self, Class::Migration | Class::Unparseable)
    }
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Class::Migration => "MIGRATION",
            Class::PassThrough => "PASS-THROUGH",
            // The caller logs this as MIGRATION-FAILSAFE; the raw name is kept
            // here so the classifier does not encode routing policy.
            Class::Unparseable => "UNPARSEABLE",
        };
        f.write_str(label)
    }
}

/// Everything the caller needs to log a verdict with its supporting evidence,
/// so an operator can tell a genuine migration from a novel transaction format
/// without re-parsing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// `"V1".."V6"`, or `"unparseable"`.
    pub version: String,
    /// Orchard value balance in zatoshis. Positive means value LEAVING Orchard.
    pub orchard_vb: i64,
    /// Ironwood value balance in zatoshis. Negative means value ENTERING Ironwood.
    pub ironwood_vb: i64,
    /// Sapling value balance in zatoshis. Not part of the predicate; logged
    /// because a migration that also touches Sapling is worth seeing.
    pub sapling_vb: i64,
    /// `None` when the transaction sets no expiry.
    pub expiry_height: Option<u32>,
    /// Transparent input count.
    pub inputs: usize,
    /// Transparent output count.
    pub outputs: usize,
    /// Length of the raw transaction bytes classified.
    pub len: usize,
    /// Why the parse failed, for `Class::Unparseable` only.
    pub error: Option<String>,
    /// The verdict itself.
    pub class: Class,
}

impl Evidence {
    /// Evidence for bytes that never parsed. All balances are reported as zero
    /// because nothing was read out of them; `error` carries the reason.
    fn unparseable(len: usize, error: String) -> Self {
        Evidence {
            version: "unparseable".to_string(),
            orchard_vb: 0,
            ironwood_vb: 0,
            sapling_vb: 0,
            expiry_height: None,
            inputs: 0,
            outputs: 0,
            len,
            error: Some(error),
            class: Class::Unparseable,
        }
    }
}

/// The fee-aware turnstile predicate, migration case.
///
/// ```text
/// is_migration(tx) := tx.version == V6
///                  && orchard_value_balance  > 0   (value LEAVING Orchard)
///                  && ironwood_value_balance < 0   (value ENTERING Ironwood)
/// ```
///
/// Pure: no I/O, no state.
pub fn classify(raw: &[u8]) -> Class {
    classify_with_evidence(raw).class
}

/// [`classify`], plus the parsed facts the verdict rests on, for logging.
///
/// The verdict this returns is byte-for-byte the same decision `classify`
/// makes: `classify` is defined in terms of this function, so the log line can
/// never disagree with the routing decision.
pub fn classify_with_evidence(raw: &[u8]) -> Evidence {
    let mut cursor = Cursor::new(raw);

    let tx = match Transaction::zcash_deserialize(&mut cursor) {
        Ok(tx) => tx,
        Err(err) => return Evidence::unparseable(raw.len(), err.to_string()),
    };

    // Zebra's deserializer stops at the end of the transaction and ignores
    // whatever follows, so a body with trailing junk parses Ok. Reject it: we
    // must classify exactly the bytes the backing node would act on, not a
    // prefix of them. Verified: a valid tx plus 16 junk bytes deserializes Ok
    // without this check.
    if cursor.position() != raw.len() as u64 {
        return Evidence::unparseable(
            raw.len(),
            format!(
                "trailing bytes: parsed {} of {} bytes",
                cursor.position(),
                raw.len()
            ),
        );
    }

    // The value-balance accessors each build a ValueBalance with exactly ONE
    // pool slot populated: orchard_value_balance() sets only `orchard`,
    // ironwood_value_balance() sets only `ironwood`. Calling .orchard_amount()
    // on the ironwood balance returns 0 and would silently turn every migration
    // into a PassThrough, so the selector must match the accessor.
    let orchard_vb = tx.orchard_value_balance().orchard_amount().zatoshis();
    let ironwood_vb = tx.ironwood_value_balance().ironwood_amount().zatoshis();
    let sapling_vb = tx.sapling_value_balance().sapling_amount().zatoshis();

    // Pre-V6 transactions have no Ironwood bundle, so both balances read 0 and
    // the predicate below can never fire. The version check is kept explicit
    // anyway: it is part of the specified predicate, and it documents that a
    // migration is a V6-only shape.
    //
    // Note on the strict `> 0`: a migration that netted its Orchard balance to
    // exactly zero would classify as PassThrough, which is the leaking
    // direction. Spending an Orchard note always produces a positive Orchard
    // balance in practice, so this shape should not exist, but the boundary is
    // an open question for the design owners rather than something this
    // function is free to widen.
    let class = if matches!(tx, Transaction::V6 { .. }) && orchard_vb > 0 && ironwood_vb < 0 {
        Class::Migration
    } else {
        Class::PassThrough
    };

    Evidence {
        version: format!("V{}", tx.version()),
        orchard_vb,
        ironwood_vb,
        sapling_vb,
        expiry_height: tx.expiry_height().map(|height| height.0),
        inputs: tx.inputs().len(),
        outputs: tx.outputs().len(),
        len: raw.len(),
        error: None,
        class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_unparseable() {
        assert_eq!(classify(&[]), Class::Unparseable);
    }

    #[test]
    fn garbage_is_unparseable() {
        assert_eq!(classify(&[0xff; 64]), Class::Unparseable);
    }

    #[test]
    fn unparseable_is_routed_like_a_migration() {
        assert!(Class::Migration.treat_as_migration());
        assert!(Class::Unparseable.treat_as_migration());
        assert!(!Class::PassThrough.treat_as_migration());
    }

    #[test]
    fn evidence_and_class_never_disagree() {
        for bytes in [&[][..], &[0xff; 64][..]] {
            assert_eq!(classify(bytes), classify_with_evidence(bytes).class);
        }
    }
}

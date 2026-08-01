//! The turnstile classifier: a pure function from raw transaction bytes to a verdict.
//!
//! What it detects is an ORCHARD EXIT: a transaction that moves value out of the
//! Orchard pool, whatever pool the value lands in afterwards.
//!
//! Why that is the right thing to detect, and not "Orchard -> Ironwood":
//!
//! * NU6.3 closes the Orchard pool to new VALUE. A transaction-level rule
//!   forbids value entering Orchard, so the chain predicate is Orchard pool
//!   value non-increasing and `orchard_value_balance >= 0` for every
//!   post-activation transaction. (Closed to new value, NOT to activity:
//!   same-receiver change still lands in the pool and the note commitment tree
//!   keeps growing. Orchard is not "exit-only".)
//! * So anyone still holding Orchard notes has held them since before
//!   activation, and SPENDING ORCHARD AT ALL is the identifying event. It
//!   reveals "this IP controls legacy Orchard funds" against a finite, shrinking
//!   set of holders.
//! * The destination pool is irrelevant to that inference. An Orchard withdrawal
//!   to transparent, or to Sapling, leaks exactly the same fact as one into
//!   Ironwood, so all of them are diverted.
//!
//! This is Zooko's ruling on the classifier's scope: any transaction with
//! Orchard value balance > 0 is a privacy risk to the user, regardless of the
//! destination pool.
//!
//! This is the highest-stakes code in the shim. A false negative (an Orchard
//! exit classified as `PassThrough`) is a privacy leak, because the transaction
//! is then broadcast through the operator's own indexer, linking the wallet that
//! holds legacy Orchard funds to the operator's view of the network. A false
//! positive is merely a wasted diversion.
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
    /// An ORCHARD EXIT: value left the Orchard pool, to any destination.
    /// Privacy-critical: must not be broadcast through the operator's indexer.
    ///
    /// A note on the name. Post-NU6.3 every Orchard exit is legacy-fund
    /// movement, so batching all of them is the right behaviour, but "Migration"
    /// is imprecise for the class as a whole: an Orchard-to-transparent deshield
    /// is not literally a migration into Ironwood. The variant keeps its name
    /// because it is what the log lines, the routing helper and the operator
    /// docs already call the diverted class; [`is_orchard_exit`] is the accurate
    /// name for the predicate behind it.
    Migration,
    /// A transaction that moved no value out of the Orchard pool: either it
    /// carries no Orchard bundle, or its Orchard balance is zero (or negative,
    /// which post-NU6.3 is consensus-invalid). By the ruling's criterion no
    /// legacy Orchard value moved, so it is forwarded to the backing indexer.
    /// One limit worth knowing about, not a claim of zero leakage: a net-zero
    /// Orchard bundle can still spend legacy notes and publish their
    /// nullifiers, so "no value moved out" is not the same as "nothing was
    /// revealed". See [`is_orchard_exit`] for the open question that raises.
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
/// so an operator can tell a genuine Orchard exit from a novel transaction
/// format without re-parsing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// `"V1".."V6"`, or `"unparseable"`.
    pub version: String,
    /// Orchard value balance in zatoshis. Positive means value LEAVING Orchard.
    /// This is the whole predicate: `orchard_vb > 0` is the verdict.
    pub orchard_vb: i64,
    /// Ironwood value balance in zatoshis. Negative means value ENTERING Ironwood.
    ///
    /// NOT part of the predicate any more, and still worth logging: it is what
    /// tells an operator where an Orchard exit went (into Ironwood, or out to
    /// transparent or Sapling), which is exactly the evidence needed to see that
    /// the classifier is catching the destinations it used to miss.
    pub ironwood_vb: i64,
    /// Sapling value balance in zatoshis. Not part of the predicate; logged
    /// because an Orchard exit that also touches Sapling is worth seeing.
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

/// The turnstile predicate.
///
/// ```text
/// is_orchard_exit(tx) := orchard_value_balance > 0   (value LEAVING Orchard)
/// ```
///
/// One conjunct, no version guard, no destination check. See the module docs for
/// why the destination pool is irrelevant.
///
/// Pure: no I/O, no state.
pub fn classify(raw: &[u8]) -> Class {
    classify_with_evidence(raw).class
}

/// The predicate, isolated so it has an accurate name and one place to audit.
///
/// `true` means value left the Orchard pool in this transaction, which is the
/// privacy-relevant event no matter where it landed.
///
/// The boundary, stated exactly: `> 0` catches every transaction that moves
/// value OUT of Orchard. A bundle that nets to zero moves none out and passes
/// through, and that is the ruling's criterion. Post-NU6.3 `orchard_vb < 0`
/// (value entering Orchard) is consensus-invalid, so `== 0` is the only other
/// case that can occur on chain.
///
/// One thing this leaves OPEN rather than settles, recorded here so nobody
/// closes it by reading the code: a net-zero Orchard bundle can still SPEND
/// legacy Orchard notes (fee paid from transparent or Sapling, change back to
/// the same receiver), and spending them publishes their nullifiers on the
/// wire. Under the ruling's own rationale, quoted in the module docs, spending
/// Orchard at all is the identifying event, so such a transaction is an
/// identifying event that the ruling's criterion does not catch. Whether to
/// widen the predicate to "an Orchard bundle is present with at least one
/// spend" is Zooko's call, not the classifier's. It is in the shim README's
/// open questions and is in front of Zooko and Taylor; do not change the
/// predicate here to pre-empt the answer.
pub fn is_orchard_exit(orchard_value_balance: i64) -> bool {
    orchard_value_balance > 0
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
    // on the ironwood balance returns 0 and would silently turn every Orchard
    // exit into a PassThrough, so the selector must match the accessor.
    let orchard_vb = tx.orchard_value_balance().orchard_amount().zatoshis();
    let ironwood_vb = tx.ironwood_value_balance().ironwood_amount().zatoshis();
    let sapling_vb = tx.sapling_value_balance().sapling_amount().zatoshis();

    // No version guard, deliberately. V5 transactions carry Orchard bundles too,
    // and a V5 Orchard spend leaks the same fact as a V6 one. It needs no guard
    // to be safe either: orchard_value_balance() reads orchard_shielded_data(),
    // which is version-agnostic and returns zero for V1..V4, where there is no
    // Orchard bundle at all (zebra-chain/src/transaction.rs, both accessors).
    // So a transparent transaction reads orchard_vb == 0 and passes through by
    // the predicate itself rather than by a special case.
    //
    // No destination check either. `ironwood_vb` is read above for the log line
    // only: an Orchard withdrawal to transparent or to Sapling reveals the same
    // "this IP controls legacy Orchard funds" as one into Ironwood, so gating on
    // where the value went would pass exactly those leaks through in the clear.
    let class = if is_orchard_exit(orchard_vb) {
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
    fn the_predicate_is_the_orchard_sign_alone() {
        // Any withdrawal, however small, is an exit from a closed pool.
        assert!(is_orchard_exit(1));
        assert!(is_orchard_exit(250_000));
        // No Orchard value moved out, so the ruling's criterion passes it
        // through. See `is_orchard_exit` for what that criterion does not catch.
        assert!(!is_orchard_exit(0));
        // Value entering Orchard, consensus-invalid post-NU6.3. Kept as a probe
        // that the predicate is directional.
        assert!(!is_orchard_exit(-250_000));
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

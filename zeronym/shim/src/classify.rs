//! The turnstile classifier: a pure function from raw transaction bytes to a verdict.
//!
//! What it detects is ORCHARD ACTIVITY: a transaction that carries any Orchard
//! actions at all. Not an exit, not a direction, not an amount. Presence.
//!
//! This is Zooko's ruling on the classifier's scope, in his words: any
//! transaction that has any Orchard actions in it is (a) potentially
//! security-sensitive, because it could leak information the user did not want
//! to disclose, and (b) probably time-insensitive, because people and their
//! tools are already used to the idea that doing anything with Orchard funds
//! might take longer than normal. So the safe default is to divert every one of
//! them to the batching system, regardless of whether `orchard_value_balance`
//! is greater than the fee, equal to the fee, or zero.
//!
//! Both halves of that rationale matter, and they are what keeps the rule from
//! growing:
//!
//! * **Security-sensitive.** NU6.3 closed the Orchard pool to new VALUE, so
//!   anyone still holding Orchard notes has held them since before activation.
//!   Touching Orchard at all is the identifying event: it reveals "this IP
//!   controls legacy Orchard funds" against a finite, shrinking set of holders.
//!   Spending publishes nullifiers whatever the balance nets to.
//! * **Time-insensitive.** Batching costs latency, and Orchard users already
//!   expect legacy-fund movement to be slow. That is what makes the safe default
//!   affordable here.
//!
//! ORCHARD ONLY, NOT IRONWOOD, and that is deliberate. Ironwood is the NEW pool,
//! where ordinary time-sensitive commerce will live, and the time-insensitivity
//! half of the rationale does not hold for it. A transaction with only Ironwood
//! actions must still pass through, so there is no Ironwood arm in this
//! predicate and none should be added.
//!
//! What the widening closed: a transaction must pay a fee, and unless the fee
//! comes from another pool it comes out of Orchard, so most internal shuffling
//! already showed `orchard_value_balance > 0` and the old exit predicate caught
//! it. The gap was the shuffle whose fee is paid from a DIFFERENT pool, which
//! leaves `orchard_value_balance == 0` with Orchard actions still present. That
//! transaction used to be handed to the operator's indexer in the clear.
//!
//! This is the highest-stakes code in the shim. A false negative (a transaction
//! carrying Orchard actions classified as `PassThrough`) is a privacy leak,
//! because the transaction is then broadcast through the operator's own indexer,
//! linking the wallet that holds legacy Orchard funds to the operator's view of
//! the network. A false positive is merely a wasted diversion.
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
    /// The transaction TOUCHES ORCHARD: it carries at least one Orchard action.
    /// Privacy-critical: must not be broadcast through the operator's indexer.
    ///
    /// A note on the name. Post-NU6.3 any Orchard activity is legacy-fund
    /// activity, so batching all of it is the right behaviour, but "Migration"
    /// is imprecise for the class as a whole: an Orchard-to-transparent deshield
    /// is not literally a migration into Ironwood, and a net-zero internal
    /// shuffle moves nothing anywhere. The variant keeps its name because it is
    /// what the log lines, the routing helper and the operator docs already call
    /// the diverted class; [`is_orchard_touching`] is the accurate name for the
    /// predicate behind it.
    Migration,
    /// A transaction that carries NO Orchard actions: no Orchard bundle at all.
    /// Transparent, Sapling, and Ironwood-only transactions land here, which is
    /// the point. Ironwood is the new pool where ordinary time-sensitive
    /// commerce lives, and it is forwarded to the backing indexer.
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
/// so an operator can tell genuine Orchard activity from a novel transaction
/// format without re-parsing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// `"V1".."V6"`, or `"unparseable"`.
    pub version: String,
    /// How many Orchard actions the transaction carries. **This is the deciding
    /// fact**: zero is a `PassThrough`, anything else is a `Migration`. An
    /// Orchard bundle is guaranteed non-empty by the wire format
    /// (`orchard::ShieldedData.actions` is an `AtLeastOne`), so this is zero
    /// exactly when there is no Orchard bundle.
    pub orchard_actions: usize,
    /// Orchard value balance in zatoshis. Positive means value LEAVING Orchard.
    ///
    /// **Evidence only, not the predicate.** It was the predicate until Zooko's
    /// widening; now it says how much moved, while `orchard_actions` says
    /// whether to divert. Post-NU6.3 it is always `>= 0`, because a
    /// transaction-level rule forbids value entering the Orchard pool. That is
    /// context for reading the log line, not logic.
    pub orchard_vb: i64,
    /// Ironwood value balance in zatoshis. Negative means value ENTERING Ironwood.
    ///
    /// Not part of the predicate, and it must not become part of it: an
    /// Ironwood-only transaction is ordinary commerce and passes through. It is
    /// logged because it is what tells an operator where an Orchard withdrawal
    /// went (into Ironwood, or out to transparent or Sapling).
    pub ironwood_vb: i64,
    /// Sapling value balance in zatoshis. Not part of the predicate; logged
    /// because Orchard activity that also touches Sapling is worth seeing.
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
    /// Evidence for bytes that never parsed. Balances and counts are reported as
    /// zero because nothing was read out of them; `error` carries the reason.
    fn unparseable(len: usize, error: String) -> Self {
        Evidence {
            version: "unparseable".to_string(),
            orchard_actions: 0,
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
/// is_orchard_touching(tx) := tx has at least one Orchard action
/// ```
///
/// One conjunct, no version guard, no destination check, no amount, no sign. See
/// the module docs for Zooko's rationale.
///
/// Pure: no I/O, no state.
pub fn classify(raw: &[u8]) -> Class {
    classify_with_evidence(raw).class
}

/// The predicate, isolated so it has an accurate name and one place to audit.
///
/// `true` means the transaction carries Orchard actions, which is the
/// privacy-relevant event no matter what its value balance is, which direction
/// that balance points, or where the value landed.
///
/// It is written as "an Orchard bundle is present" rather than "the action count
/// is non-zero" because the two are EXACTLY equivalent and this form cannot be
/// fooled by an empty bundle: `orchard::ShieldedData.actions` is an
/// `AtLeastOne<AuthorizedAction>` (zebra-chain/src/orchard/shielded_data.rs), so
/// a bundle that exists has at least one action. `orchard_action_count` reports
/// the count as evidence, and `presence_and_action_count_agree` pins the
/// equivalence.
///
/// `orchard_shielded_data()` is version-agnostic (zebra-chain/src/transaction.rs)
/// and returns `None` for V1..V4, where there is no Orchard bundle at all, so a
/// transparent transaction passes through by the predicate itself rather than by
/// a special case. A V5 Orchard spend leaks the same fact as a V6 one and is
/// caught by the same line.
///
/// The boundary this must NOT cross: Ironwood. A V6 carrying only Ironwood
/// actions reads `orchard_shielded_data() == None` and passes through, because
/// Ironwood is the new pool where ordinary time-sensitive commerce lives and
/// Zooko's time-insensitivity rationale does not extend to it.
pub fn is_orchard_touching(tx: &Transaction) -> bool {
    tx.orchard_shielded_data().is_some()
}

/// How many Orchard actions the transaction carries. Zero means no Orchard
/// bundle. Reported as evidence in the log line, since it is the deciding fact.
pub fn orchard_action_count(tx: &Transaction) -> usize {
    tx.orchard_actions().count()
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

    // The predicate. Presence of an Orchard bundle, nothing else.
    let class = if is_orchard_touching(&tx) {
        Class::Migration
    } else {
        Class::PassThrough
    };

    // Evidence. None of this gates the verdict.
    //
    // The value-balance accessors each build a ValueBalance with exactly ONE
    // pool slot populated: orchard_value_balance() sets only `orchard`,
    // ironwood_value_balance() sets only `ironwood`. Calling .orchard_amount()
    // on the ironwood balance returns 0, so the selector must match the
    // accessor. That used to be a routing bug waiting to happen; since the
    // widening it can only produce a misleading log line, which is still worth
    // getting right.
    let orchard_actions = orchard_action_count(&tx);
    let orchard_vb = tx.orchard_value_balance().orchard_amount().zatoshis();
    let ironwood_vb = tx.ironwood_value_balance().ironwood_amount().zatoshis();
    let sapling_vb = tx.sapling_value_balance().sapling_amount().zatoshis();

    // The equivalence the predicate relies on, checked where it is used. A
    // bundle is an AtLeastOne, so an Orchard bundle with zero actions cannot be
    // constructed or deserialized; if that ever changes, this fires in debug and
    // CI rather than silently passing a bundle through.
    debug_assert_eq!(
        class == Class::Migration,
        orchard_actions > 0,
        "an Orchard bundle must carry at least one action"
    );

    Evidence {
        version: format!("V{}", tx.version()),
        orchard_actions,
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

    /// V6 with an Orchard bundle: Orchard(+250_000), Ironwood(-240_000).
    const V6_MIGRATION: &[u8] = include_bytes!("../tests/fixtures/v6_migration.bin");

    /// The same shape with both balances negated: Orchard(-250_000),
    /// Ironwood(+240_000). Under the old exit predicate this was a
    /// `PassThrough`; it carries Orchard actions, so now it is a `Migration`.
    const V6_REVERSE: &[u8] = include_bytes!("../tests/fixtures/v6_reverse.bin");

    /// V6 with an Orchard bundle whose value balance is exactly zero: the
    /// internal shuffle whose fee came from another pool. The gap the widening
    /// closes.
    const V6_ORCHARD_ZERO: &[u8] = include_bytes!("../tests/fixtures/v6_orchard_zero.bin");

    /// V6 with an Ironwood bundle and NO Orchard bundle: ordinary Ironwood
    /// commerce, which must keep passing through.
    const V6_IRONWOOD_ONLY: &[u8] = include_bytes!("../tests/fixtures/v6_ironwood_only.bin");

    fn parse(raw: &[u8]) -> Transaction {
        Transaction::zcash_deserialize(&mut Cursor::new(raw)).expect("fixture parses")
    }

    #[test]
    fn empty_input_is_unparseable() {
        assert_eq!(classify(&[]), Class::Unparseable);
    }

    #[test]
    fn garbage_is_unparseable() {
        assert_eq!(classify(&[0xff; 64]), Class::Unparseable);
    }

    #[test]
    fn the_predicate_is_the_presence_of_orchard_actions() {
        // Three Orchard bundles with three different value balances: positive,
        // negative, and exactly zero. The predicate does not read any of them.
        for (bytes, name) in [
            (V6_MIGRATION, "orchard_vb > 0"),
            (V6_REVERSE, "orchard_vb < 0"),
            (V6_ORCHARD_ZERO, "orchard_vb == 0"),
        ] {
            let tx = parse(bytes);
            assert!(
                is_orchard_touching(&tx),
                "{name}: Orchard actions are present, so it is diverted"
            );
        }

        // No Orchard bundle: the Ironwood side of the boundary, and the reason
        // the rule stops at Orchard.
        assert!(!is_orchard_touching(&parse(V6_IRONWOOD_ONLY)));
    }

    #[test]
    fn presence_and_action_count_agree() {
        // The equivalence `is_orchard_touching` is written on: an Orchard bundle
        // is an AtLeastOne, so presence and a non-zero count are the same fact.
        for bytes in [V6_MIGRATION, V6_REVERSE, V6_ORCHARD_ZERO, V6_IRONWOOD_ONLY] {
            let tx = parse(bytes);
            assert_eq!(is_orchard_touching(&tx), orchard_action_count(&tx) > 0);
        }
    }

    #[test]
    fn unparseable_is_routed_like_a_migration() {
        assert!(Class::Migration.treat_as_migration());
        assert!(Class::Unparseable.treat_as_migration());
        assert!(!Class::PassThrough.treat_as_migration());
    }

    #[test]
    fn evidence_and_class_never_disagree() {
        for bytes in [
            &[][..],
            &[0xff; 64][..],
            V6_MIGRATION,
            V6_ORCHARD_ZERO,
            V6_IRONWOOD_ONLY,
        ] {
            assert_eq!(classify(bytes), classify_with_evidence(bytes).class);
        }
    }
}

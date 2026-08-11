//! The shim-to-hub wire frames, version 1: `SubmitV1` in, `AckV1` back.
//!
//! This is the byte layout the Nym mixnet transport carries. It is written here
//! and, byte for byte, in `zero-indexer-shim`'s own `wire` module. The two crates
//! are separate workspaces on purpose (each lockfile is authoritative for its own
//! reproducible build), so the codec cannot be shared as a dependency; instead a
//! committed golden-vector file, identical in both crates' fixtures, fails a test
//! loudly the moment the two encoders drift.
//!
//! Two properties this layer exists to hold:
//!
//! * **Fixed size.** Every `SubmitV1` is exactly [`FRAME_BYTES`] and every
//!   `AckV1` is exactly [`ACK_BYTES`], padded with zeros, so a record's length
//!   carries no information to any layer that can see it. The transaction's true
//!   length lives in the `tx_len` field, read only after the frame is decrypted.
//! * **No txid on the wire.** Correlation between a submission and its
//!   acknowledgement is by a random 16-byte nonce the shim mints, never a txid
//!   (the txid is a control input an adversary could write; both ends compute it
//!   from the bytes instead). The nonce is echoed in the ack and matched there.
//!
//! ```text
//! SubmitV1, exactly FRAME_BYTES:
//!   0    magic    4   b"ZNS1"
//!   4    nonce   16   request nonce, from OsRng
//!   20   tx_len   4   u32 big-endian
//!   24   tx       tx_len bytes
//!   ..   padding  zeros to FRAME_BYTES
//!
//! AckV1, exactly ACK_BYTES:
//!   0    magic    4   b"ZNA1"
//!   4    nonce   16   echoed request nonce
//!   20   disp     1   0 accepted, 1 refused
//!   21   refusal  1   0 none, else an AckRefusal code
//!   ..   padding  zeros to ACK_BYTES
//! ```
//!
//! Decode is strict about the header and deliberately lax about the padding: a
//! wrong total length, a bad magic, or a `tx_len` that overruns the frame is a
//! [`WireError`], which the listener answers as `bad_frame`, but the bytes past
//! the declared transaction are never read, so nothing downstream can smuggle
//! meaning into the padding.
//!
//! A parse failure of the transaction INSIDE the frame is NOT a wire error: the
//! frame decodes fine and the bytes are queued and published like any other
//! (REVIEW #5). Only the frame envelope is this module's concern.

use zeroize::Zeroizing;

/// The fixed on-wire size of every `SubmitV1`. Matches the queue's per-entry byte
/// budget (`queue::MAX_TX_BYTES`) and the frame the batching design pads to.
pub const FRAME_BYTES: usize = 64 * 1024;

/// The fixed on-wire size of every `AckV1`.
pub const ACK_BYTES: usize = 64;

/// The request nonce is 16 bytes.
pub const NONCE_BYTES: usize = 16;

/// A 16-byte request nonce, minted per submission by the shim and echoed in the
/// ack. It is the ONLY correlation handle on the wire.
pub type Nonce = [u8; NONCE_BYTES];

/// `SubmitV1` magic. The final byte is the version.
const SUBMIT_MAGIC: [u8; 4] = *b"ZNS1";

/// `AckV1` magic. The final byte is the version.
const ACK_MAGIC: [u8; 4] = *b"ZNA1";

/// magic (4) + nonce (16) + tx_len (4).
const SUBMIT_HEADER_BYTES: usize = 24;

/// The largest transaction a `SubmitV1` can carry. A transaction larger than this
/// cannot be privately batched, which is the price of leaking zero bits of
/// length; the shim surfaces it to the wallet as an error rather than sending it.
pub const MAX_NYM_TX_BYTES: usize = FRAME_BYTES - SUBMIT_HEADER_BYTES;

/// Why a frame could not be built or read. Every decode failure means the same
/// thing to the listener (answer `bad_frame`); the variants exist so the reason
/// can be logged without a per-entry identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The buffer was not exactly the fixed frame size.
    WrongLength { expected: usize, got: usize },
    /// The 4-byte magic did not match.
    BadMagic,
    /// Encode side: the transaction is larger than [`MAX_NYM_TX_BYTES`].
    TxTooLarge { len: usize },
    /// Decode side: the declared `tx_len` runs past the end of the frame.
    TxLenOverrunsFrame { declared: usize },
    /// Decode side: the disposition byte was neither accepted (0) nor refused (1).
    UnknownDisposition(u8),
    /// Decode side: the refusal byte was not a known [`AckRefusal`] code.
    UnknownRefusal(u8),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::WrongLength { expected, got } => {
                write!(f, "wrong frame length: expected {expected}, got {got}")
            }
            WireError::BadMagic => f.write_str("bad frame magic"),
            WireError::TxTooLarge { len } => {
                write!(f, "transaction {len} bytes exceeds the {MAX_NYM_TX_BYTES}-byte frame budget")
            }
            WireError::TxLenOverrunsFrame { declared } => {
                write!(f, "declared tx_len {declared} overruns the frame")
            }
            WireError::UnknownDisposition(byte) => write!(f, "unknown disposition byte {byte}"),
            WireError::UnknownRefusal(byte) => write!(f, "unknown refusal byte {byte}"),
        }
    }
}

impl std::error::Error for WireError {}

/// The disposition an `AckV1` carries: the hub took responsibility for the
/// submission, or it refused it with a typed reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckKind {
    /// The hub holds the bytes and will publish them. Covers both a fresh
    /// admission and a duplicate, exactly as the HTTP path does.
    Accepted,
    /// The hub declined the submission. Every refusal fails closed at the shim.
    Refused(AckRefusal),
}

/// The typed refusal an `AckV1` can carry (refusal byte 1..5; byte 0 is the
/// "none" that rides with an [`AckKind::Accepted`]). The strings match
/// [`crate::queue::Refusal`]'s reasons, plus `bad_frame` for a frame the hub
/// could not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckRefusal {
    /// The transaction would expire on or before the flush that would publish it.
    ExpiryTooTight,
    /// Larger than the fixed frame.
    TooLarge,
    /// The hub queue is at its byte budget.
    QueueFull,
    /// The chain tip is stale, so admission cannot be trusted.
    TipStale,
    /// The hub could not decode the frame at all.
    BadFrame,
}

impl AckRefusal {
    /// The on-wire code. `0` is reserved for "none" and is never an `AckRefusal`.
    pub fn code(self) -> u8 {
        match self {
            AckRefusal::ExpiryTooTight => 1,
            AckRefusal::TooLarge => 2,
            AckRefusal::QueueFull => 3,
            AckRefusal::TipStale => 4,
            AckRefusal::BadFrame => 5,
        }
    }

    /// Parse an on-wire refusal code. `None` for `0` (there is no refusal) or any
    /// unknown value.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(AckRefusal::ExpiryTooTight),
            2 => Some(AckRefusal::TooLarge),
            3 => Some(AckRefusal::QueueFull),
            4 => Some(AckRefusal::TipStale),
            5 => Some(AckRefusal::BadFrame),
            _ => None,
        }
    }

    /// A stable machine-readable reason, safe to log. Carries no per-entry
    /// information.
    pub fn as_str(self) -> &'static str {
        match self {
            AckRefusal::ExpiryTooTight => "expiry_too_tight",
            AckRefusal::TooLarge => "too_large",
            AckRefusal::QueueFull => "queue_full",
            AckRefusal::TipStale => "tip_stale",
            AckRefusal::BadFrame => "bad_frame",
        }
    }
}

/// Map an admission refusal onto its wire form. `bad_frame` has no
/// `queue::Refusal` source because it is produced by this module on a decode
/// failure, before admission is ever reached.
impl From<crate::queue::Refusal> for AckRefusal {
    fn from(refusal: crate::queue::Refusal) -> Self {
        match refusal {
            crate::queue::Refusal::ExpiryTooTight => AckRefusal::ExpiryTooTight,
            crate::queue::Refusal::TooLarge => AckRefusal::TooLarge,
            crate::queue::Refusal::Full => AckRefusal::QueueFull,
            crate::queue::Refusal::TipStale => AckRefusal::TipStale,
        }
    }
}

/// Build a `SubmitV1` frame carrying `tx` under `nonce`, padded to
/// [`FRAME_BYTES`]. The buffer holds the transaction bytes, so it is
/// [`Zeroizing`]: a freed copy of a migration lingering in enclave memory is
/// exactly what attestation cannot excuse.
pub fn encode_submit(nonce: &Nonce, tx: &[u8]) -> Result<Zeroizing<Vec<u8>>, WireError> {
    if tx.len() > MAX_NYM_TX_BYTES {
        return Err(WireError::TxTooLarge { len: tx.len() });
    }
    let mut frame = Zeroizing::new(vec![0u8; FRAME_BYTES]);
    frame[0..4].copy_from_slice(&SUBMIT_MAGIC);
    frame[4..20].copy_from_slice(nonce);
    frame[20..24].copy_from_slice(&(tx.len() as u32).to_be_bytes());
    frame[SUBMIT_HEADER_BYTES..SUBMIT_HEADER_BYTES + tx.len()].copy_from_slice(tx);
    Ok(frame)
}

/// Read a `SubmitV1` frame back to its nonce and transaction bytes. Strict on the
/// header, silent on the padding (only the declared transaction region is read).
/// The returned transaction is [`Zeroizing`] for the same reason the encode
/// buffer is.
pub fn decode_submit(frame: &[u8]) -> Result<(Nonce, Zeroizing<Vec<u8>>), WireError> {
    if frame.len() != FRAME_BYTES {
        return Err(WireError::WrongLength {
            expected: FRAME_BYTES,
            got: frame.len(),
        });
    }
    if frame[0..4] != SUBMIT_MAGIC {
        return Err(WireError::BadMagic);
    }
    let mut nonce = [0u8; NONCE_BYTES];
    nonce.copy_from_slice(&frame[4..20]);
    let declared = u32::from_be_bytes([frame[20], frame[21], frame[22], frame[23]]) as usize;
    if declared > MAX_NYM_TX_BYTES {
        return Err(WireError::TxLenOverrunsFrame { declared });
    }
    let tx = Zeroizing::new(frame[SUBMIT_HEADER_BYTES..SUBMIT_HEADER_BYTES + declared].to_vec());
    Ok((nonce, tx))
}

/// Best-effort recovery of the request nonce from a frame that FAILED to decode,
/// so a `bad_frame` acknowledgement can still be correlated when the failure was
/// only in the `tx_len` field (the magic and nonce are intact). Returns `None`
/// when the frame is too short or lacks the submit magic, in which case there is
/// no trustworthy nonce and the sender falls back to its submit timeout.
pub fn peek_nonce(frame: &[u8]) -> Option<Nonce> {
    if frame.len() < SUBMIT_HEADER_BYTES || frame[0..4] != SUBMIT_MAGIC {
        return None;
    }
    let mut nonce = [0u8; NONCE_BYTES];
    nonce.copy_from_slice(&frame[4..20]);
    Some(nonce)
}

/// Build an `AckV1` frame echoing `nonce` and carrying `kind`, padded to
/// [`ACK_BYTES`]. No transaction bytes, so no zeroizing needed.
pub fn encode_ack(nonce: &Nonce, kind: AckKind) -> [u8; ACK_BYTES] {
    let mut frame = [0u8; ACK_BYTES];
    frame[0..4].copy_from_slice(&ACK_MAGIC);
    frame[4..20].copy_from_slice(nonce);
    let (disp, refusal) = match kind {
        AckKind::Accepted => (0u8, 0u8),
        AckKind::Refused(refusal) => (1u8, refusal.code()),
    };
    frame[20] = disp;
    frame[21] = refusal;
    frame
}

/// Read an `AckV1` frame back to its nonce and disposition.
pub fn decode_ack(frame: &[u8]) -> Result<(Nonce, AckKind), WireError> {
    if frame.len() != ACK_BYTES {
        return Err(WireError::WrongLength {
            expected: ACK_BYTES,
            got: frame.len(),
        });
    }
    if frame[0..4] != ACK_MAGIC {
        return Err(WireError::BadMagic);
    }
    let mut nonce = [0u8; NONCE_BYTES];
    nonce.copy_from_slice(&frame[4..20]);
    let (disp, refusal) = (frame[20], frame[21]);
    let kind = match disp {
        0 => {
            // Accepted rides with refusal byte 0; anything else is a malformed ack.
            if refusal != 0 {
                return Err(WireError::UnknownRefusal(refusal));
            }
            AckKind::Accepted
        }
        1 => AckKind::Refused(
            AckRefusal::from_code(refusal).ok_or(WireError::UnknownRefusal(refusal))?,
        ),
        other => return Err(WireError::UnknownDisposition(other)),
    };
    Ok((nonce, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Refusal;

    /// The committed golden vectors, byte-identical to the shim crate's copy. If
    /// this file and either crate's encoder disagree, the codecs have drifted and
    /// this test fails loudly. Regenerate with `regenerate_wire_vectors` (ignored).
    const VECTORS: &[u8] = include_bytes!("../tests/fixtures/wire_v1_vectors.bin");

    /// The canonical nonce for the golden vectors: 0xA0..0xAF.
    fn vector_nonce() -> Nonce {
        let mut n = [0u8; NONCE_BYTES];
        for (i, b) in n.iter_mut().enumerate() {
            *b = 0xA0 + i as u8;
        }
        n
    }

    /// The canonical transaction for the golden vectors: 0x00..0x3F (64 bytes).
    fn vector_tx() -> Vec<u8> {
        (0..64u16).map(|i| i as u8).collect()
    }

    /// Build the canonical vector stream: one SubmitV1, then AckV1 accepted and
    /// one AckV1 for every refusal, in code order.
    fn build_vectors() -> Vec<u8> {
        let nonce = vector_nonce();
        let mut out = Vec::new();
        out.extend_from_slice(&encode_submit(&nonce, &vector_tx()).expect("fits the frame"));
        out.extend_from_slice(&encode_ack(&nonce, AckKind::Accepted));
        for refusal in [
            AckRefusal::ExpiryTooTight,
            AckRefusal::TooLarge,
            AckRefusal::QueueFull,
            AckRefusal::TipStale,
            AckRefusal::BadFrame,
        ] {
            out.extend_from_slice(&encode_ack(&nonce, AckKind::Refused(refusal)));
        }
        out
    }

    #[test]
    fn the_encoder_reproduces_the_committed_golden_vectors() {
        // Byte-equality both pins this crate's encoder and, because the shim crate
        // commits the identical file and runs the identical assertion, proves the
        // two independent codecs agree on every byte, padding included.
        assert_eq!(build_vectors().as_slice(), VECTORS);
        assert_eq!(VECTORS.len(), FRAME_BYTES + 6 * ACK_BYTES);
    }

    #[test]
    fn a_submit_round_trips() {
        let nonce = vector_nonce();
        let tx = vector_tx();
        let frame = encode_submit(&nonce, &tx).unwrap();
        assert_eq!(frame.len(), FRAME_BYTES);
        let (got_nonce, got_tx) = decode_submit(&frame).unwrap();
        assert_eq!(got_nonce, nonce);
        assert_eq!(got_tx.as_slice(), tx.as_slice());
    }

    #[test]
    fn a_maximum_size_transaction_round_trips() {
        let nonce = vector_nonce();
        let tx = vec![0x5a; MAX_NYM_TX_BYTES];
        let frame = encode_submit(&nonce, &tx).unwrap();
        let (_, got_tx) = decode_submit(&frame).unwrap();
        assert_eq!(got_tx.len(), MAX_NYM_TX_BYTES);
        assert_eq!(got_tx.as_slice(), tx.as_slice());
    }

    #[test]
    fn a_transaction_over_the_budget_will_not_encode() {
        let nonce = vector_nonce();
        let tx = vec![0u8; MAX_NYM_TX_BYTES + 1];
        assert_eq!(
            encode_submit(&nonce, &tx),
            Err(WireError::TxTooLarge {
                len: MAX_NYM_TX_BYTES + 1
            })
        );
    }

    #[test]
    fn decode_is_lax_about_padding_and_strict_about_the_header() {
        let nonce = vector_nonce();
        let tx = vec![0x11; 5];
        let mut frame = encode_submit(&nonce, &tx).unwrap().to_vec();

        // Padding after the declared transaction is never read: dirtying it does
        // not change the decoded transaction.
        for byte in frame.iter_mut().skip(SUBMIT_HEADER_BYTES + tx.len()) {
            *byte = 0xff;
        }
        let (_, got_tx) = decode_submit(&frame).unwrap();
        assert_eq!(got_tx.as_slice(), tx.as_slice());

        // Wrong length, bad magic, and an overrunning tx_len are all rejected.
        assert!(matches!(
            decode_submit(&frame[..FRAME_BYTES - 1]),
            Err(WireError::WrongLength { .. })
        ));
        frame[0] ^= 0xff;
        assert_eq!(decode_submit(&frame), Err(WireError::BadMagic));
        frame[0] ^= 0xff;
        frame[20..24].copy_from_slice(&((MAX_NYM_TX_BYTES + 1) as u32).to_be_bytes());
        assert_eq!(
            decode_submit(&frame),
            Err(WireError::TxLenOverrunsFrame {
                declared: MAX_NYM_TX_BYTES + 1
            })
        );
    }

    #[test]
    fn every_ack_disposition_round_trips() {
        let nonce = vector_nonce();
        for kind in [
            AckKind::Accepted,
            AckKind::Refused(AckRefusal::ExpiryTooTight),
            AckKind::Refused(AckRefusal::TooLarge),
            AckKind::Refused(AckRefusal::QueueFull),
            AckKind::Refused(AckRefusal::TipStale),
            AckKind::Refused(AckRefusal::BadFrame),
        ] {
            let frame = encode_ack(&nonce, kind);
            assert_eq!(frame.len(), ACK_BYTES);
            let (got_nonce, got_kind) = decode_ack(&frame).unwrap();
            assert_eq!(got_nonce, nonce);
            assert_eq!(got_kind, kind);
        }
    }

    #[test]
    fn a_malformed_ack_is_rejected() {
        let nonce = vector_nonce();
        let good = encode_ack(&nonce, AckKind::Accepted);

        assert!(matches!(
            decode_ack(&good[..ACK_BYTES - 1]),
            Err(WireError::WrongLength { .. })
        ));

        let mut bad_magic = good;
        bad_magic[0] ^= 0xff;
        assert_eq!(decode_ack(&bad_magic), Err(WireError::BadMagic));

        let mut bad_disp = good;
        bad_disp[20] = 7;
        assert_eq!(decode_ack(&bad_disp), Err(WireError::UnknownDisposition(7)));

        let mut bad_refusal = good;
        bad_refusal[20] = 1;
        bad_refusal[21] = 99;
        assert_eq!(decode_ack(&bad_refusal), Err(WireError::UnknownRefusal(99)));
    }

    #[test]
    fn refusal_codes_are_stable_and_total() {
        for refusal in [
            AckRefusal::ExpiryTooTight,
            AckRefusal::TooLarge,
            AckRefusal::QueueFull,
            AckRefusal::TipStale,
            AckRefusal::BadFrame,
        ] {
            assert_eq!(AckRefusal::from_code(refusal.code()), Some(refusal));
        }
        // 0 is "none", never a refusal.
        assert_eq!(AckRefusal::from_code(0), None);
    }

    #[test]
    fn every_queue_refusal_maps_to_a_wire_refusal_with_the_same_reason() {
        // The listener builds a refused AckV1 straight from a `queue::Refusal`, so
        // the two enums must agree on the reason string, or an operator reads one
        // name on the hub and a different one at the shim.
        for refusal in [
            Refusal::ExpiryTooTight,
            Refusal::TooLarge,
            Refusal::Full,
            Refusal::TipStale,
        ] {
            assert_eq!(AckRefusal::from(refusal).as_str(), refusal.as_str());
        }
    }

    #[test]
    fn peek_nonce_recovers_only_when_the_frame_is_structurally_ours() {
        let nonce = vector_nonce();
        let mut frame = encode_submit(&nonce, &vector_tx()).unwrap().to_vec();
        // Only tx_len is wrong: decode fails, but the nonce is still recoverable
        // for a correlatable bad_frame ack.
        frame[20..24].copy_from_slice(&((MAX_NYM_TX_BYTES + 1) as u32).to_be_bytes());
        assert!(decode_submit(&frame).is_err());
        assert_eq!(peek_nonce(&frame), Some(nonce));
        // Wrong magic or too short: no trustworthy nonce.
        frame[0] ^= 0xff;
        assert_eq!(peek_nonce(&frame), None);
        assert_eq!(peek_nonce(&[0u8; 10]), None);
    }

    /// Rewrite the committed golden-vector file from the current encoder. Ignored
    /// because it writes into the source tree; run deliberately with
    /// `cargo test regenerate_wire_vectors -- --ignored`, then copy the file to
    /// the shim crate's fixtures so both stay byte-identical.
    #[test]
    #[ignore = "writes tests/fixtures/wire_v1_vectors.bin"]
    fn regenerate_wire_vectors() {
        std::fs::write("tests/fixtures/wire_v1_vectors.bin", build_vectors())
            .expect("write the golden vectors");
    }
}

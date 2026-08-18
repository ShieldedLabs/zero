//! The shim-to-hub wire frames, version 1: `SubmitV1` in and `AckV1` back for
//! a diverted migration; `LookupV1` in and `LookupReplyV1` back for a
//! hub-served `GetTransaction` (the shim is stateless and routes every lookup
//! to the hub).
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
//! * **Fixed size.** Every `SubmitV1` and every `LookupReplyV1` is exactly
//!   [`FRAME_BYTES`], every `AckV1` is exactly [`ACK_BYTES`], and every
//!   `LookupV1` is exactly [`LOOKUP_BYTES`], padded with zeros, so a record's
//!   length carries no information to any layer that can see it. In particular
//!   a lookup reply's size hides found-versus-not-found as well as the
//!   transaction's true length, which lives in the `tx_len` field, read only
//!   after the frame is decrypted.
//! * **No txid as a correlation handle.** Correlation between a request and its
//!   reply is by a random 16-byte nonce the shim mints, never a txid (the txid
//!   is a control input an adversary could write; both ends compute it from the
//!   bytes instead). The nonce is echoed in the reply and matched there. The
//!   one hash the wire does carry is `LookupV1`'s queried hash, which IS the
//!   query, not a handle: the reply echoes only the nonce.
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
//!
//! LookupV1, exactly LOOKUP_BYTES:
//!   0    magic     4   b"ZNL1"
//!   4    nonce    16   request nonce, from OsRng
//!   20   hash_len  1   length of the queried hash (normally 32)
//!   21   hash      hash_len bytes, wire order
//!   ..   padding   zeros to LOOKUP_BYTES
//!
//! LookupReplyV1, exactly FRAME_BYTES:
//!   0    magic     4   b"ZNR1"
//!   4    nonce    16   echoed request nonce
//!   20   disp      1   0 found, 1 not_found, 2 error
//!   21   height    8   u64 big-endian, 0 = mempool
//!   29   tx_len    4   u32 big-endian
//!   33   tx        tx_len bytes (found only)
//!   ..   padding   zeros to FRAME_BYTES
//! ```
//!
//! Decode is strict about the header and deliberately lax about the padding: a
//! wrong total length, a bad magic, a length field that overruns the frame, or
//! a reply whose disposition and payload fields disagree is a [`WireError`],
//! which the listener answers as `bad_frame` for a bad request, but the bytes
//! past the declared region are never read, so nothing downstream can smuggle
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

/// The fixed on-wire size of every `LookupV1`. Small on purpose: the request
/// carries only a hash, and padding it to [`FRAME_BYTES`] would spend a full
/// frame of bandwidth per wallet poll to hide only "this shim did a lookup",
/// which the operator already infers from migration activity. The REPLY is the
/// side whose content must be hidden, and it is a full frame.
pub const LOOKUP_BYTES: usize = 64;

/// The request nonce is 16 bytes.
pub const NONCE_BYTES: usize = 16;

/// A 16-byte request nonce, minted per submission by the shim and echoed in the
/// ack. It is the ONLY correlation handle on the wire.
pub type Nonce = [u8; NONCE_BYTES];

/// `SubmitV1` magic. The final byte is the version.
const SUBMIT_MAGIC: [u8; 4] = *b"ZNS1";

/// `AckV1` magic. The final byte is the version.
const ACK_MAGIC: [u8; 4] = *b"ZNA1";

/// `LookupV1` magic. The final byte is the version.
const LOOKUP_MAGIC: [u8; 4] = *b"ZNL1";

/// `LookupReplyV1` magic. The final byte is the version.
const LOOKUP_REPLY_MAGIC: [u8; 4] = *b"ZNR1";

/// magic (4) + nonce (16) + tx_len (4).
const SUBMIT_HEADER_BYTES: usize = 24;

/// magic (4) + nonce (16) + hash_len (1).
const LOOKUP_HEADER_BYTES: usize = 21;

/// magic (4) + nonce (16) + disp (1) + height (8) + tx_len (4).
const LOOKUP_REPLY_HEADER_BYTES: usize = 33;

/// The largest transaction this transport will carry. A transaction larger than
/// this cannot be privately batched, which is the price of leaking zero bits of
/// length; the shim surfaces it to the wallet as an error rather than sending it.
///
/// Bounded by the LOOKUP REPLY header, not the submit header, even though it
/// gates a submit. The reply header is nine bytes wider, so a transaction sized
/// between the two budgets could be admitted and then never served back: this
/// hub would answer every later lookup `error`, and the wallet would see
/// UNAVAILABLE forever for a migration that was accepted and will be published.
/// Spending nine bytes of an unreachable budget makes that window
/// unrepresentable rather than merely undocumented.
pub const MAX_NYM_TX_BYTES: usize = FRAME_BYTES - LOOKUP_REPLY_HEADER_BYTES;

/// The largest hash a `LookupV1` can carry (normally 32 bytes, a txid in wire
/// order).
pub const MAX_LOOKUP_HASH_BYTES: usize = LOOKUP_BYTES - LOOKUP_HEADER_BYTES;

/// The largest transaction a `LookupReplyV1` can serve. Nine bytes SMALLER than
/// [`MAX_NYM_TX_BYTES`], because the reply header carries `disp` and `height`
/// that the submit header does not; a transaction admitted near the submit cap
/// therefore cannot be served back, and the lookup arm answers it `error`,
/// which fails closed at the shim. Unreachable for migrations sized under both
/// caps.
pub const MAX_LOOKUP_REPLY_TX_BYTES: usize = FRAME_BYTES - LOOKUP_REPLY_HEADER_BYTES;

/// Why a frame could not be built or read. Every decode failure means the same
/// thing to the listener (answer `bad_frame`); the variants exist so the reason
/// can be logged without a per-entry identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The buffer was not exactly the fixed frame size.
    WrongLength { expected: usize, got: usize },
    /// The 4-byte magic did not match.
    BadMagic,
    /// Encode side: the transaction is larger than the frame's budget
    /// ([`MAX_NYM_TX_BYTES`] for a submit, [`MAX_LOOKUP_REPLY_TX_BYTES`] for a
    /// lookup reply).
    TxTooLarge { len: usize, budget: usize },
    /// Decode side: the declared `tx_len` runs past the end of the frame.
    TxLenOverrunsFrame { declared: usize },
    /// Encode side: the queried hash is larger than [`MAX_LOOKUP_HASH_BYTES`].
    HashTooLarge { len: usize },
    /// Decode side: the declared `hash_len` runs past the end of the frame.
    HashLenOverrunsFrame { declared: usize },
    /// Decode side: the disposition byte was not a known value for the frame.
    UnknownDisposition(u8),
    /// Decode side: the refusal byte was not a known [`AckRefusal`] code.
    UnknownRefusal(u8),
    /// Decode side: a `not_found` or `error` lookup reply carried a nonzero
    /// height or `tx_len`, exactly as forbidden as an accepted ack with a
    /// nonzero refusal byte.
    StrayReplyPayload,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::WrongLength { expected, got } => {
                write!(f, "wrong frame length: expected {expected}, got {got}")
            }
            WireError::BadMagic => f.write_str("bad frame magic"),
            // The BUDGET, never the transaction's own length. This string ends
            // up in a log, and in an enclave a log reaches the parent host: a
            // refused migration's exact size is precisely the kind of bit D4
            // and REVIEW #12 exist to keep off that channel. The length stays
            // in the struct for a caller that legitimately needs it.
            WireError::TxTooLarge { budget, .. } => {
                write!(f, "transaction exceeds the {budget}-byte frame budget")
            }
            WireError::TxLenOverrunsFrame { declared } => {
                write!(f, "declared tx_len {declared} overruns the frame")
            }
            WireError::HashTooLarge { .. } => {
                write!(
                    f,
                    "hash exceeds the {MAX_LOOKUP_HASH_BYTES}-byte lookup budget"
                )
            }
            WireError::HashLenOverrunsFrame { declared } => {
                write!(f, "declared hash_len {declared} overruns the frame")
            }
            WireError::UnknownDisposition(byte) => write!(f, "unknown disposition byte {byte}"),
            WireError::UnknownRefusal(byte) => write!(f, "unknown refusal byte {byte}"),
            WireError::StrayReplyPayload => {
                f.write_str("not_found/error reply carries a height or transaction bytes")
            }
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
            // Deliberately NOT a new wire code: see `Refusal::Draining`. A shim
            // reacts to a draining hub exactly as it reacts to a full one.
            crate::queue::Refusal::Draining => AckRefusal::QueueFull,
        }
    }
}

/// Build a `SubmitV1` frame carrying `tx` under `nonce`, padded to
/// [`FRAME_BYTES`]. The buffer holds the transaction bytes, so it is
/// [`Zeroizing`]: a freed copy of a migration lingering in enclave memory is
/// exactly what attestation cannot excuse.
pub fn encode_submit(nonce: &Nonce, tx: &[u8]) -> Result<Zeroizing<Vec<u8>>, WireError> {
    if tx.len() > MAX_NYM_TX_BYTES {
        return Err(WireError::TxTooLarge {
            len: tx.len(),
            budget: MAX_NYM_TX_BYTES,
        });
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

/// The disposition a `LookupReplyV1` carries. `Found` serves the transaction at
/// `height` (`0` is the mempool sentinel, matching the HTTP path's
/// `x-tx-height`); `NotFound` maps onto the gRPC not-found the operator's
/// indexer would return; `Error` fails CLOSED at the shim (UNAVAILABLE to the
/// wallet, never the operator's indexer).
#[derive(Debug, Clone)]
pub enum LookupReply {
    /// The hub (queue, then indexer) found the transaction at `height`
    /// (`0` = mempool).
    Found { height: u64, tx: Zeroizing<Vec<u8>> },
    /// The hub does not know the transaction.
    NotFound,
    /// The hub could not answer. The shim fails closed.
    Error,
}

// Zeroizing does not carry PartialEq through, so compare contents by hand.
impl PartialEq for LookupReply {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                LookupReply::Found {
                    height: a,
                    tx: a_tx,
                },
                LookupReply::Found {
                    height: b,
                    tx: b_tx,
                },
            ) => a == b && a_tx.as_slice() == b_tx.as_slice(),
            (LookupReply::NotFound, LookupReply::NotFound) => true,
            (LookupReply::Error, LookupReply::Error) => true,
            _ => false,
        }
    }
}

impl Eq for LookupReply {}

/// Build a `LookupV1` frame querying `hash` under `nonce`, padded to
/// [`LOOKUP_BYTES`]. The hash is the wallet's `TxFilter.hash` in wire order,
/// normally 32 bytes; it is the query itself, the one hash the wire carries by
/// design. No transaction bytes, so no zeroizing needed.
pub fn encode_lookup(nonce: &Nonce, hash: &[u8]) -> Result<[u8; LOOKUP_BYTES], WireError> {
    if hash.len() > MAX_LOOKUP_HASH_BYTES {
        return Err(WireError::HashTooLarge { len: hash.len() });
    }
    let mut frame = [0u8; LOOKUP_BYTES];
    frame[0..4].copy_from_slice(&LOOKUP_MAGIC);
    frame[4..20].copy_from_slice(nonce);
    frame[20] = hash.len() as u8;
    frame[LOOKUP_HEADER_BYTES..LOOKUP_HEADER_BYTES + hash.len()].copy_from_slice(hash);
    Ok(frame)
}

/// Read a `LookupV1` frame back to its nonce and queried hash. Strict on the
/// header, silent on the padding (only the declared hash region is read).
pub fn decode_lookup(frame: &[u8]) -> Result<(Nonce, Vec<u8>), WireError> {
    if frame.len() != LOOKUP_BYTES {
        return Err(WireError::WrongLength {
            expected: LOOKUP_BYTES,
            got: frame.len(),
        });
    }
    if frame[0..4] != LOOKUP_MAGIC {
        return Err(WireError::BadMagic);
    }
    let mut nonce = [0u8; NONCE_BYTES];
    nonce.copy_from_slice(&frame[4..20]);
    let declared = frame[20] as usize;
    if declared > MAX_LOOKUP_HASH_BYTES {
        return Err(WireError::HashLenOverrunsFrame { declared });
    }
    let hash = frame[LOOKUP_HEADER_BYTES..LOOKUP_HEADER_BYTES + declared].to_vec();
    Ok((nonce, hash))
}

/// Best-effort recovery of the request nonce from a `LookupV1` that FAILED to
/// decode, mirroring [`peek_nonce`]: when only the `hash_len` field is bad the
/// listener can still answer a correlatable `error` reply. `None` when the
/// frame is too short or lacks the lookup magic.
pub fn peek_lookup_nonce(frame: &[u8]) -> Option<Nonce> {
    if frame.len() < LOOKUP_HEADER_BYTES || frame[0..4] != LOOKUP_MAGIC {
        return None;
    }
    let mut nonce = [0u8; NONCE_BYTES];
    nonce.copy_from_slice(&frame[4..20]);
    Some(nonce)
}

/// Build a `LookupReplyV1` echoing `nonce` and carrying `reply`, padded to
/// [`FRAME_BYTES`] so the reply's size hides found-versus-not-found and the
/// transaction's length alike. The buffer can hold transaction bytes, so it is
/// [`Zeroizing`] like the submit frame.
pub fn encode_lookup_reply(
    nonce: &Nonce,
    reply: &LookupReply,
) -> Result<Zeroizing<Vec<u8>>, WireError> {
    let mut frame = Zeroizing::new(vec![0u8; FRAME_BYTES]);
    frame[0..4].copy_from_slice(&LOOKUP_REPLY_MAGIC);
    frame[4..20].copy_from_slice(nonce);
    match reply {
        LookupReply::Found { height, tx } => {
            if tx.len() > MAX_LOOKUP_REPLY_TX_BYTES {
                return Err(WireError::TxTooLarge {
                    len: tx.len(),
                    budget: MAX_LOOKUP_REPLY_TX_BYTES,
                });
            }
            frame[20] = 0;
            frame[21..29].copy_from_slice(&height.to_be_bytes());
            frame[29..33].copy_from_slice(&(tx.len() as u32).to_be_bytes());
            frame[LOOKUP_REPLY_HEADER_BYTES..LOOKUP_REPLY_HEADER_BYTES + tx.len()]
                .copy_from_slice(tx);
        }
        LookupReply::NotFound => frame[20] = 1,
        LookupReply::Error => frame[20] = 2,
    }
    Ok(frame)
}

/// Read a `LookupReplyV1` back to its nonce and disposition. Strict on every
/// header field: a `not_found` or `error` that smuggles a height or transaction
/// bytes is rejected, exactly as an accepted ack with a nonzero refusal byte
/// is. The returned transaction is [`Zeroizing`].
pub fn decode_lookup_reply(frame: &[u8]) -> Result<(Nonce, LookupReply), WireError> {
    if frame.len() != FRAME_BYTES {
        return Err(WireError::WrongLength {
            expected: FRAME_BYTES,
            got: frame.len(),
        });
    }
    if frame[0..4] != LOOKUP_REPLY_MAGIC {
        return Err(WireError::BadMagic);
    }
    let mut nonce = [0u8; NONCE_BYTES];
    nonce.copy_from_slice(&frame[4..20]);
    let disp = frame[20];
    let height = u64::from_be_bytes(frame[21..29].try_into().expect("eight bytes"));
    let declared = u32::from_be_bytes([frame[29], frame[30], frame[31], frame[32]]) as usize;
    match disp {
        0 => {
            if declared > MAX_LOOKUP_REPLY_TX_BYTES {
                return Err(WireError::TxLenOverrunsFrame { declared });
            }
            let tx = Zeroizing::new(
                frame[LOOKUP_REPLY_HEADER_BYTES..LOOKUP_REPLY_HEADER_BYTES + declared].to_vec(),
            );
            Ok((nonce, LookupReply::Found { height, tx }))
        }
        1 | 2 => {
            if height != 0 || declared != 0 {
                return Err(WireError::StrayReplyPayload);
            }
            let reply = if disp == 1 {
                LookupReply::NotFound
            } else {
                LookupReply::Error
            };
            Ok((nonce, reply))
        }
        other => Err(WireError::UnknownDisposition(other)),
    }
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

    /// The canonical queried hash for the golden vectors: 0xC0..0xDF (32 bytes).
    fn vector_hash() -> Vec<u8> {
        (0..32u16).map(|i| 0xC0 + i as u8).collect()
    }

    /// The canonical vector-stream lookup replies, in disposition order with the
    /// mempool sentinel covered: found at a height, found in the mempool,
    /// not_found, error.
    fn vector_replies() -> [LookupReply; 4] {
        [
            LookupReply::Found {
                height: 778_899,
                tx: Zeroizing::new(vector_tx()),
            },
            LookupReply::Found {
                height: 0,
                tx: Zeroizing::new(vector_tx()),
            },
            LookupReply::NotFound,
            LookupReply::Error,
        ]
    }

    /// Build the canonical vector stream: one SubmitV1, AckV1 accepted and one
    /// AckV1 for every refusal in code order, then one LookupV1 and one
    /// LookupReplyV1 for every disposition (found at a height, found in the
    /// mempool, not_found, error).
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
        out.extend_from_slice(&encode_lookup(&nonce, &vector_hash()).expect("fits the frame"));
        for reply in vector_replies() {
            out.extend_from_slice(&encode_lookup_reply(&nonce, &reply).expect("fits the frame"));
        }
        out
    }

    #[test]
    fn the_encoder_reproduces_the_committed_golden_vectors() {
        // Byte-equality both pins this crate's encoder and, because the shim crate
        // commits the identical file and runs the identical assertion, proves the
        // two independent codecs agree on every byte, padding included.
        assert_eq!(build_vectors().as_slice(), VECTORS);
        assert_eq!(
            VECTORS.len(),
            5 * FRAME_BYTES + 6 * ACK_BYTES + LOOKUP_BYTES
        );
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
    fn an_over_budget_error_never_renders_the_transactions_own_size() {
        // These strings reach a log, and in an enclave a log reaches the parent
        // host. The budget is a constant the operator already knows; the
        // transaction's size is a bit about a wallet's activity that D4 and
        // REVIEW #12 exist to keep off that channel.
        let secret_len = MAX_NYM_TX_BYTES + 12_345;
        let rendered = WireError::TxTooLarge {
            len: secret_len,
            budget: MAX_NYM_TX_BYTES,
        }
        .to_string();
        assert!(
            !rendered.contains(&secret_len.to_string()),
            "the length leaked into: {rendered}"
        );
        assert!(rendered.contains(&MAX_NYM_TX_BYTES.to_string()));

        let hash_len = MAX_LOOKUP_HASH_BYTES + 7;
        let rendered = WireError::HashTooLarge { len: hash_len }.to_string();
        assert!(
            !rendered.contains(&hash_len.to_string()),
            "the length leaked into: {rendered}"
        );
    }

    #[test]
    fn the_submit_budget_is_bounded_by_what_a_lookup_reply_can_carry() {
        // Otherwise a transaction between the two budgets is admissible and
        // then permanently unlookupable: accepted, published, and answered
        // `error` on every later lookup, so the wallet sees UNAVAILABLE forever
        // for a migration that really was accepted.
        assert!(MAX_NYM_TX_BYTES <= MAX_LOOKUP_REPLY_TX_BYTES);
        let at_cap = LookupReply::Found {
            height: 1,
            tx: Zeroizing::new(vec![0x5a; MAX_NYM_TX_BYTES]),
        };
        assert!(
            encode_lookup_reply(&vector_nonce(), &at_cap).is_ok(),
            "anything the submit gate admits must fit a reply frame"
        );
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
                len: MAX_NYM_TX_BYTES + 1,
                budget: MAX_NYM_TX_BYTES
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

    #[test]
    fn a_lookup_round_trips() {
        let nonce = vector_nonce();
        let hash = vector_hash();
        let frame = encode_lookup(&nonce, &hash).unwrap();
        assert_eq!(frame.len(), LOOKUP_BYTES);
        let (got_nonce, got_hash) = decode_lookup(&frame).unwrap();
        assert_eq!(got_nonce, nonce);
        assert_eq!(got_hash, hash);
    }

    #[test]
    fn boundary_hashes_round_trip() {
        // An empty hash is structurally valid (the lookup arm decides what it
        // means), and the maximum fills the frame exactly.
        let nonce = vector_nonce();
        for len in [0, MAX_LOOKUP_HASH_BYTES] {
            let hash = vec![0x77; len];
            let frame = encode_lookup(&nonce, &hash).unwrap();
            let (_, got_hash) = decode_lookup(&frame).unwrap();
            assert_eq!(got_hash, hash);
        }
    }

    #[test]
    fn a_hash_over_the_budget_will_not_encode() {
        let nonce = vector_nonce();
        let hash = vec![0u8; MAX_LOOKUP_HASH_BYTES + 1];
        assert_eq!(
            encode_lookup(&nonce, &hash),
            Err(WireError::HashTooLarge {
                len: MAX_LOOKUP_HASH_BYTES + 1
            })
        );
    }

    #[test]
    fn lookup_decode_is_lax_about_padding_and_strict_about_the_header() {
        let nonce = vector_nonce();
        let hash = vec![0x22; 5];
        let mut frame = encode_lookup(&nonce, &hash).unwrap();

        // Padding after the declared hash is never read.
        for byte in frame.iter_mut().skip(LOOKUP_HEADER_BYTES + hash.len()) {
            *byte = 0xff;
        }
        let (_, got_hash) = decode_lookup(&frame).unwrap();
        assert_eq!(got_hash, hash);

        // Wrong length, bad magic, and an overrunning hash_len are all rejected.
        assert!(matches!(
            decode_lookup(&frame[..LOOKUP_BYTES - 1]),
            Err(WireError::WrongLength { .. })
        ));
        frame[0] ^= 0xff;
        assert_eq!(decode_lookup(&frame), Err(WireError::BadMagic));
        frame[0] ^= 0xff;
        frame[20] = (MAX_LOOKUP_HASH_BYTES + 1) as u8;
        assert_eq!(
            decode_lookup(&frame),
            Err(WireError::HashLenOverrunsFrame {
                declared: MAX_LOOKUP_HASH_BYTES + 1
            })
        );
    }

    #[test]
    fn every_lookup_reply_disposition_round_trips() {
        let nonce = vector_nonce();
        for reply in vector_replies() {
            let frame = encode_lookup_reply(&nonce, &reply).unwrap();
            assert_eq!(frame.len(), FRAME_BYTES);
            let (got_nonce, got_reply) = decode_lookup_reply(&frame).unwrap();
            assert_eq!(got_nonce, nonce);
            assert_eq!(got_reply, reply);
        }
    }

    #[test]
    fn a_maximum_size_reply_transaction_round_trips() {
        let nonce = vector_nonce();
        let reply = LookupReply::Found {
            height: 42,
            tx: Zeroizing::new(vec![0x5a; MAX_LOOKUP_REPLY_TX_BYTES]),
        };
        let frame = encode_lookup_reply(&nonce, &reply).unwrap();
        let (_, got_reply) = decode_lookup_reply(&frame).unwrap();
        assert_eq!(got_reply, reply);
    }

    #[test]
    fn a_reply_transaction_over_the_budget_will_not_encode() {
        // The reply budget is nine bytes tighter than the submit budget (its
        // header carries disp and height); the lookup arm maps this onto an
        // `error` reply, which fails closed at the shim.
        let nonce = vector_nonce();
        let reply = LookupReply::Found {
            height: 42,
            tx: Zeroizing::new(vec![0u8; MAX_LOOKUP_REPLY_TX_BYTES + 1]),
        };
        assert_eq!(
            encode_lookup_reply(&nonce, &reply),
            Err(WireError::TxTooLarge {
                len: MAX_LOOKUP_REPLY_TX_BYTES + 1,
                budget: MAX_LOOKUP_REPLY_TX_BYTES
            })
        );
    }

    #[test]
    fn a_malformed_lookup_reply_is_rejected() {
        let nonce = vector_nonce();
        let good = encode_lookup_reply(&nonce, &LookupReply::NotFound).unwrap();

        assert!(matches!(
            decode_lookup_reply(&good[..FRAME_BYTES - 1]),
            Err(WireError::WrongLength { .. })
        ));

        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xff;
        assert_eq!(decode_lookup_reply(&bad_magic), Err(WireError::BadMagic));

        let mut bad_disp = good.clone();
        bad_disp[20] = 3;
        assert_eq!(
            decode_lookup_reply(&bad_disp),
            Err(WireError::UnknownDisposition(3))
        );

        // A found whose tx_len overruns the frame is rejected.
        let found = encode_lookup_reply(
            &nonce,
            &LookupReply::Found {
                height: 1,
                tx: Zeroizing::new(vec![0x11; 5]),
            },
        )
        .unwrap();
        let mut overrun = found.clone();
        overrun[29..33].copy_from_slice(&((MAX_LOOKUP_REPLY_TX_BYTES + 1) as u32).to_be_bytes());
        assert_eq!(
            decode_lookup_reply(&overrun),
            Err(WireError::TxLenOverrunsFrame {
                declared: MAX_LOOKUP_REPLY_TX_BYTES + 1
            })
        );

        // not_found or error smuggling a height or transaction bytes is exactly
        // as forbidden as an accepted ack with a nonzero refusal byte.
        let mut stray_height = good.clone();
        stray_height[21..29].copy_from_slice(&7u64.to_be_bytes());
        assert_eq!(
            decode_lookup_reply(&stray_height),
            Err(WireError::StrayReplyPayload)
        );
        let mut stray_tx = encode_lookup_reply(&nonce, &LookupReply::Error).unwrap();
        stray_tx[29..33].copy_from_slice(&5u32.to_be_bytes());
        assert_eq!(
            decode_lookup_reply(&stray_tx),
            Err(WireError::StrayReplyPayload)
        );
    }

    #[test]
    fn peek_lookup_nonce_recovers_only_when_the_frame_is_structurally_ours() {
        let nonce = vector_nonce();
        let mut frame = encode_lookup(&nonce, &vector_hash()).unwrap();
        // Only hash_len is wrong: decode fails, but the nonce is still
        // recoverable for a correlatable error reply.
        frame[20] = (MAX_LOOKUP_HASH_BYTES + 1) as u8;
        assert!(decode_lookup(&frame).is_err());
        assert_eq!(peek_lookup_nonce(&frame), Some(nonce));
        // Wrong magic or too short: no trustworthy nonce. A submit frame is not
        // a lookup frame, and vice versa.
        assert_eq!(
            peek_lookup_nonce(&encode_submit(&nonce, &[]).unwrap()),
            None
        );
        assert_eq!(peek_nonce(&frame), None);
        frame[0] ^= 0xff;
        assert_eq!(peek_lookup_nonce(&frame), None);
        assert_eq!(peek_lookup_nonce(&[0u8; 10]), None);
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

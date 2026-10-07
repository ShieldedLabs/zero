// -*- mode: Bluespec; -*-

/// The shim-to-hub wire, at the level of what a frame can say.
///
/// Four frames exist: a submission and its ack, a lookup and its reply. Byte
/// layout, padding and malformed frames are below this level. What is modelled
/// is the part with protocol consequences: which distinctions a hub's answer
/// keeps when it is put on the wire, and how the shim reads it back.
module wire {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"

  /// The refusal codes an ack can carry. There are four, for five refusals.
  type WireRefusal = WExpiryTooTight | WTooLarge | WQueueFull | WTipStale

  /// An ack's disposition. A fresh admission and a duplicate are the same ack.
  type WireAck = WAccepted | WRefused(WireRefusal)

  /// A lookup reply's disposition. Only `WFound` has room for a height and a
  /// body; a `not_found` or an `error` that carries either does not decode.
  type WireReply =
    | WFound({ body: Option[Payload], height: Height })
    | WNotFound
    | WError

  /// A frame. Requests and replies are matched by `nonce` and by nothing else.
  type Msg =
    | Submit({ nonce: Nonce, payload: Payload })
    | Ack({ nonce: Nonce, ack: WireAck })
    | Lookup({ nonce: Nonce, txid: TxId })
    | LookupReply({ nonce: Nonce, reply: WireReply })

  /// The on-wire length of a frame. Every frame of a kind is padded to one
  /// length, so length reveals the kind and nothing about the content.
  type SizeClass = FrameSize | AckSize | LookupSize

  pure def sizeOf(msg: Msg): SizeClass =
    match msg {
      | Submit(_) => FrameSize
      | LookupReply(_) => FrameSize
      | Ack(_) => AckSize
      | Lookup(_) => LookupSize
    }

  /// A hub's decision as an ack. A draining hub answers as a full one does:
  /// the shim reacts to both the same way.
  pure def renderAck(kind: AckKind): WireAck =
    match kind {
      | Admitted => WAccepted
      | Duplicate => WAccepted
      | Refused(refusal) =>
        match refusal {
          | TipStale => WRefused(WTipStale)
          | HubDraining => WRefused(WQueueFull)
          | TooLarge => WRefused(WTooLarge)
          | ExpiryTooTight => WRefused(WExpiryTooTight)
          | Full => WRefused(WQueueFull)
        }
    }

  /// A hub's lookup outcome as a reply. A queue hit is "found, height 0, no
  /// body": the hub confirms the transaction is pending without handing its
  /// bytes to whoever asked. An indexer answer is forwarded as it came.
  pure def render(outcome: HubOutcome): WireReply =
    match outcome {
      | QueueHit => WFound({ body: None, height: MEMPOOL_HEIGHT })
      | FromIndexer(answer) =>
        match answer {
          | IFound(found) => WFound(found)
          | INotFound => WNotFound
          | IUnavailable => WError
        }
    }

  /// What a hub outcome is meant to tell a wallet asking for `query`. This is
  /// the intent, written without reference to the wire; `interpretReply` after
  /// `render` is the mechanism, and the two agree on every outcome a queue or
  /// an honest indexer produces.
  ///
  /// They disagree on one outcome an honest indexer never produces: an indexer
  /// answer of "found, height 0, no body". It means nothing was returned, and
  /// on the wire it is the hub's own "queued here", so the wallet is told
  /// pending. The encoding is not injective there, and the shim cannot tell.
  pure def meaning(outcome: HubOutcome, query: TxId): LookupObs =
    match outcome {
      | QueueHit => Pending
      | FromIndexer(answer) =>
        match answer {
          | IFound(found) =>
            match found.body {
              | Some(payload) =>
                if (payload.txid == Some(query))
                  Tx({ payload: payload, height: found.height })
                else NotFound
              // An indexer that reports a transaction and returns none has
              // told the wallet nothing.
              | None => NotFound
            }
          | INotFound => NotFound
          | IUnavailable => Unavailable
        }
    }

  /// How the shim reads a lookup reply for `query`. The arms, in order:
  ///
  /// 1. found, height 0, no body: relayed as pending;
  /// 2. found otherwise: served only if the returned bytes parse and their
  ///    txid is the one asked for, else not found. Nothing else is compared:
  ///    not the bytes, so a twin passes, and not the height;
  /// 3. not found;
  /// 4. error: the lookup fails closed, and is never reported as not found.
  pure def interpretReply(reply: WireReply, query: TxId): LookupObs =
    match reply {
      | WFound(found) =>
        match found.body {
          | None => if (found.height == MEMPOOL_HEIGHT) Pending else NotFound
          | Some(payload) =>
            if (payload.txid == Some(query))
              Tx({ payload: payload, height: found.height })
            else NotFound
        }
      | WNotFound => NotFound
      | WError => Unavailable
    }
}

// -*- mode: Bluespec; -*-

/// The hub as the protocol sees it: which payloads it has queued, which are
/// out with a flush, and what it puts on the wire. No phase, no tip, no
/// schedule.
///
/// `hubTest` checks that every step of the real hub from a reachable state is
/// a step of this one (the abstraction lemma), so a protocol invariant that
/// holds over this hub, and reads only queue membership and wire replies,
/// holds over the real one. The converse does not hold: this hub answers
/// where the real one would refuse or error, so a reachable state here need
/// not be reachable there.
module abstractHub {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"
  import wire.* from "./wire"

  type AHub = { queue: Set[Payload], held: Set[Payload] }

  /// A client's request, with the indexer's answer for a lookup.
  type ARequest =
    | ASubmit(Payload)
    | ALookup({ txid: TxId, answer: IndexerAnswer })

  type AReply = AAck(WireAck) | AWire(WireReply)

  /// A request's effect: the hub after it, and its reply.
  type AAnswer = { hub: AHub, reply: AReply }

  pure val emptyAHub: AHub = { queue: Set(), held: Set() }

  pure val WIRE_ACKS: Set[WireAck] =
    Set(WAccepted, WRefused(WExpiryTooTight), WRefused(WQueueFull), WRefused(WTipStale))

  /// An honest hub accepts a submission into its queue or refuses it under any
  /// code, and answers a lookup from its queue first and its indexer second.
  pure def honestAnswers(h: AHub, request: ARequest): Set[AAnswer] =
    match request {
      | ASubmit(payload) =>
        WIRE_ACKS.map(ack =>
          { hub: if (ack == WAccepted) { ...h, queue: h.queue.union(Set(payload)) } else h, reply: AAck(ack) })
      | ALookup(lookup) =>
        val hit = h.queue.exists(payload => payload.txid == Some(lookup.txid))
        val outcome = if (hit) QueueHit else FromIndexer(lookup.answer)
        Set({ hub: h, reply: AWire(render(outcome)) })
    }

  /// A Byzantine hub answers a lookup with anything: a body from `universe` or
  /// none, at any height, or not found, or an error. A submission it accepts
  /// or refuses as it likes, which the honest relation already allows; its ack
  /// says which.
  pure def byzantineAnswers(h: AHub, request: ARequest, universe: Set[Payload]): Set[AAnswer] =
    match request {
      | ASubmit(_) => honestAnswers(h, request)
      | ALookup(_) =>
        tuples(Set(None).union(universe.map(payload => Some(payload))), WIRE_HEIGHTS)
          .map(((body, claimed)) => WFound({ body: body, height: claimed }))
          .union(Set(WNotFound, WError))
          .map(reply => { hub: h, reply: AWire(reply) })
    }

  /// The moves no client asks for: the queue goes out with a flush; an entry
  /// out with it is settled; what is kept comes back; everything is lost.
  pure def take(h: AHub): AHub = { queue: Set(), held: h.held.union(h.queue) }
  pure def settle(h: AHub, payload: Payload): AHub = { ...h, held: h.held.exclude(Set(payload)) }
  pure def giveBack(h: AHub, kept: Set[Payload]): AHub = { queue: h.queue.union(kept), held: Set() }

  pure def isInternal(h: AHub, after: AHub): bool =
    or {
      after == h,
      after == h.take(),
      h.held.exists(payload => after == h.settle(payload)),
      after.held == Set() and after.queue.subseteq(h.queue.union(h.held)) and h.queue.subseteq(after.queue),
      after == emptyAHub,
    }
}

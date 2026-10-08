// -*- mode: Bluespec; -*-

/// The wire layer, checked exhaustively over a small universe of payloads,
/// heights and queries.
module wireTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"

  pure val pA = { id: "a", txid: Some("ta"), created: 1, expiry: Some(9), class: OrchardTouching, oversize: false }
  pure val pATwin = { ...pA, id: "a-twin" }
  pure val pB = { id: "b", txid: Some("tb"), created: 1, expiry: None, class: PassThrough, oversize: false }
  pure val pJunk = { id: "junk", txid: None, created: 1, expiry: None, class: Unparseable, oversize: false }

  pure val PAYLOADS = Set(pA, pATwin, pB, pJunk)
  pure val HEIGHTS = WIRE_HEIGHTS
  pure val QUERIES = Set("ta", "tb", "unknown")
  pure val BODIES = Set(None).union(PAYLOADS.map(payload => Some(payload)))

  /// Every indexer answer the type admits.
  pure val ANSWERS =
    tuples(BODIES, HEIGHTS).map(((body, height)) => IFound({ body: body, height: height }))
      .union(Set(INotFound, IUnavailable))

  /// The answers an honest indexer gives: a found transaction has a body, and
  /// the body parses.
  pure val HONEST_ANSWERS = ANSWERS.filter(answer =>
    match answer {
      | IFound(found) =>
        match found.body {
          | Some(payload) => isSome(payload.txid)
          | None => false
        }
      | INotFound => true
      | IUnavailable => true
    })

  pure val HONEST_OUTCOMES = Set(QueueHit).union(HONEST_ANSWERS.map(answer => FromIndexer(answer)))

  pure val REPLIES =
    tuples(BODIES, HEIGHTS).map(((body, height)) => WFound({ body: body, height: height }))
      .union(Set(WNotFound, WError))

  pure val REFUSALS = Set(TipStale, HubDraining, ExpiryTooTight)

  /// F1. Rendering an honest outcome and reading it back gives what the
  /// outcome means.
  run renderThenInterpretIsMeaningTest =
    assert(tuples(HONEST_OUTCOMES, QUERIES).forall(((outcome, query)) =>
      interpretReply(render(outcome), query) == meaning(outcome, query)))

  /// F2. The documented collision: a queue hit and an indexer answer of
  /// "found, height 0, no body" are the same reply, though they mean different
  /// things. The wallet cannot tell them apart: it is told pending for both,
  /// so an indexer that answers that way for a transaction nobody holds makes
  /// the wallet see pending (`indexerForgesPendingTest` is that run). No two
  /// honest outcomes collide.
  run sentinelCollisionTest = all {
    assert(render(QueueHit) == render(FromIndexer(IFound({ body: None, height: AtZero })))),
    assert(QUERIES.forall(query =>
      and {
        interpretReply(render(QueueHit), query) == Pending,
        interpretReply(render(FromIndexer(IFound({ body: None, height: AtZero }))), query) == Pending,
      })),
    assert(QUERIES.forall(query =>
      meaning(QueueHit, query)
        != meaning(FromIndexer(IFound({ body: None, height: AtZero })), query))),
    assert(tuples(HONEST_OUTCOMES, HONEST_OUTCOMES).forall(((left, right)) =>
      left != right implies render(left) != render(right))),
  }

  /// F3. The shim serves a transaction only when its txid is the one asked
  /// for, and compares nothing else.
  run servedOnlyOnMatchingTxidTest = all {
    assert(tuples(REPLIES, QUERIES).forall(((reply, query)) =>
      match interpretReply(reply, query) {
        | Tx(tx) => tx.payload.txid == Some(query)
        | _ => true
      })),
    // Found with no body at a mined height is not a transaction and not the
    // pending sentinel.
    assert(interpretReply(WFound({ body: None, height: AtMined }), "ta") == NotFound),
    // A twin of the transaction asked for is served.
    assert(interpretReply(WFound({ body: Some(pATwin), height: AtMined }), "ta")
      == Tx({ payload: pATwin, height: AtMined })),
    // The height is passed through as given.
    assert(HEIGHTS.forall(height =>
      interpretReply(WFound({ body: Some(pA), height: height }), "ta")
        == Tx({ payload: pA, height: height }))),
    // Bytes that do not parse are never served, at any height.
    assert(tuples(HEIGHTS, QUERIES).forall(((height, query)) =>
      interpretReply(WFound({ body: Some(pJunk), height: height }), query) == NotFound)),
  }

  /// F4. A hub error fails closed. It is never reported as not found.
  run errorIsNeverNotFoundTest =
    assert(QUERIES.forall(query => interpretReply(WError, query) == Unavailable))

  /// F10. A draining hub refuses under the queue-full code; a fresh admission
  /// and a duplicate are one acceptance. Every refusal has its own code.
  run ackRenderingTest = all {
    assert(renderAck(Refused(HubDraining)) == WRefused(WQueueFull)),
    assert(renderAck(Admitted) == WAccepted),
    assert(renderAck(Duplicate) == WAccepted),
    assert(REFUSALS.forall(refusal => renderAck(Refused(refusal)) != WAccepted)),
    assert(tuples(REFUSALS, REFUSALS).forall(((left, right)) =>
      renderAck(Refused(left)) == renderAck(Refused(right)) implies left == right)),
  }
}

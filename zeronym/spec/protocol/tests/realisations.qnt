// -*- mode: Bluespec; -*-

/// The hub's part of each pinned protocol run that shows a violation or a
/// reached state: the inputs the real hub sees in that run, the replies the
/// run relies on, in order, and the hub as the protocol sees it at the end.
///
/// `realisedRunsTest` replays each through the real hub function. Each
/// protocol run ends with `realisedBy`, so the two cannot drift apart. The
/// soup has no order, so a protocol run is compared on the set of replies.
///
/// No runs here: a module that imports this one does not run them again.
///
/// K1b has no hub step: the frame is never delivered.
module realisations {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import hub.* from "../hub"
  import abstractHub.* from "../abstractHub"
  import protocol.* from "../protocol"

  /// A step the hub takes, or a lie: a member of the Byzantine relation, with
  /// the state unchanged.
  type Step = Do(HubInput) | Lie({ input: HubInput, out: HubOutput })

  type Realisation = { steps: List[Step], replies: List[AReply], last: AHub }

  pure def sub(nonce: Nonce, payload: Payload): HubInput = SubmitHInput({ nonce: nonce, payload: payload })
  pure def look(nonce: Nonce, txid: TxId, answer: IndexerAnswer): HubInput =
    LookupHInput({ nonce: nonce, txid: txid, answer: answer })
  pure def judged(payload: Payload, verdict: Verdict): HubInput =
    VerdictHInput({ payload: payload, verdict: verdict })
  pure def found(body: Option[Payload], height: WireHeight): IndexerAnswer = IFound({ body: body, height: height })
  pure def tips(heights: List[Height]): List[Step] = heights.foldl([], (acc, h) => acc.append(Do(TipHInput(h))))
  pure def does(inputs: List[HubInput]): List[Step] = inputs.foldl([], (acc, i) => acc.append(Do(i)))

  pure def flushed(payload: Payload, verdict: Verdict): List[Step] =
    does([FlushDueHInput, judged(payload, verdict), FlushDoneHInput])

  /// One lie about `early`, in answer to a lookup.
  pure def lieAbout(nonce: Nonce, answer: IndexerAnswer, outcome: HubOutcome): Step =
    Lie({ input: look(nonce, "early", answer), out: LookupReplyOutput({ nonce: nonce, outcome: outcome }) })

  pure val accepted = AAck(WAccepted)
  pure val pending = AWire(WFound({ body: None, height: AtZero }))
  pure val inMempool = AWire(WFound({ body: Some(early), height: AtZero }))
  pure val queuedEarly = { queue: Set(early), held: Set() }

  /// Running at height 2 with `early` queued, as most runs begin.
  pure val earlyQueued = tips([1, 2]).append(Do(sub(0, early)))
  /// The same, flushed and accepted at height 3.
  pure val earlyPublished = earlyQueued.concat(tips([3])).concat(flushed(early, Accepted))

  // ------------------------------------------------------------------------
  // Every component honest
  // ------------------------------------------------------------------------

  /// K1a: refused after the flush at 3.
  pure val k1a: Realisation = {
    steps: tips([1, 2, 3]).concat(does([FlushDueHInput, sub(0, tight)])),
    replies: [AAck(WRefused(WExpiryTooTight))],
    last: emptyAHub,
  }

  /// K2a: pending, then in the mempool.
  pure val k2a: Realisation = {
    steps: earlyQueued.append(Do(look(1, "early", INotFound))).concat(tips([3])).concat(flushed(early, Accepted))
      .append(Do(look(2, "early", found(Some(early), AtZero)))),
    replies: [accepted, pending, inMempool],
    last: emptyAHub,
  }

  /// K2b and K2c: the resubmission comes from the wallet or the third party.
  pure val k2bc: Realisation = {
    steps: earlyPublished.append(Do(look(1, "early", found(Some(early), AtZero)))).append(Do(sub(2, early)))
      .append(Do(look(3, "early", found(Some(early), AtZero)))),
    replies: [accepted, inMempool, accepted, pending],
    last: queuedEarly,
  }

  /// K2d: the flush window.
  pure val k2d: Realisation = {
    steps: earlyQueued.append(Do(look(1, "early", INotFound))).concat(tips([3])).append(Do(FlushDueHInput))
      .append(Do(look(2, "early", INotFound))),
    replies: [accepted, pending, AWire(WNotFound)],
    last: { queue: Set(), held: Set(early) },
  }

  /// K2e: rejected at flush.
  pure val k2e: Realisation = {
    steps: earlyQueued.append(Do(look(1, "early", INotFound))).concat(tips([3])).concat(flushed(early, Rejected))
      .append(Do(look(2, "early", INotFound))),
    replies: [accepted, pending, AWire(WNotFound)],
    last: emptyAHub,
  }

  /// W8: a third party is told `early` is queued.
  pure val w8: Realisation = {
    steps: earlyQueued.append(Do(look(0, "early", INotFound))),
    replies: [accepted, pending],
    last: queuedEarly,
  }

  /// W9, the unknown upgrade: queued and missed.
  pure val w9: Realisation = {
    steps: tips([1]).concat(does([sub(0, junk), look(1, "junk", INotFound)])),
    replies: [accepted, AWire(WNotFound)],
    last: { queue: Set(junk), held: Set() },
  }

  // ------------------------------------------------------------------------
  // The "required" runs of the trust matrix and W16, each one lie
  // ------------------------------------------------------------------------

  /// G2, the hub: the queued bytes, to a third party.
  pure val hubLeaksBody: Realisation = {
    steps: earlyQueued.append(lieAbout(0, INotFound, FromIndexer(found(Some(early), AtZero)))),
    replies: [accepted, inMempool],
    last: queuedEarly,
  }

  /// G4, the hub: not found for a queued transaction.
  pure val hubDeniesQueued: Realisation = {
    steps: earlyQueued.append(lieAbout(1, INotFound, FromIndexer(INotFound))),
    replies: [accepted, AWire(WNotFound)],
    last: queuedEarly,
  }

  /// G4, the hub: a false height.
  pure val hubFalseHeight: Realisation = {
    steps: earlyPublished.append(lieAbout(1, found(Some(early), AtZero), FromIndexer(found(Some(early), AtOther)))),
    replies: [accepted, AWire(WFound({ body: Some(early), height: AtOther }))],
    last: emptyAHub,
  }

  /// G8, the hub: accepted and not queued.
  pure val hubAcksUnqueued: Realisation = {
    steps: tips([1, 2]).append(Lie({ input: sub(0, early), out: AckOutput({ nonce: 0, kind: Admitted }) })),
    replies: [accepted],
    last: emptyAHub,
  }

  /// W16: a twin at a false height.
  pure val w16: Realisation = {
    steps: earlyQueued.append(lieAbout(1, INotFound, FromIndexer(found(Some(earlyTwin), AtOther)))),
    replies: [accepted, AWire(WFound({ body: Some(earlyTwin), height: AtOther }))],
    last: queuedEarly,
  }

  /// G2, the indexer: the honest hub forwards the unpublished bytes.
  pure val indexerLeaksBody: Realisation = {
    steps: earlyQueued.concat(tips([3])).concat(does([FlushDueHInput, judged(early, Retryable),
      look(0, "early", found(Some(early), AtZero))])),
    replies: [accepted, inMempool],
    last: { queue: Set(), held: Set(early) },
  }

  /// G4, the indexer: a forged pending for a transaction nobody queued.
  pure val indexerForgesPending: Realisation = {
    steps: tips([1, 2]).append(Do(look(1, "early", found(None, AtZero)))),
    replies: [pending],
    last: emptyAHub,
  }

  // ------------------------------------------------------------------------
  // The link to the protocol runs
  // ------------------------------------------------------------------------

  /// Every ack and lookup reply the hub has sent, to any client.
  pure def hubReplies(s: System): Set[AReply] =
    Set(ShimAddr, ThirdPartyAddr).map(client =>
      s.acks(client).map(ack => AAck(ack.ack)).union(s.replies(client).map(reply => AWire(reply.reply)))
    ).flatten()

  /// The protocol run that ends in `s` has the hub replies and the final hub
  /// of `r`.
  pure def realisedBy(s: System, r: Realisation): bool =
    and {
      s.hubReplies() == r.replies.foldl(Set(), (acc, reply) => acc.union(Set(reply))),
      s.hub == r.last,
    }
}

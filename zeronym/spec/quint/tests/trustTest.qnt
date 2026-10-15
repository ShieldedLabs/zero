// -*- mode: Bluespec; -*-

/// The trust matrix, cell by cell: for every guarantee that needs a component
/// to be honest, a run in which that component is Byzantine and the guarantee
/// fails.
///
/// Each run is built from the machine's own steps. It names the one Byzantine
/// transition it relies on, written out as a `...With` step, and ends by
/// stating what in the wallet's log, the soup or the audit record constitutes
/// the failure. The guarantee is asserted in the state just before that
/// transition, so the run shows the lie is what breaks it.

module trustTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import abstractHub.* from "../abstractHub"
  import shim.* from "../shim"
  import protocol.* from "../protocol"

  // ------------------------------------------------------------------------
  // A Byzantine hub (`byzHub`)
  // ------------------------------------------------------------------------

  /// G2 needs the hub. Asked by a third party about a queued txid, it answers
  /// with the queued bytes.
  run hubServesQueuedBodyTest =
    initByzHub
      .then(submitTo(0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .expect(queuedBytesConfidential)
      .then(hubReceiveWith(
        fromThirdParty(lookupMail(0, "early")), INotFound,
        { hub: s.hub, reply: AWire(render(FromIndexer(IFound({ body: Some(early), height: AtZero })))) },
      ))
      .expect(s.replies(ThirdPartyAddr) == Set({ nonce: 0, reply: WFound({ body: Some(early), height: AtZero }) }))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      // What it learned came in the body of a reply addressed to it.
      .expect(s.repliedBodies(ThirdPartyAddr) == Set(early) and s.operator == Set())
      .expect(not(queuedBytesConfidential))

  /// G4 needs the hub. It answers not found for a transaction it has queued.
  run hubDeniesQueuedTest =
    initByzHub
      .then(submitTo(0, early))
      .then(ask("early"))
      .expect(lookupValidityPerHub)
      .then(hubReceiveWith(
        lookupMail(1, "early"), INotFound,
        { hub: s.hub, reply: AWire(render(FromIndexer(INotFound))) },
      ))
      .then(deliverToShim(replyMail(1, WNotFound)))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(s.hub.queue == Set(early) and audit.windows.get(1) == Set(Pending))
      .expect(not(lookupValidityPerHub))

  /// G4 needs the hub, second run. It serves a mempool transaction as mined.
  /// The txid is right, so the shim passes it on.
  run hubServesFalseHeightTest =
    initByzHub
      .then(submitTo(0, early))
      .then(flush([early], Accepted))
      .then(ask("early"))
      .expect(lookupValidityPerHub)
      .then(hubReceiveWith(
        lookupMail(1, "early"), chainAnswer(s.indexer, "early"),
        { hub: s.hub, reply: AWire(render(FromIndexer(IFound({ body: Some(early), height: AtMined })))) },
      ))
      .then(deliverToShim(replyMail(1, WFound({ body: Some(early), height: AtMined }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: AtMined }), via: Some(1) }))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.windows.get(1) == Set(Tx({ payload: early, height: AtZero })))
      .expect(not(lookupValidityPerHub) and txidAuthenticity)

  /// G5 needs the hub. It answers pending for a transaction it does not
  /// hold, the reply only a queue hit should give.
  run hubForgesPendingTest =
    initByzHub
      .then(sends(Clean(early), true))
      .then(ask("early"))
      .expect(pendingOnlyFromQueue)
      .then(hubReceiveWith(
        lookupMail(1, "early"), INotFound,
        { hub: s.hub, reply: AWire(WFound({ body: None, height: AtZero })) },
      ))
      .then(deliverToShim(replyMail(1, WFound({ body: None, height: AtZero }))))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(s.hub.queue == Set() and audit.windows.get(1) == Set(NotFound))
      .expect(not(pendingOnlyFromQueue))

  /// G3 survives. The hub answers with another transaction; the shim compares
  /// txids and refuses it.
  run wrongTransactionIsRefusedTest =
    initByzHub
      .then(sendToHub(early))
      .then(ask("early"))
      .then(hubReceiveWith(
        lookupMail(1, "early"), INotFound,
        { hub: s.hub, reply: AWire(render(FromIndexer(IFound({ body: Some(tight), height: AtMined })))) },
      ))
      .then(deliverToShim(replyMail(1, WFound({ body: Some(tight), height: AtMined }))))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(txidAuthenticity)

  // ------------------------------------------------------------------------
  // A Byzantine indexer (`byzIndexer`)
  // ------------------------------------------------------------------------

  // A hub folds several indexer endpoints into one answer. A lookup answer
  // comes from the first endpoint that says found, so one misbehaving endpoint
  // is enough for the first two runs. The tip is the maximum over endpoints,
  // so holding it back, as the last two do, takes every endpoint. The model
  // has one abstract indexer and does not enforce that difference.

  /// G2 needs the indexer. It is offered a batch, reports nothing judged, and
  /// then serves the unpublished bytes in a lookup answer, which the honest
  /// hub forwards to whoever asked.
  run indexerServesUnpublishedBodyTest =
    initByzIndexer
      .then(submitTo(0, early))
      .then(hubTake)
      .then(judge(early, Retryable))
      .expect(s.indexer.offered == Set(early) and s.onChain("early") == Absent)
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .expect(queuedBytesConfidential)
      .then(deliverLookupFrom(
        ThirdPartyAddr, 0, "early", IFound({ body: Some(early), height: AtZero }),
      ))
      .expect(s.replies(ThirdPartyAddr) == Set({ nonce: 0, reply: WFound({ body: Some(early), height: AtZero }) }))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      // What it learned came in the body of a reply addressed to it.
      .expect(s.repliedBodies(ThirdPartyAddr) == Set(early) and s.operator == Set())
      .expect(not(queuedBytesConfidential))

  /// Finding 8, before #85. For a transaction that exists nowhere the
  /// indexer answers "found, height 0, no body". The hub forwards it
  /// unchanged, and on the wire it is the hub's own "queued here": the wallet
  /// sees pending. That broke G4 and G5.
  run indexerForgesPendingPre85Test =
    initByzIndexerPre85
      .then(sends(Clean(early), true))
      .then(ask("early"))
      .expect(lookupValidityPerHub and pendingOnlyFromQueue)
      .then(deliverLookupFrom(ShimAddr, 1, "early", IFound({ body: None, height: AtZero })))
      .then(deliverToShim(replyMail(1, WFound({ body: None, height: AtZero }))))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(s.hub.queue == Set() and audit.windows.get(1) == Set(NotFound))
      .expect(not(lookupValidityPerHub) and not(pendingOnlyFromQueue))

  /// Finding 8, fixed by #85. The same answer is that endpoint failing. With
  /// no other endpoint the hub answers with an error, and the wallet sees the
  /// lookup as unavailable, not pending. G4 and G5 hold.
  run indexerEmptyAnswerIsUnavailableTest =
    initByzIndexer
      .then(sends(Clean(early), true))
      .then(ask("early"))
      .then(deliverLookupFrom(ShimAddr, 1, "early", IFound({ body: None, height: AtZero })))
      .expect(s.replies(ShimAddr) == Set({ nonce: 1, reply: WError }))
      .then(deliverToShim(replyMail(1, WError)))
      .expect(lastEvent == Got({ query: "early", obs: Unavailable, via: Some(1) }))
      .expect(lookupValidityPerHub and pendingOnlyFromQueue)

  /// G4 still needs the indexer after #85. It serves a published mempool
  /// transaction as mined. The txid is right, so the shim passes it on. G5
  /// is not touched: nothing here reads as pending.
  run indexerServesFalseHeightTest =
    initByzIndexer
      .then(submitTo(0, early))
      .then(flush([early], Accepted))
      .then(ask("early"))
      .expect(lookupValidityPerHub)
      .then(deliverLookupFrom(ShimAddr, 1, "early", IFound({ body: Some(early), height: AtMined })))
      .then(deliverToShim(replyMail(1, WFound({ body: Some(early), height: AtMined }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: AtMined }), via: Some(1) }))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.windows.get(1) == Set(Tx({ payload: early, height: AtZero })))
      .expect(not(lookupValidityPerHub) and pendingOnlyFromQueue and txidAuthenticity)
}

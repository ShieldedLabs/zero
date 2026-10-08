// -*- mode: Bluespec; -*-

/// The trust matrix, cell by cell: for every guarantee that needs a component
/// to be honest, a run in which that component is Byzantine and the guarantee
/// fails.
///
/// Each run is built from the machine's own steps. It names the one Byzantine
/// transition it relies on, written out as a `...With` step, and ends by
/// stating what in the wallet's log, the soup or the audit record constitutes
/// the failure. Each is followed by its control: the same wallet inputs in the
/// same configuration, with the component taking the honest transition, which
/// is always among those it may take. In the control the guarantee holds.

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
      .then(hubReceiveWith(
        fromThirdParty(lookupMail(0, "early")), INotFound,
        { hub: s.hub, reply: AWire(render(FromIndexer(IFound({ body: Some(early), height: AtZero })))) },
      ))
      .expect(s.replies(ThirdPartyAddr) == Set({ nonce: 0, reply: WFound({ body: Some(early), height: AtZero }) }))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      // What it learned came in the body of a reply addressed to it.
      .expect(s.repliedBodies(ThirdPartyAddr) == Set(early) and s.operator == Set())
      .expect(not(queuedBytesConfidential))

  run hubServesQueuedBodyControlTest =
    initByzHub
      .then(submitTo(0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .then(deliverLookupFrom(ThirdPartyAddr, 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set({ nonce: 0, reply: WFound({ body: None, height: AtZero }) }))
      .expect(queuedBytesConfidential)

  /// G4 needs the hub. It answers not found for a transaction it has queued.
  run hubDeniesQueuedTest =
    initByzHub
      .then(submitTo(0, early))
      .then(ask("early"))
      .then(hubReceiveWith(
        lookupMail(1, "early"), INotFound,
        { hub: s.hub, reply: AWire(render(FromIndexer(INotFound))) },
      ))
      .then(deliverToShim(replyMail(1, WNotFound)))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(s.hub.queue == Set(early) and audit.windows.get(1) == Set(Pending))
      .expect(not(lookupValidityPerHub))

  run hubDeniesQueuedControlTest =
    initByzHub
      .then(submitTo(0, early))
      .then(lookUp(1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(lookupValidityPerHub)

  /// G4 needs the hub, second run. It serves a mempool transaction as mined,
  /// at a height it made up. The txid is right, so the shim passes it on.
  run hubServesFalseHeightTest =
    initByzHub
      .then(submitTo(0, early))
      .then(flush([early], Accepted))
      .then(ask("early"))
      .then(hubReceiveWith(
        lookupMail(1, "early"), chainAnswer(s.indexer, "early"),
        { hub: s.hub, reply: AWire(render(FromIndexer(IFound({ body: Some(early), height: AtOther })))) },
      ))
      .then(deliverToShim(replyMail(1, WFound({ body: Some(early), height: AtOther }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: AtOther }), via: Some(1) }))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.windows.get(1) == Set(Tx({ payload: early, height: AtZero })))
      .expect(not(lookupValidityPerHub) and txidAuthenticity)

  run hubServesFalseHeightControlTest =
    initByzHub
      .then(submitTo(0, early))
      .then(flush([early], Accepted))
      .then(lookUp(1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: AtZero }), via: Some(1) }))
      .expect(lookupValidityPerHub)

  /// G8 needs the hub. It acks a submission as accepted and does not queue it.
  run hubAcksWithoutAdmittingTest =
    initByzHub
      .then(sendToHub(early))
      .then(hubReceiveWith(submitMail(0, early), INotFound, { hub: s.hub, reply: AAck(WAccepted) }))
      .expect(s.acks(ShimAddr) == Set({ nonce: 0, ack: WAccepted }))
      .expect(s.hub.queue == Set() and audit.everQueued == Set())
      .expect(not(ackImpliesQueued))

  run hubAcksWithoutAdmittingControlTest =
    initByzHub
      .then(sendToHub(early))
      .then(deliverSubmit(0, early))
      .expect(s.acks(ShimAddr) == Set({ nonce: 0, ack: WAccepted }) and s.hub.queue == Set(early))
      .expect(ackImpliesQueued)

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
      .then(deliverLookupFrom(
        ThirdPartyAddr, 0, "early", IFound({ body: Some(early), height: AtZero }),
      ))
      .expect(s.replies(ThirdPartyAddr) == Set({ nonce: 0, reply: WFound({ body: Some(early), height: AtZero }) }))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      // What it learned came in the body of a reply addressed to it.
      .expect(s.repliedBodies(ThirdPartyAddr) == Set(early) and s.operator == Set())
      .expect(not(queuedBytesConfidential))

  run indexerServesUnpublishedBodyControlTest =
    initByzIndexer
      .then(submitTo(0, early))
      .then(hubTake)
      .then(judge(early, Retryable))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .then(deliverLookupFrom(ThirdPartyAddr, 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set({ nonce: 0, reply: WNotFound }))
      .expect(queuedBytesConfidential)

  /// G4 needs the indexer. For a transaction that exists nowhere it answers
  /// "found, height 0, no body". The hub forwards it unchanged, and on the
  /// wire it is the hub's own "queued here": the wallet sees pending.
  run indexerForgesPendingTest =
    initByzIndexer
      .then(sends(Clean(early), true))
      .then(ask("early"))
      .then(deliverLookupFrom(ShimAddr, 1, "early", IFound({ body: None, height: AtZero })))
      .then(deliverToShim(replyMail(1, WFound({ body: None, height: AtZero }))))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(audit.everQueued == Set() and audit.windows.get(1) == Set(NotFound))
      .expect(not(lookupValidityPerHub))

  run indexerForgesPendingControlTest =
    initByzIndexer
      .then(sends(Clean(early), true))
      .then(lookUp(1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(lookupValidityPerHub)
}

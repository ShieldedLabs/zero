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

module byzHubTrust {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = byzHub).* from "../protocol"

  /// G2 needs the hub. Asked by a third party about a queued txid, it answers
  /// with the queued bytes.
  run hubServesQueuedBodyTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .then(hubReceiveWith(
        fromThirdParty(lookupMail(0, "early")), INotFound,
        s.hub.toLookupReplyOutput(0, FromIndexer(IFound({ body: Some(early), height: MEMPOOL_HEIGHT }))),
      ))
      .expect(s.replies(ThirdPartyAddr) == Set((0, WFound({ body: Some(early), height: MEMPOOL_HEIGHT }))))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      .expect(not(queuedBytesConfidential))

  run hubServesQueuedBodyControlTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .then(deliverLookupFrom(ThirdPartyAddr, 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set((0, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(queuedBytesConfidential)

  /// G4 needs the hub. It answers not found for a transaction it has queued.
  run hubDeniesQueuedTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(ask("early"))
      .then(hubReceiveWith(
        lookupMail(1, "early"), INotFound,
        s.hub.toLookupReplyOutput(1, FromIndexer(INotFound)),
      ))
      .then(deliverToShim(replyMail(1, WNotFound)))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(s.hub.queued() == Set(early) and audit.windows.get(1) == Set(Pending))
      .expect(not(lookupValidityPerHub))

  run hubDeniesQueuedControlTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(lookUp(1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(lookupValidityPerHub)

  /// G4 needs the hub, second run. It serves a mempool transaction as mined,
  /// at a height it made up. The txid is right, so the shim passes it on.
  run hubServesFalseHeightTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(block)
      .then(flush([early], Accepted))
      .then(ask("early"))
      .then(hubReceiveWith(
        lookupMail(1, "early"), chainAnswer(s.indexer, "early"),
        s.hub.toLookupReplyOutput(1, FromIndexer(IFound({ body: Some(early), height: 9 }))),
      ))
      .then(deliverToShim(replyMail(1, WFound({ body: Some(early), height: 9 }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: 9 }), via: Some(1) }))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.windows.get(1) == Set(Tx({ payload: early, height: MEMPOOL_HEIGHT })))
      .expect(not(lookupValidityPerHub) and txidAuthenticity)

  run hubServesFalseHeightControlTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(block)
      .then(flush([early], Accepted))
      .then(lookUp(1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(1) }))
      .expect(lookupValidityPerHub)

  /// G8 needs the hub. It acks a submission as accepted and does not queue it.
  run hubAcksWithoutAdmittingTest =
    started
      .then(block)
      .then(sendToHub(early))
      .then(hubReceiveWith(submitMail(0, early), INotFound, s.hub.toAckOutput(0, Admitted)))
      .expect(s.acks(ShimAddr) == Set((0, WAccepted)))
      .expect(s.hub.queued() == Set() and audit.everQueued == Set())
      .expect(not(ackImpliesQueued))

  run hubAcksWithoutAdmittingControlTest =
    started
      .then(block)
      .then(sendToHub(early))
      .then(deliverSubmit(0, early))
      .expect(s.acks(ShimAddr) == Set((0, WAccepted)) and s.hub.queued() == Set(early))
      .expect(ackImpliesQueued)

  /// G3 survives. The hub answers with another transaction; the shim compares
  /// txids and refuses it.
  run wrongTransactionIsRefusedTest =
    started
      .then(block)
      .then(sendToHub(early))
      .then(ask("early"))
      .then(hubReceiveWith(
        lookupMail(1, "early"), INotFound,
        s.hub.toLookupReplyOutput(1, FromIndexer(IFound({ body: Some(tight), height: 3 }))),
      ))
      .then(deliverToShim(replyMail(1, WFound({ body: Some(tight), height: 3 }))))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(txidAuthenticity)
}

module byzIndexerTrust {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = byzIndexer).* from "../protocol"

  // A hub folds several indexer endpoints into one answer. A lookup answer
  // comes from the first endpoint that says found, so one misbehaving endpoint
  // is enough for the first two runs. The tip is the maximum over endpoints,
  // so holding it back, as the last two do, takes every endpoint. The model
  // has one abstract indexer and does not enforce that difference.

  /// G2 needs the indexer. It is offered a batch, reports nothing judged, and
  /// then serves the unpublished bytes in a lookup answer, which the honest
  /// hub forwards to whoever asked.
  run indexerServesUnpublishedBodyTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(block)
      .then(hubFlushBegin)
      .then(judge(early, Retryable))
      .expect(s.indexer.offered == Set(early) and s.onChain("early") == Absent)
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .then(deliverLookupFrom(
        ThirdPartyAddr, 0, "early", IFound({ body: Some(early), height: MEMPOOL_HEIGHT }),
      ))
      .expect(s.replies(ThirdPartyAddr) == Set((0, WFound({ body: Some(early), height: MEMPOOL_HEIGHT }))))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      .expect(not(queuedBytesConfidential))

  run indexerServesUnpublishedBodyControlTest =
    started
      .then(block)
      .then(submitTo(0, early))
      .then(block)
      .then(hubFlushBegin)
      .then(judge(early, Retryable))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .then(deliverLookupFrom(ThirdPartyAddr, 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set((0, WNotFound)))
      .expect(queuedBytesConfidential)

  /// G4 needs the indexer. For a transaction that exists nowhere it answers
  /// "found, height 0, no body". The hub forwards it unchanged, and on the
  /// wire it is the hub's own "queued here": the wallet sees pending.
  run indexerForgesPendingTest =
    started
      .then(block)
      .then(sends(Clean(early), true))
      .then(ask("early"))
      .then(deliverLookupFrom(ShimAddr, 1, "early", IFound({ body: None, height: MEMPOOL_HEIGHT })))
      .then(deliverToShim(replyMail(1, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(audit.everQueued == Set() and audit.windows.get(1) == Set(NotFound))
      .expect(not(lookupValidityPerHub))

  run indexerForgesPendingControlTest =
    started
      .then(block)
      .then(sends(Clean(early), true))
      .then(lookUp(1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(lookupValidityPerHub)
}

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

module byzShimTrust {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = byzShim).* from "../protocol"

  /// G1 needs the shim. It hands a migration to the operator.
  run operatorSeesMigrationTest =
    started
      .then(block)
      .then(walletSendWith(Clean(early), 1, s.shim.toForwardOutput(early)))
      .expect(s.operator == Set(early) and early.class == OrchardTouching)
      .expect(not(operatorBlind))

  run operatorSeesMigrationControlTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .expect(s.operator == Set() and s.net == Set(submitMail("h1", 0, early)))
      .expect(operatorBlind)

  /// G2 needs the shim. It sees every migration in the clear and reveals one.
  run shimDisclosesPlaintextTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .then(byzDiscloseWith(early))
      .expect(s.disclosed == Set(early) and s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      .expect(not(queuedBytesConfidential))

  run shimDisclosesPlaintextControlTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .expect(s.tpLearned() == Set())
      .expect(queuedBytesConfidential)

  /// G3 needs the shim. Asked for one transaction, it serves another.
  run shimServesAnotherTransactionTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(walletGetWith("early", 0, s.shim.toLookupDoneOutput("early", Tx({ payload: tight, height: 3 }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: tight, height: 3 }), via: None }))
      .expect(not(txidAuthenticity))

  run shimServesAnotherTransactionControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(lookUp("h1", 1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(1) }))
      .expect(txidAuthenticity)

  /// G4 needs the shim. It reports a transaction pending that no hub holds,
  /// without asking one.
  run shimInventsStatusTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .then(walletGetWith("early", 0, s.shim.toLookupDoneOutput("early", Pending)))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: None }))
      .expect(s.queuedAt("h1") == Set() and s.lookups(ShimAddr) == Set())
      .expect(not(lookupValidityPerHub))

  run shimInventsStatusControlTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .then(lookUp("h1", 1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(lookupValidityPerHub)
}

module awaitAckByzShimTrust {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = awaitAckByzShim).* from "../protocol"

  /// G5 needs the shim. It tells the wallet ok and sends nothing.
  run toldOkWithoutSendingTest =
    started
      .then(block)
      .then(walletSendWith(Clean(early), 1, s.shim.toSendDoneOutput(Clean(early), SentOk)))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(s.net == Set() and audit.everQueued.get("h1") == Set())
      .expect(not(toldImpliesQueued))

  run toldOkWithoutSendingControlTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverToShim(ackMail("h1", 0, WAccepted)))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(audit.everQueued.get("h1") == Set(early))
      .expect(toldImpliesQueued)
}

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

  def h1 = s.hubs.get("h1")

  /// G2 needs the hub. Asked by a third party about a queued txid, it answers
  /// with the queued bytes.
  run hubServesQueuedBodyTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early", "h1"))
      .then(hubReceiveWith(
        "h1", fromThirdParty(lookupMail("h1", 0, "early")), INotFound,
        h1.toLookupReplyOutput(0, FromIndexer(IFound({ body: Some(early), height: MEMPOOL_HEIGHT }))),
      ))
      .expect(s.replies(ThirdPartyAddr) == Set((0, "h1", WFound({ body: Some(early), height: MEMPOOL_HEIGHT }))))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      .expect(not(queuedBytesConfidential))

  run hubServesQueuedBodyControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early", "h1"))
      .then(deliverLookupFrom(ThirdPartyAddr, "h1", 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set((0, "h1", WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(queuedBytesConfidential)

  /// G4 needs the hub. It answers not found for a transaction it has queued.
  run hubDeniesQueuedTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(ask("early", 0))
      .then(hubReceiveWith(
        "h1", lookupMail("h1", 1, "early"), INotFound,
        h1.toLookupReplyOutput(1, FromIndexer(INotFound)),
      ))
      .then(deliverToShim(replyMail("h1", 1, WNotFound)))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(s.queuedAt("h1") == Set(early) and audit.windows.get(1) == Set(Pending))
      .expect(not(lookupValidityPerHub))

  run hubDeniesQueuedControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(lookUp("h1", 1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(lookupValidityPerHub)

  /// G4 needs the hub, second run. It serves a mempool transaction as mined,
  /// at a height it made up. The txid is right, so the shim passes it on.
  run hubServesFalseHeightTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(ask("early", 0))
      .then(hubReceiveWith(
        "h1", lookupMail("h1", 1, "early"), chainAnswer(s.indexer, "early"),
        h1.toLookupReplyOutput(1, FromIndexer(IFound({ body: Some(early), height: 9 }))),
      ))
      .then(deliverToShim(replyMail("h1", 1, WFound({ body: Some(early), height: 9 }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: 9 }), via: Some(1) }))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.windows.get(1) == Set(Tx({ payload: early, height: MEMPOOL_HEIGHT })))
      .expect(not(lookupValidityPerHub) and txidAuthenticity)

  run hubServesFalseHeightControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(lookUp("h1", 1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(1) }))
      .expect(lookupValidityPerHub)

  /// G8 needs the hub. It acks a submission as accepted and does not queue it.
  run hubAcksWithoutAdmittingTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(hubReceiveWith("h1", submitMail("h1", 0, early), INotFound, h1.toAckOutput(0, Admitted)))
      .expect(s.acks("h1", ShimAddr) == Set((0, WAccepted)))
      .expect(s.queuedAt("h1") == Set() and audit.everQueued.get("h1") == Set())
      .expect(not(ackImpliesQueued))

  run hubAcksWithoutAdmittingControlTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .expect(s.acks("h1", ShimAddr) == Set((0, WAccepted)) and s.queuedAt("h1") == Set(early))
      .expect(ackImpliesQueued)

  /// A3 needs the hub. Once its drain has begun, it takes a submission into
  /// the queue. The simulator does not check `temporal` definitions, so the
  /// run asserts the step itself: draining before and after, nothing in
  /// flight, and the queue grown by a payload no flush handed back.
  run hubAdmitsWhileDrainingTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(hubBeginDrainWith("h1"))
      .then(sendToAll(tight))
      .expect(h1.phase == Draining and s.queuedAt("h1") == Set(early) and s.inFlightAt("h1") == Set())
      .then(hubReceiveWith(
        "h1", submitMail("h1", 1, tight), INotFound,
        { ...h1, queue: h1.queue.put(tight, 0) }.toAckOutput(1, Admitted),
      ))
      .expect(h1.phase == Draining and s.queuedAt("h1") == Set(early, tight))
      .expect(not(wRefusedDraining))

  run hubAdmitsWhileDrainingControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(hubBeginDrainWith("h1"))
      .then(sendToAll(tight))
      .expect(h1.phase == Draining and s.queuedAt("h1") == Set(early) and s.inFlightAt("h1") == Set())
      .then(deliverSubmit("h1", 1, tight))
      .expect(h1.phase == Draining and s.queuedAt("h1") == Set(early))
      .expect(wRefusedDraining)

  /// G6a needs the hub. It admits a transaction the expiry rule refuses: one
  /// expiring at 5, taken at tip 3, when the next flush is at 6.
  run hubAdmitsPastExpiryRuleTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(sendToAll(tight))
      .then(hubReceiveWith(
        "h1", submitMail("h1", 0, tight), INotFound,
        { ...h1, queue: Map(tight -> 0) }.toAckOutput(0, Admitted),
      ))
      .then(blocks(3))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: tight, height: 6, attempt: 0, nth: 0 }))
      .expect(not(offeredBeforeExpiry))

  run hubAdmitsPastExpiryRuleControlTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(sendToAll(tight))
      .then(deliverSubmit("h1", 0, tight))
      .expect(s.acks("h1", ShimAddr) == Set((0, WRefused(WExpiryTooTight))))
      .then(blocks(3))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set())
      .expect(offeredBeforeExpiry)

  /// G6b and G6c need the hub. A supported wallet's transaction is refused by an
  /// honest hub only for reasons that are not about the transaction. This hub
  /// takes one while it has not yet seen a tip, when an honest hub refuses
  /// everything. It first sees the chain at height 7, adopts that epoch
  /// without flushing, and offers the transaction at 9, its expiry, when no
  /// node can take it.
  run hubAdmitsBeforeFirstTipTest =
    init
      .then(2.reps(_ => chainAdvance))
      .then(sendToAll(early))
      .then(hubReceiveWith(
        "h1", submitMail("h1", 0, early), INotFound,
        { ...h1, queue: Map(early -> 0) }.toAckOutput(0, Admitted),
      ))
      .expect(h1.phase == Starting and audit.admitted.get(("h1", early)).height == 3)
      .then(4.reps(_ => chainAdvance))
      .then(observe("h1"))
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .expect(conforming(early, MIN_WALLET_EXPIRY))
      .expect(audit.offers == Set({ hub: "h1", payload: early, height: 9, attempt: 0, nth: 0 }))
      .expect(not(conformingFirstOfferBeforeExpiry))
      .expect(not(s.indexer.isAcceptable(early)))
      .then(judge("h1", early, Rejected))
      .expect(not(conformingFirstOfferJudgedBeforeExpiry))

  run hubAdmitsBeforeFirstTipControlTest =
    init
      .then(2.reps(_ => chainAdvance))
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .expect(s.acks("h1", ShimAddr) == Set((0, WRefused(WTipStale))))
      .then(4.reps(_ => chainAdvance))
      .then(observe("h1"))
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set())
      .expect(conformingFirstOfferBeforeExpiry and conformingFirstOfferJudgedBeforeExpiry)
}

module awaitAckByzHubTrust {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = awaitAckByzHub).* from "../protocol"

  /// G5 needs the hub. The wallet's answer is the hub's word, and the hub
  /// says accepted without queueing anything.
  run toldOkOnAFalseAckTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(hubReceiveWith(
        "h1", submitMail("h1", 0, early), INotFound,
        s.hubs.get("h1").toAckOutput(0, Admitted),
      ))
      .then(deliverToShim(ackMail("h1", 0, WAccepted)))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(audit.everQueued.get("h1") == Set())
      .expect(not(toldImpliesQueued))

  run toldOkOnAFalseAckControlTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverToShim(ackMail("h1", 0, WAccepted)))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(toldImpliesQueued)
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
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(hubFlushBeginWith("h1"))
      .then(judge("h1", early, Retryable))
      .expect(s.indexer.offered == Set(early) and s.onChain("early") == Absent)
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early", "h1"))
      .then(deliverLookupFrom(
        ThirdPartyAddr, "h1", 0, "early", IFound({ body: Some(early), height: MEMPOOL_HEIGHT }),
      ))
      .expect(s.replies(ThirdPartyAddr) == Set((0, "h1", WFound({ body: Some(early), height: MEMPOOL_HEIGHT }))))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      .expect(not(queuedBytesConfidential))

  run indexerServesUnpublishedBodyControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(hubFlushBeginWith("h1"))
      .then(judge("h1", early, Retryable))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early", "h1"))
      .then(deliverLookupFrom(ThirdPartyAddr, "h1", 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set((0, "h1", WNotFound)))
      .expect(queuedBytesConfidential)

  /// G4 needs the indexer. For a transaction that exists nowhere it answers
  /// "found, height 0, no body". The hub forwards it unchanged, and on the
  /// wire it is the hub's own "queued here": the wallet sees pending.
  run indexerForgesPendingTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .then(ask("early", 0))
      .then(deliverLookupFrom(ShimAddr, "h1", 1, "early", IFound({ body: None, height: MEMPOOL_HEIGHT })))
      .then(deliverToShim(replyMail("h1", 1, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      .expect(audit.everQueued.get("h1") == Set() and audit.windows.get(1) == Set(NotFound))
      .expect(not(lookupValidityPerHub))

  run indexerForgesPendingControlTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .then(lookUp("h1", 1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(1) }))
      .expect(lookupValidityPerHub)

  /// G6a needs the indexer. The hub asks for the tip at every block. From
  /// height 3 the indexer goes on answering 2, and at height 5 it answers
  /// truthfully. The flush scheduled for 3 runs at 5, where a transaction
  /// expiring at 5 has no margin left.
  run indexerWithholdsTipTest =
    started
      .then(block)
      .then(submitTo("h1", 0, tight))
      .then(chainAdvance)
      .then(hubObserveTipWith("h1", 2))
      .then(chainAdvance)
      .then(hubObserveTipWith("h1", 2))
      .then(chainAdvance)
      .expect(s.height() == 5 and s.hubs.get("h1").tip == Some(2) and not(chainMayAdvance))
      .then(hubObserveTipWith("h1", 5))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: tight, height: 5, attempt: 0, nth: 0 }))
      .expect(not(offeredBeforeExpiry))

  /// The same polls, answered truthfully.
  run indexerWithholdsTipControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, tight))
      .then(chainAdvance)
      .then(hubObserveTipWith("h1", 3))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: tight, height: 3, attempt: 0, nth: 0 }))
      .then(judge("h1", tight, Accepted))
      .then(hubFlushEndWith("h1"))
      .then(chainAdvance)
      .then(hubObserveTipWith("h1", 4))
      .then(chainAdvance)
      .then(hubObserveTipWith("h1", 5))
      .expect(s.height() == 5 and audit.offers.size() == 1)
      .expect(offeredBeforeExpiry)

  /// G6b and G6c need the indexer. The same lie, kept up to height 8, does it
  /// to a supported wallet's transaction: it is offered at 8 with expiry 9,
  /// and after one block in flight the node cannot take it.
  run indexerWithholdsTipFromConformingTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(5.reps(_ => chainAdvance.then(hubObserveTipWith("h1", 2))))
      .then(chainAdvance)
      .expect(s.height() == 8 and s.hubs.get("h1").tip == Some(2))
      .then(hubObserveTipWith("h1", 8))
      .then(hubFlushBeginWith("h1"))
      .expect(conforming(early, MIN_WALLET_EXPIRY) and audit.admitted.get(("h1", early)).height == 2)
      .expect(audit.offers == Set({ hub: "h1", payload: early, height: 8, attempt: 0, nth: 0 }))
      .expect(not(conformingFirstOfferBeforeExpiry))
      .then(chainAdvance)
      .expect(not(s.indexer.isAcceptable(early)))
      .then(judge("h1", early, Rejected))
      .expect(audit.verdicts == Set({ hub: "h1", payload: early, height: 9, nth: 0, final: true }))
      .expect(not(conformingFirstOfferJudgedBeforeExpiry))

  /// The same polls, answered truthfully: offered at 3, and accepted after a
  /// block in flight.
  run indexerWithholdsTipFromConformingControlTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(chainAdvance)
      .then(hubObserveTipWith("h1", 3))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: early, height: 3, attempt: 0, nth: 0 }))
      .then(chainAdvance)
      .then(judge("h1", early, Accepted))
      .then(hubFlushEndWith("h1"))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.verdicts == Set({ hub: "h1", payload: early, height: 4, nth: 0, final: true }))
      .expect(conformingFirstOfferBeforeExpiry and conformingFirstOfferJudgedBeforeExpiry)
}

module replicatedOneByzTrust {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = replicatedOneByz).* from "../protocol"

  // Two hubs, `h1` honest and `h2` Byzantine. Replication does not dilute
  // trust: every hub receives every migration, and a lookup may start at
  // either.

  def h2 = s.hubs.get("h2")

  /// G2 is required of every hub. The Byzantine replica holds the same bytes
  /// as the honest one and gives them away.
  run oneReplicaServesQueuedBodyTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverSubmit("h2", 1, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early", "h2"))
      .then(hubReceiveWith(
        "h2", fromThirdParty(lookupMail("h2", 0, "early")), INotFound,
        h2.toLookupReplyOutput(0, FromIndexer(IFound({ body: Some(early), height: MEMPOOL_HEIGHT }))),
      ))
      .expect(s.tpLearned() == Set(early) and s.onChain("early") == Absent)
      .expect(not(queuedBytesConfidential))

  run oneReplicaServesQueuedBodyControlTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverSubmit("h2", 1, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early", "h2"))
      .then(deliverLookupFrom(ThirdPartyAddr, "h2", 0, "early", INotFound))
      .expect(s.tpLearned() == Set())
      .expect(queuedBytesConfidential)

  /// G4 is required of every hub. The cursor points at the Byzantine replica,
  /// which denies a transaction it has queued; its answer is final.
  run cursorLandsOnLyingReplicaTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverSubmit("h2", 1, early))
      .then(ask("early", 1))
      .then(hubReceiveWith(
        "h2", lookupMail("h2", 2, "early"), INotFound,
        h2.toLookupReplyOutput(2, FromIndexer(INotFound)),
      ))
      .then(deliverToShim(replyMail("h2", 2, WNotFound)))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(2) }))
      .expect(s.queuedAt("h1") == Set(early) and s.queuedAt("h2") == Set(early))
      .expect(not(lookupValidityPerHub))

  run cursorLandsOnLyingReplicaControlTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverSubmit("h2", 1, early))
      .then(ask("early", 1))
      .then(deliverLookup("h2", 2, "early"))
      .then(deliverToShim(replyMail("h2", 2, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(2) }))
      .expect(lookupValidityPerHub)

  /// G3 survives. The Byzantine replica answers with another transaction; the
  /// shim compares txids and refuses it.
  run wrongTransactionIsRefusedTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(ask("early", 1))
      .then(hubReceiveWith(
        "h2", lookupMail("h2", 2, "early"), INotFound,
        h2.toLookupReplyOutput(2, FromIndexer(IFound({ body: Some(tight), height: 3 }))),
      ))
      .then(deliverToShim(replyMail("h2", 2, WFound({ body: Some(tight), height: 3 }))))
      .expect(lastEvent == Got({ query: "early", obs: NotFound, via: Some(2) }))
      .expect(txidAuthenticity)

  /// The per-hub guarantees survive for the honest hub. The Byzantine replica
  /// acks a migration it does not queue, and admits one the expiry rule
  /// refuses and publishes it late. Both are failures of that replica alone.
  ///
  /// And the wallet, told ok, has no honest hub holding its first migration:
  /// the honest hub's frame was never delivered. Under `DispatchOnly` that is
  /// not something replication promises.
  run honestReplicaKeepsItsGuaranteesTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(hubReceiveWith("h2", submitMail("h2", 1, early), INotFound, h2.toAckOutput(1, Admitted)))
      .expect(s.toldOk() == Set(early))
      .expect(audit.everQueued.get("h1") == Set() and audit.everQueued.get("h2") == Set())
      .expect(not(ackImpliesQueued) and ackImpliesQueuedForHonestHubs)
      .then(block)
      .then(hubFlushBeginWith("h1"))
      .then(hubFlushBeginWith("h2"))
      .then(sendToAll(tight))
      .then(deliverSubmit("h1", 2, tight))
      .then(hubReceiveWith(
        "h2", submitMail("h2", 3, tight), INotFound,
        { ...h2, queue: Map(tight -> 0) }.toAckOutput(3, Admitted),
      ))
      .expect(s.acks("h1", ShimAddr) == Set((2, WRefused(WExpiryTooTight))))
      .then(blocks(3))
      .then(hubFlushBeginWith("h1"))
      .then(hubFlushBeginWith("h2"))
      .expect(audit.offers == Set({ hub: "h2", payload: tight, height: 6, attempt: 0, nth: 0 }))
      .expect(not(offeredBeforeExpiry) and offeredBeforeExpiryForHonestHubs)
      .expect(conformingFirstOfferBeforeExpiryForHonestHubs)
}

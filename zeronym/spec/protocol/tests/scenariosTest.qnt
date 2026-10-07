// -*- mode: Bluespec; -*-

/// Scripted runs against the honest configurations, and the two witnesses that
/// need a Byzantine component: one run for each behaviour that must be
/// reachable, and one for each cause of each known gap.
///
/// A run is a sequence of the machine's own steps. Where a run says a property
/// fails, it also says what is in the wallet's log, the soup or the audit
/// record that makes it fail.
///
/// The schedule in every configuration: the chain starts at height 1, a flush
/// is scheduled at heights 3, 6, 9 and 12, and the mining margin is 2 blocks.
/// Shim nonces count up from 0, one per frame.

module baselineScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = baseline).* from "../protocol"

  def queue = s.hubs.get("h1").queue
  def tpAcks = s.acks("h1", ThirdPartyAddr)

  // ------------------------------------------------------------------------
  // Witnesses
  // ------------------------------------------------------------------------

  /// W1, W2, W3. A migration from send to mined, with the wallet polling.
  run pendingThenMempoolThenMinedTest =
    started
      .then(block)
      // Height 2. The shim diverts and answers at once; the operator sees
      // nothing.
      .then(submitTo("h1", 0, early))
      .expect(lastEvent == Sent({ input: Clean(early), obs: SentOk }))
      .expect(s.queuedAt("h1") == Set(early) and s.operator == Set())
      .then(lookUp("h1", 1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      // Height 3 is a flush boundary.
      .then(block)
      .then(flush("h1", [early], Accepted))
      .expect(s.queuedAt("h1") == Set() and s.onChain("early") == InMempool)
      .then(lookUp("h1", 2, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(2) }))
      .then(block)
      .then(chainMineWith("early"))
      .then(lookUp("h1", 3, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: 4 }), via: Some(3) }))
      .expect(wPending and wTxInMempool and wTxMined)
      .expect(operatorBlind and queuedBytesConfidential and txidAuthenticity and lookupValidityPerHub)
      .expect(offeredBeforeExpiry and conformingFirstOfferBeforeExpiry and ackImpliesQueued and wellFormed)
      .expect(vConformingFirstOfferJudged and conformingFirstOfferJudgedBeforeExpiry)
      .expect(statusNeverRegresses)

  /// A pass-through transaction goes to the operator and nowhere else.
  run passThroughIsForwardedTest =
    started
      .then(sendToAll(plain))
      .expect(lastEvent == Sent({ input: Clean(plain), obs: SentToOperator }))
      .expect(s.operator == Set(plain) and s.net == Set())
      .expect(vOperatorBlind and operatorBlind and vQueuedBytesConfidential and queuedBytesConfidential)

  /// W4. Each of the five refusals, in one hub's life.
  run everyRefusalTest =
    init
      // The hub has started and has seen no tip.
      .then(thirdPartySubmitWith(garbage, "h1"))
      .then(deliverSubmitFrom(ThirdPartyAddr, "h1", 0, garbage))
      .expect(wRefusedTipStale and tpAcks == Set((0, WRefused(WTipStale))))
      .then(allObserve)
      .then(thirdPartySubmitWith(bloat, "h1"))
      .then(deliverSubmitFrom(ThirdPartyAddr, "h1", 1, bloat))
      .expect(wRefusedTooLarge and tpAcks.contains((1, WRefused(WTooLarge))))
      // Height 3, and the flush scheduled there has run. The next is at 6,
      // which a transaction expiring at 5 does not survive.
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(submitTo("h1", 0, tight))
      .expect(wRefusedExpiryTooTight and s.acks("h1", ShimAddr) == Set((0, WRefused(WExpiryTooTight))))
      // Two admissions fill the queue. The network then delivers the third
      // party's first frame a second time.
      .then(submitTo("h1", 1, junk))
      .then(submitTo("h1", 2, early))
      .expect(s.queuedAt("h1") == Set(junk, early))
      .then(deliverSubmitFrom(ThirdPartyAddr, "h1", 0, garbage))
      .expect(wRefusedFull and not(wRefusedDraining))
      // Draining is checked before size: the oversize frame, delivered again,
      // is now refused for that. On the wire it is the same refusal as full.
      .then(hubBeginDrainWith("h1"))
      .then(deliverSubmitFrom(ThirdPartyAddr, "h1", 1, bloat))
      .expect(wRefusedDraining)
      .expect(tpAcks == Set(
        (0, WRefused(WTipStale)), (0, WRefused(WQueueFull)),
        (1, WRefused(WTooLarge)), (1, WRefused(WQueueFull)),
      ))
      .expect(audit.everQueued.get("h1") == Set(junk, early))

  /// W8. The accepted disclosure: a third party that knows a txid is told it
  /// is queued, and is not given the bytes.
  run thirdPartyLearnsItIsQueuedTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early", "h1"))
      .then(deliverLookupFrom(ThirdPartyAddr, "h1", 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set((0, "h1", WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(wQueuedDisclosed)
      .expect(s.tpLearned() == Set() and queuedBytesConfidential)

  /// W9. A queued payload the hub cannot parse has no txid to be found by.
  run unparseableIsQueuedAndMissedTest =
    started
      .then(submitTo("h1", 0, junk))
      .expect(lastEvent == Sent({ input: Clean(junk), obs: SentOk }) and s.queuedAt("h1") == Set(junk))
      .then(lookUp("h1", 1, "junk"))
      .expect(lastEvent == Got({ query: "junk", obs: NotFound, via: Some(1) }))
      .expect(wUnparseableMissed and lookupValidityPerHub)

  /// W17. Anyone can put a payload in a hub's queue.
  run thirdPartyPayloadIsQueuedTest =
    started
      .then(thirdPartySubmitWith(garbage, "h1"))
      .then(deliverSubmitFrom(ThirdPartyAddr, "h1", 0, garbage))
      .expect(tpAcks == Set((0, WAccepted)) and s.queuedAt("h1") == Set(garbage))
      .expect(wThirdPartyPayloadQueued and queuedBytesConfidential)

  // ------------------------------------------------------------------------
  // K1. Told ok, and no hub ever has it
  // ------------------------------------------------------------------------

  /// K1a. The only hub refuses the frame after the wallet was told ok.
  run toldOkThenRefusedTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(submitTo("h1", 0, tight))
      .expect(s.wallet.log == [Sent({ input: Clean(tight), obs: SentOk })])
      .expect(s.acks("h1", ShimAddr) == Set((0, WRefused(WExpiryTooTight))))
      .expect(audit.everQueued.get("h1") == Set())
      .expect(wToldRefusedEverywhere and not(toldImpliesQueued))

  /// K1b. The frame is never delivered. Nothing obliges the network to.
  run toldOkAndNeverDeliveredTest =
    started
      .then(block)
      .then(sendToAll(early))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(s.net == Set(submitMail("h1", 0, early)))
      .expect(audit.everQueued.get("h1") == Set())
      .expect(wToldNeverDelivered and not(toldImpliesQueued))

  // ------------------------------------------------------------------------
  // K2. The status a wallet sees goes backwards
  // ------------------------------------------------------------------------

  /// K2a. Two polls, answered in order, delivered out of order.
  run repliesReorderedTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(ask("early", 0))
      .then(deliverLookup("h1", 1, "early"))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(ask("early", 0))
      .then(deliverLookup("h1", 2, "early"))
      .then(deliverToShim(replyMail("h1", 2, WFound({ body: Some(early), height: MEMPOOL_HEIGHT }))))
      .then(deliverToShim(replyMail("h1", 1, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(s.wallet.log == [
        Sent({ input: Clean(early), obs: SentOk }),
        Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(2) }),
        Got({ query: "early", obs: Pending, via: Some(1) }),
      ])
      // Each answer was true when it was given.
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// K2b. The wallet sends published bytes again. The hub's memory of them
  /// went with the flush, so they are admitted and pending once more.
  run walletResendsPublishedTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(lookUp("h1", 1, "early"))
      .then(submitTo("h1", 2, early))
      .expect(s.queuedAt("h1") == Set(early) and s.onChain("early") == InMempool)
      .then(lookUp("h1", 3, "early"))
      .expect(s.wallet.log.slice(1, 4) == [
        Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(1) }),
        Sent({ input: Clean(early), obs: SentOk }),
        Got({ query: "early", obs: Pending, via: Some(3) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// K2c. The same, done by a third party: the bytes are public once
  /// published, and submission is open to anyone.
  run thirdPartyResubmitsPublishedTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(lookUp("h1", 1, "early"))
      .expect(s.tpPayloads().contains(early))
      .then(thirdPartySubmitWith(early, "h1"))
      .then(deliverSubmitFrom(ThirdPartyAddr, "h1", 0, early))
      .then(lookUp("h1", 2, "early"))
      .expect(s.wallet.log.slice(1, 3) == [
        Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(1) }),
        Got({ query: "early", obs: Pending, via: Some(2) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// K2d. The flush window: the queue is empty and the chain does not have
  /// the batch yet.
  run flushWindowTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(lookUp("h1", 1, "early"))
      .then(block)
      .then(hubFlushBeginWith("h1"))
      .expect(s.inFlightAt("h1") == Set(early) and s.onChain("early") == Absent)
      .then(lookUp("h1", 2, "early"))
      .expect(s.wallet.log.slice(1, 3) == [
        Got({ query: "early", obs: Pending, via: Some(1) }),
        Got({ query: "early", obs: NotFound, via: Some(2) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// K2e. The node rejects the transaction at flush. It was pending; now it
  /// is nowhere.
  run rejectedAtFlushTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(lookUp("h1", 1, "early"))
      .then(block)
      .then(flush("h1", [early], Rejected))
      .expect(s.queuedAt("h1") == Set() and s.inFlightAt("h1") == Set() and s.onChain("early") == Absent)
      .then(lookUp("h1", 2, "early"))
      .expect(s.wallet.log.slice(1, 3) == [
        Got({ query: "early", obs: Pending, via: Some(1) }),
        Got({ query: "early", obs: NotFound, via: Some(2) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)
}

module awaitAckScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = awaitAck).* from "../protocol"

  /// Under `AwaitVerdict` the wallet hears nothing until the hub answers, and
  /// then hears the hub's decision.
  run toldWhatTheHubDecidedTest =
    started
      .then(block)
      .then(sendToAll(early))
      .expect(s.wallet.log == [] and s.sentByWallet() == Set(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverToShim(ackMail("h1", 0, WAccepted)))
      .expect(lastEvent == Sent({ input: Clean(early), obs: SentOk }))
      .expect(vToldImpliesQueued and toldImpliesQueued)
      // A transaction the hub refuses is reported as refused.
      .then(blocks(1))
      .then(sendToAll(tight))
      .then(deliverSubmit("h1", 1, tight))
      .then(deliverToShim(ackMail("h1", 1, WRefused(WExpiryTooTight))))
      .expect(lastEvent == Sent({ input: Clean(tight), obs: SentRejected }))
      // And silence fails closed.
      .then(sendToAll(junk))
      .then(timeOutAck(2))
      .expect(lastEvent == Sent({ input: Clean(junk), obs: SendUnavailable }))
      .expect(toldImpliesQueued)

}

module replicatedScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = replicated).* from "../protocol"

  /// Every hub that receives a migration queues it and publishes it.
  /// W14. Two hubs publish the same payload in their own flushes.
  run publishedByBothHubsTest =
    started
      .then(block)
      .then(sendToAll(early))
      .expect(s.net == Set(submitMail("h1", 0, early), submitMail("h2", 1, early)))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverSubmit("h2", 1, early))
      .then(block)
      .then(flush("h1", [early], Accepted))
      .then(flush("h2", [early], AlreadyKnown))
      .expect(audit.offers.map(offer => offer.hub) == Set("h1", "h2"))
      .expect(wPublishedByTwoHubs)

  /// K1c. The sweep stops after the first address. One frame was handed over,
  /// so the wallet is told ok; the second hub is never sent the migration.
  run toldOkAfterPrefixSendTest =
    started
      .then(block)
      .then(sends(Clean(early), 1))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(s.net == Set(submitMail("h1", 0, early)))
      .expect(wToldPrefixOnly)

  /// K2f. One hub has the migration queued and says so. The next poll starts
  /// at the other hub, which never received it; its not-found is final.
  run hubsDisagreeTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(ask("early", 0))
      .then(deliverLookup("h1", 2, "early"))
      .then(deliverToShim(replyMail("h1", 2, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .then(ask("early", 1))
      .then(deliverLookup("h2", 3, "early"))
      .then(deliverToShim(replyMail("h2", 3, WNotFound)))
      .expect(s.wallet.log.slice(1, 3) == [
        Got({ query: "early", obs: Pending, via: Some(2) }),
        Got({ query: "early", obs: NotFound, via: Some(3) }),
      ])
      // No lookup is outstanding: the shim asked nobody else. (Nonces 0 and 1
      // are the acks nobody waits for.)
      .expect(s.shim.waiters.keys() == Set(0, 1))
      // Each hub told the truth about itself.
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// W13. A lookup whose first hub stays silent moves on, and the next hub
  /// answers.
  run lookupFailsOverOnTimeoutTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(ask("early", 1))
      .expect(s.lookups(ShimAddr) == Set((2, "h2", "early")))
      .then(timeOutLookup(2))
      .expect(s.lookups(ShimAddr) == Set((2, "h2", "early"), (3, "h1", "early")))
      .then(deliverLookup("h1", 3, "early"))
      .expect(wFailoverAnswered)
      .then(deliverToShim(replyMail("h1", 3, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(3) }))
      .expect(lookupValidityPerHub)
}

module byzHubScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = byzHub).* from "../protocol"

  /// W16. A Byzantine hub serves a twin of the wallet's transaction at a
  /// height the chain has not reached. The shim checks the txid, which a twin
  /// shares, and nothing else: it serves both.
  run twinAtFalseHeightIsServedTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(ask("early", 0))
      .then(hubReceiveWith(
        "h1", lookupMail("h1", 1, "early"), INotFound,
        s.hubs.get("h1").toLookupReplyOutput(1, FromIndexer(IFound({ body: Some(earlyTwin), height: 9 }))),
      ))
      .then(deliverToShim(replyMail("h1", 1, WFound({ body: Some(earlyTwin), height: 9 }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: earlyTwin, height: 9 }), via: Some(1) }))
      .expect(s.height() == 2 and s.onChain("early") == Absent)
      .expect(wTwinServed and wFalseHeightServed)
      .expect(txidAuthenticity and not(lookupValidityPerHub))
}

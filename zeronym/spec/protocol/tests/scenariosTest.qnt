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

  /// W5, W6, W7, W12. An indexer that cannot be reached: entries are requeued,
  /// dropped as expired, dropped as exhausted, and held past capacity.
  run requeueAndDropTest =
    started
      .then(submitTo("h1", 0, junk))
      .then(block)
      // Height 2: a transaction expiring at 5 survives the flush at 3.
      .then(submitTo("h1", 1, tight))
      .then(block)
      .then(hubFlushBeginWith("h1"))
      // While the batch is out, two fresh admissions fill the queue.
      .then(submitTo("h1", 2, early))
      .then(thirdPartySubmitWith(garbage, "h1"))
      .then(deliverSubmitFrom(ThirdPartyAddr, "h1", 0, garbage))
      .expect(s.queuedAt("h1") == Set(early, garbage) and s.inFlightAt("h1") == Set(junk, tight))
      .then(judge("h1", junk, Retryable))
      .then(judge("h1", tight, Retryable))
      .then(hubFlushEndWith("h1"))
      // `junk` never expires and comes back, over capacity. `tight` would not
      // survive the flush at 6 and is given up on.
      .expect(queue == Map(early -> 0, garbage -> 0, junk -> 1))
      .expect(audit.dropped == Set({ hub: "h1", payload: tight, attempts: 0 }))
      .expect(wRequeued and wDroppedExpired and wQueueOverCapacity and not(wDroppedExhausted))
      // `tight` was acknowledged, and is now held nowhere and judged by nobody:
      // a third way to lose an acknowledged payload (K5).
      .expect(s.acks("h1", ShimAddr).contains((1, WAccepted)) and not(ackedIsHeldOrSettled))
      .then(blocks(3))
      .then(hubFlushBeginWith("h1"))
      .then(judge("h1", early, Accepted))
      .then(judge("h1", garbage, Accepted))
      .then(judge("h1", junk, Retryable))
      .then(hubFlushEndWith("h1"))
      .expect(queue == Map(junk -> 2))
      // The third failure is one more than the attempts allowed.
      .then(blocks(3))
      .then(flush("h1", [junk], Retryable))
      .expect(queue == Map() and wDroppedExhausted)
      .expect(audit.dropped.contains({ hub: "h1", payload: junk, attempts: 2 }))
      .expect(offeredBeforeExpiry and wellFormed)

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

  // ------------------------------------------------------------------------
  // K5. Acknowledged, then lost
  // ------------------------------------------------------------------------

  /// K5a. The hub crashes after the ack. The queue was in memory.
  run ackedThenCrashedTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .expect(s.acks("h1", ShimAddr) == Set((0, WAccepted)) and ackedIsHeldOrSettled)
      .then(hubCrashWith("h1"))
      .expect(s.hubs.get("h1") == downHub(HUB_PARAMS))
      .expect(audit.offers == Set() and s.onChain("early") == Absent)
      .expect(not(ackedIsHeldOrSettled) and ackImpliesQueued)

  /// K5b. The final flush of a draining hub finds the indexer unreachable.
  /// The entry was offered and nothing judged it; the hub stops, and the
  /// transaction is held nowhere.
  run ackedThenLostAtDrainTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(hubBeginDrainWith("h1"))
      .then(flush("h1", [early], Retryable))
      .expect(s.hubs.get("h1").phase == Stopped)
      .expect(s.queuedAt("h1") == Set() and s.inFlightAt("h1") == Set() and s.onChain("early") == Absent)
      .expect(s.acks("h1", ShimAddr) == Set((0, WAccepted)))
      .expect(audit.offers == Set({ hub: "h1", payload: early, height: 2, attempt: 0, nth: 0 }))
      .expect(audit.verdicts == Set({ hub: "h1", payload: early, height: 2, nth: 0, final: false }))
      .expect(not(ackedIsHeldOrSettled))

  // ------------------------------------------------------------------------
  // K6, control
  // ------------------------------------------------------------------------

  /// The wallet inputs of K6 under a timely tip. The second requeue is judged
  /// at tip 9, finds that an expiry of 11 does not survive the flush at 12,
  /// and drops the entry: it is never offered past its expiry.
  run requeueUnderTimelyTipDropsTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(block)
      .then(sendToAll(late))
      .then(block)
      .then(deliverSubmit("h1", 0, late))
      .then(block)
      .then(flush("h1", [late], Retryable))
      .expect(queue == Map(late -> 1))
      .then(blocks(3))
      .then(flush("h1", [late], Retryable))
      .expect(queue == Map())
      .expect(audit.dropped == Set({ hub: "h1", payload: late, attempts: 1 }))
      .expect(audit.offers.map(offer => offer.height) == Set(6, 9))
      .expect(conformingEveryOfferBeforeExpiry)
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

  /// K5a under `AwaitVerdict`: told ok on the hub's word, admitted, and lost
  /// to a crash, with every component honest.
  run toldOkAdmittedThenLostTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(deliverSubmit("h1", 0, early))
      .then(deliverToShim(ackMail("h1", 0, WAccepted)))
      .then(hubCrashWith("h1"))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(s.queuedAt("h1") == Set() and audit.offers == Set() and s.onChain("early") == Absent)
      .expect(toldImpliesQueued and not(ackedIsHeldOrSettled))
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

module flakyTipScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = flakyTip).* from "../protocol"

  /// K3. A tight-expiry transaction is admitted against a tip reported one
  /// block back, below a boundary the hub has already flushed. Admission
  /// reasons that the next flush is at 3; it is at 6, after the expiry.
  run tightExpiryAdmittedBehindFlushedBoundaryTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(hubObserveTipWith("h1", 2))
      .then(submitTo("h1", 0, tight))
      .expect(s.queuedAt("h1") == Set(tight))
      .expect(audit.admitted.get(("h1", tight)) == { height: 3, tip: 2 })
      .then(blocks(3))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: tight, height: 6, attempt: 0, nth: 0 }))
      // Not a supported wallet: the guarantee for those is untouched.
      .expect(not(conforming(tight, MIN_WALLET_EXPIRY)))
      .expect(not(offeredBeforeExpiry) and conformingFirstOfferBeforeExpiry)

  /// A supported wallet's transaction, admitted the same way and then flushed
  /// one block late because the tip is reported one block back. The reorg
  /// slack leaves it exactly the mining margin at the offer. One block arrives
  /// while the batch is in flight, which the margin is there to pay for, and
  /// the node still accepts it.
  run conformingSurvivesRegressionTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(hubObserveTipWith("h1", 2))
      .then(submitTo("h1", 0, early))
      .then(blocks(2))
      .then(2.reps(_ => chainAdvance))
      .then(hubObserveTipWith("h1", 6))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: early, height: 7, attempt: 0, nth: 0 }))
      .expect(early.expiry == Some(7 + MINING_MARGIN))
      .expect(vConformingOfferAdmittedBehind and conformingFirstOfferBeforeExpiry and offeredBeforeExpiry)
      .then(chainAdvance)
      .expect(s.height() == 8 and s.flightBlocks.get("h1") == MAX_FLIGHT_BLOCKS and not(chainMayAdvance))
      .then(judge("h1", early, Accepted))
      .then(hubFlushEndWith("h1"))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.verdicts == Set({ hub: "h1", payload: early, height: 8, nth: 0, final: true }))
      .expect(vConformingFirstOfferJudged and conformingFirstOfferJudgedBeforeExpiry)
}

module flakyTipSlowFlightScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = flakyTipSlowFlight).* from "../protocol"

  /// K7. The steps of `conformingSurvivesRegressionTest`, with the batch in
  /// flight for two blocks, as many as the margin reserves. The offer left the
  /// whole margin, and G6b holds. By the time the node looks, the transaction
  /// can no longer be mined: the budget had already spent the slack, and the
  /// flight spent the margin.
  run slowFlightSpendsTheMarginTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(hubObserveTipWith("h1", 2))
      .then(submitTo("h1", 0, early))
      .then(blocks(2))
      .then(2.reps(_ => chainAdvance))
      .then(hubObserveTipWith("h1", 6))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: early, height: 7, attempt: 0, nth: 0 }))
      .then(2.reps(_ => chainAdvance))
      .expect(s.height() == 9 and early.expiry == Some(9))
      // An honest node cannot take it now; it is rejected.
      .expect(not(s.indexer.isAcceptable(early)))
      .expect(not(honestIndexerOutputs(s.indexer, BroadcastIInput(early)).contains(VerdictOutput(Accepted))))
      .then(judge("h1", early, Rejected))
      .expect(audit.verdicts == Set({ hub: "h1", payload: early, height: 9, nth: 0, final: true }))
      .expect(not(flightWithinMargin))
      .expect(conformingFirstOfferBeforeExpiry and not(conformingFirstOfferJudgedBeforeExpiry))
}

module flakyTipNoSlackScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = flakyTipNoSlack).* from "../protocol"

  /// This configuration's migration from a supported wallet: built at height
  /// 2, expiring exactly at the floor of 6 blocks.
  pure val atFloor = orchard("early", 2, 8)

  /// K3'. The steps of `conformingSurvivesRegressionTest` with the expiry
  /// floor equal to the three-term budget. The flush is one block late and
  /// the transaction misses the mining margin by that block.
  run conformingMissesMarginWithoutSlackTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(hubObserveTipWith("h1", 2))
      .then(submitTo("h1", 0, atFloor))
      .then(blocks(2))
      .then(2.reps(_ => chainAdvance))
      .then(hubObserveTipWith("h1", 6))
      .then(hubFlushBeginWith("h1"))
      .expect(conforming(atFloor, MIN_WALLET_EXPIRY))
      .expect(audit.admitted.get(("h1", atFloor)).height == 3)
      .expect(audit.offers == Set({ hub: "h1", payload: atFloor, height: 7, attempt: 0, nth: 0 }))
      .expect(not(conformingFirstOfferBeforeExpiry))
}

module staleLagScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = staleLag).* from "../protocol"

  def queue = s.hubs.get("h1").queue

  /// K4. The hub hears nothing after height 5, one block short of the flush
  /// at 6, while the chain goes on. Its cadence still follows the tip it last
  /// saw, so nothing is flushed until it goes stale at height 8. A supported
  /// wallet's transaction, expiring at 9, is offered at 8: not yet expired,
  /// with one block of margin where two are reserved. One block arrives while
  /// the batch is in flight, as the margin allows for, and the node can no
  /// longer take it.
  run silenceAcrossBoundaryMissesMarginTest =
    started
      .then(block)
      .then(sendToAll(early))
      .then(block)
      .then(hubFlushBeginWith("h1"))
      .then(deliverSubmit("h1", 0, early))
      .then(blocks(2))
      .then(3.reps(_ => chainAdvance))
      .expect(s.height() == 8 and s.hubs.get("h1").tip == Some(5) and not(s.hubs.get("h1").isFlushDue()))
      .then(hubTipStaleWith("h1", 8))
      .then(hubFlushBeginWith("h1"))
      .expect(conforming(early, MIN_WALLET_EXPIRY))
      .expect(audit.admitted.get(("h1", early)) == { height: 3, tip: 3 })
      .expect(audit.offers == Set({ hub: "h1", payload: early, height: 8, attempt: 0, nth: 0 }))
      .expect(early.expiry == Some(9) and MINING_MARGIN == 2)
      .expect(not(conformingFirstOfferBeforeExpiry) and not(offeredBeforeExpiry))
      .then(chainAdvance)
      .expect(s.height() == 9 and s.flightBlocks.get("h1") == MAX_FLIGHT_BLOCKS and flightWithinMargin)
      .expect(not(s.indexer.isAcceptable(early)))
      .then(judge("h1", early, Rejected))
      .expect(audit.verdicts == Set({ hub: "h1", payload: early, height: 9, nth: 0, final: true }))
      .expect(not(conformingFirstOfferJudgedBeforeExpiry))

  /// K6. A stale hub's flush finds the indexer unreachable, twice. Requeue
  /// judges the entry at the observed tip, which stopped at 5: the next flush
  /// it knows of is the one at 6, so an expiry of 11 looks safe both times.
  /// The free-running schedule offers it again at 12.
  run requeuedPastExpiryTest =
    started
      .then(blocks(2))
      .then(hubFlushBeginWith("h1"))
      .then(block)
      .then(sendToAll(late))
      .then(block)
      .then(deliverSubmit("h1", 0, late))
      .then(3.reps(_ => chainAdvance))
      .then(hubTipStaleWith("h1", 8))
      .then(flush("h1", [late], Retryable))
      .expect(queue == Map(late -> 1))
      .then(chainAdvance)
      .then(hubTipStaleWith("h1", 9))
      .then(flush("h1", [late], Retryable))
      .expect(queue == Map(late -> 2) and s.hubs.get("h1").tip == Some(5))
      .then(chainAdvance)
      .then(hubTipStaleWith("h1", 10))
      .then(chainAdvance)
      .then(hubTipStaleWith("h1", 11))
      .then(chainAdvance)
      .then(hubTipStaleWith("h1", 12))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set(
        { hub: "h1", payload: late, height: 8, attempt: 0, nth: 0 },
        { hub: "h1", payload: late, height: 9, attempt: 1, nth: 1 },
        { hub: "h1", payload: late, height: 12, attempt: 2, nth: 2 },
      ))
      .expect(late.expiry == Some(11))
      // The first offer was in time, which is all G6b covers.
      .expect(not(conformingEveryOfferBeforeExpiry) and conformingFirstOfferBeforeExpiry)

  /// W18. A stale hub's free-running clock reads 6 at true height 5, and the
  /// flush scheduled for 6 runs a block early.
  run freeRunningClockFlushesEarlyTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(3.reps(_ => chainAdvance))
      .then(hubTipStaleWith("h1", 6))
      .then(hubFlushBeginWith("h1"))
      .expect(s.height() == 5 and s.hubs.get("h1").cadence == FreeRunning(6))
      .expect(s.inFlightAt("h1") == Set(early))
      .expect(wEarlyFreeRunFlush and conformingFirstOfferBeforeExpiry)

  /// A stale hub refuses submissions until it sees the tip move again.
  run staleHubRefusesTest =
    started
      .then(3.reps(_ => chainAdvance))
      .then(hubTipStaleWith("h1", 4))
      .then(submitTo("h1", 0, early))
      .expect(wRefusedTipStale and s.queuedAt("h1") == Set())
      .then(observe("h1"))
      .expect(s.hubs.get("h1").phase == Running and s.hubs.get("h1").cadence == Tracking)
      .then(deliverSubmit("h1", 0, early))
      .expect(s.queuedAt("h1") == Set(early))
}

module staleLagWithSlackScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = staleLagWithSlack).* from "../protocol"

  /// This configuration's migration from a supported wallet: built at height
  /// 2, expiring at the floor of 8 blocks.
  pure val atFloor = orchard("early", 2, 10)

  /// The steps of K4 with an expiry floor one block higher, which is the
  /// relation `staleSlackFits` asks for. The same late flush leaves the
  /// margin, and after a block in flight the node accepts.
  run sameSilenceWithSlackKeepsMarginTest =
    started
      .then(block)
      .then(sendToAll(atFloor))
      .then(block)
      .then(hubFlushBeginWith("h1"))
      .then(deliverSubmit("h1", 0, atFloor))
      .then(blocks(2))
      .then(3.reps(_ => chainAdvance))
      .then(hubTipStaleWith("h1", 8))
      .then(hubFlushBeginWith("h1"))
      .expect(audit.offers == Set({ hub: "h1", payload: atFloor, height: 8, attempt: 0, nth: 0 }))
      .expect(vConformingFirstOfferBeforeExpiry and conformingFirstOfferBeforeExpiry)
      .then(chainAdvance)
      .expect(s.height() == 9 and s.flightBlocks.get("h1") == MAX_FLIGHT_BLOCKS)
      .then(judge("h1", atFloor, Accepted))
      .expect(s.onChain("early") == InMempool)
      .expect(audit.verdicts == Set({ hub: "h1", payload: atFloor, height: 9, nth: 0, final: true }))
      .expect(conformingFirstOfferJudgedBeforeExpiry)

  /// This configuration's second migration: built at height 4, expiring at
  /// the floor.
  pure val lateAtFloor = orchard("late", 4, 12)

  /// Found by simulation; the slack does not cover it. A stale hub's
  /// free-running clock reads 6 at true height 4, and the flush scheduled for
  /// 6 runs then, with nothing to publish. When the hub sees the tip again the
  /// chain is at 5, and admission, which knows the tip and not the schedule's
  /// history, counts on the flush at 6. That flush has already happened. The
  /// transaction waits for the one at 9, and a second, shorter silence makes
  /// that one late too: it is offered at 11 with expiry 12, one block of
  /// margin where two are reserved, and a block in flight uses that up.
  ///
  /// Neither half is enough alone at these numbers. Without the early flush
  /// the transaction goes out at 6; without the second silence, at 9.
  run earlyFlushSpendsTheNextEpochTest =
    started
      .then(3.reps(_ => chainAdvance))
      .then(hubTipStaleWith("h1", 6))
      .then(hubFlushBeginWith("h1"))
      .expect(s.height() == 4 and s.hubs.get("h1").lastEpoch == Some(2))
      .then(sendToAll(lateAtFloor))
      .then(block)
      .expect(s.hubs.get("h1").cadence == Tracking and s.hubs.get("h1").tip == Some(5))
      .then(deliverSubmit("h1", 0, lateAtFloor))
      .expect(audit.admitted.get(("h1", lateAtFloor)) == { height: 5, tip: 5 })
      .then(blocks(3))
      // Height 8. The boundary at 6 has passed and nothing was flushed.
      .expect(audit.offers == Set() and s.queuedAt("h1") == Set(lateAtFloor))
      .then(3.reps(_ => chainAdvance))
      .then(hubTipStaleWith("h1", 11))
      .then(hubFlushBeginWith("h1"))
      .expect(conforming(lateAtFloor, MIN_WALLET_EXPIRY) and staleSlackFits)
      .expect(audit.offers == Set({ hub: "h1", payload: lateAtFloor, height: 11, attempt: 0, nth: 0 }))
      .expect(not(conformingFirstOfferBeforeExpiry))
      .then(chainAdvance)
      .expect(s.height() == 12 and not(s.indexer.isAcceptable(lateAtFloor)))
      .then(judge("h1", lateAtFloor, Rejected))
      .expect(not(conformingFirstOfferJudgedBeforeExpiry))
}

module byzIndexerScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = byzIndexer).* from "../protocol"

  /// W15. Premature flush. The indexer reports tip 3 at true height 2; the
  /// hub believes the boundary has come and publishes what it holds. One
  /// lying endpoint is enough for this: the tip is the maximum over endpoints.
  run tipAheadOfChainFlushesEarlyTest =
    started
      .then(block)
      .then(submitTo("h1", 0, early))
      .then(hubObserveTipWith("h1", 3))
      .then(hubFlushBeginWith("h1"))
      .expect(s.height() == 2 and s.hubs.get("h1").tip == Some(3))
      .expect(s.inFlightAt("h1") == Set(early))
      .expect(wPrematureFlush)
      // The transaction is published early, which costs it nothing.
      .expect(offeredBeforeExpiry and conformingFirstOfferBeforeExpiry)
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

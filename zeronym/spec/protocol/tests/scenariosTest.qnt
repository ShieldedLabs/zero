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
  import abstractHub.* from "../abstractHub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = baseline).* from "../protocol"

  def tpAcks = s.acks(ThirdPartyAddr)

  // ------------------------------------------------------------------------
  // Witnesses
  // ------------------------------------------------------------------------

  /// W1, W2, W3. A migration from send to mined, with the wallet polling.
  run pendingThenMempoolThenMinedTest =
    init
      .then(block)
      // Height 2. The shim diverts and answers at once; the operator sees
      // nothing.
      .then(submitTo(0, early))
      .expect(lastEvent == Sent({ input: Clean(early), obs: SentOk }))
      .expect(s.hub.queue == Set(early) and s.operator == Set())
      .then(lookUp(1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      // Height 3 is a flush boundary.
      .then(block)
      .then(flush([early], Accepted))
      .expect(s.hub.queue == Set() and s.onChain("early") == InMempool)
      .then(lookUp(2, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(2) }))
      .then(block)
      .then(chainMineWith("early"))
      .then(lookUp(3, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: 4 }), via: Some(3) }))
      .expect(wPending and wTxInMempool and wTxMined)
      .expect(operatorBlind and queuedBytesConfidential and txidAuthenticity and lookupValidityPerHub)
      .expect(ackImpliesQueued and wellFormed and statusNeverRegresses)

  /// A pass-through transaction goes to the operator and nowhere else.
  run passThroughIsForwardedTest =
    init
      .then(sendToHub(plain))
      .expect(lastEvent == Sent({ input: Clean(plain), obs: SentToOperator }))
      .expect(s.operator == Set(plain) and s.net == Set())
      .expect(vOperatorBlind and operatorBlind and vQueuedBytesConfidential and queuedBytesConfidential)

  /// W8. The accepted disclosure: a third party that knows a txid is told it
  /// is queued, and is not given the bytes.
  run thirdPartyLearnsItIsQueuedTest =
    init
      .then(block)
      .then(submitTo(0, early))
      .then(thirdPartyLearnsTxidWith("early"))
      .then(thirdPartyLookupWith("early"))
      .then(deliverLookupFrom(ThirdPartyAddr, 0, "early", INotFound))
      .expect(s.replies(ThirdPartyAddr) == Set((0, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
      .expect(wQueuedDisclosed)
      .expect(s.tpLearned() == Set() and queuedBytesConfidential)

  /// A lookup that gets no reply in time fails closed, and G4 holds of it.
  run lookupTimesOutTest =
    init
      .then(block)
      .then(submitTo(0, early))
      .then(ask("early"))
      .then(timeOutLookup(1))
      .expect(lastEvent == Got({ query: "early", obs: Unavailable, via: None }))
      .expect(s.shim.waiters == Map() and lookupValidityPerHub)

  /// W9. A queued payload the hub cannot parse has no txid to be found by.
  run unparseableIsQueuedAndMissedTest =
    init
      .then(submitTo(0, junk))
      .expect(lastEvent == Sent({ input: Clean(junk), obs: SentOk }) and s.hub.queue == Set(junk))
      .then(lookUp(1, "junk"))
      .expect(lastEvent == Got({ query: "junk", obs: NotFound, via: Some(1) }))
      .expect(wUnparseableMissed and lookupValidityPerHub)

  /// W17. Anyone can put a payload in a hub's queue.
  run thirdPartyPayloadIsQueuedTest =
    init
      .then(thirdPartySubmitWith(garbage))
      .then(deliverSubmitFrom(ThirdPartyAddr, 0, garbage))
      .expect(tpAcks == Set((0, WAccepted)) and s.hub.queue == Set(garbage))
      .expect(wThirdPartyPayloadQueued and queuedBytesConfidential)

  // ------------------------------------------------------------------------
  // K1. Told ok, and no hub ever has it
  // ------------------------------------------------------------------------

  /// K1a. The only hub refuses the frame after the wallet was told ok. Its
  /// realisation, refused for its expiry after the flush at 3, is in
  /// `realisedRunsTest`.
  run toldOkThenRefusedTest =
    init
      .then(block)
      .then(sendToHub(tight))
      .then(answerSubmitFrom(ShimAddr, 0, tight, WRefused(WExpiryTooTight)))
      .expect(s.wallet.log == [Sent({ input: Clean(tight), obs: SentOk })])
      .expect(s.acks(ShimAddr) == Set((0, WRefused(WExpiryTooTight))))
      .expect(audit.everQueued == Set())
      .expect(wToldRefusedEverywhere)

  /// K1b. The frame is never delivered. Nothing obliges the network to.
  run toldOkAndNeverDeliveredTest =
    init
      .then(block)
      .then(sendToHub(early))
      .expect(s.wallet.log == [Sent({ input: Clean(early), obs: SentOk })])
      .expect(s.net == Set(submitMail(0, early)))
      .expect(audit.everQueued == Set())
      .expect(wToldNeverDelivered)

  // ------------------------------------------------------------------------
  // K2. The status a wallet sees goes backwards
  // ------------------------------------------------------------------------

  /// K2a. Two polls, answered in order, delivered out of order.
  run repliesReorderedTest =
    init
      .then(block)
      .then(submitTo(0, early))
      .then(ask("early"))
      .then(deliverLookup(1, "early"))
      .then(block)
      .then(flush([early], Accepted))
      .then(ask("early"))
      .then(deliverLookup(2, "early"))
      .then(deliverToShim(replyMail(2, WFound({ body: Some(early), height: MEMPOOL_HEIGHT }))))
      .then(deliverToShim(replyMail(1, WFound({ body: None, height: MEMPOOL_HEIGHT }))))
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
    init
      .then(block)
      .then(submitTo(0, early))
      .then(block)
      .then(flush([early], Accepted))
      .then(lookUp(1, "early"))
      .then(submitTo(2, early))
      .expect(s.hub.queue == Set(early) and s.onChain("early") == InMempool)
      .then(lookUp(3, "early"))
      .expect(s.wallet.log.slice(1, 4) == [
        Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(1) }),
        Sent({ input: Clean(early), obs: SentOk }),
        Got({ query: "early", obs: Pending, via: Some(3) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// K2c. The same, done by a third party: the bytes are public once
  /// published, and submission is open to anyone.
  run thirdPartyResubmitsPublishedTest =
    init
      .then(block)
      .then(submitTo(0, early))
      .then(block)
      .then(flush([early], Accepted))
      .then(lookUp(1, "early"))
      .expect(s.tpPayloads().contains(early))
      .then(thirdPartySubmitWith(early))
      .then(deliverSubmitFrom(ThirdPartyAddr, 0, early))
      .then(lookUp(2, "early"))
      .expect(s.wallet.log.slice(1, 3) == [
        Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(1) }),
        Got({ query: "early", obs: Pending, via: Some(2) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// K2d. The flush window: the queue is empty and the chain does not have
  /// the batch yet.
  run flushWindowTest =
    init
      .then(block)
      .then(submitTo(0, early))
      .then(lookUp(1, "early"))
      .then(block)
      .then(hubTake)
      .expect(s.hub.held == Set(early) and s.onChain("early") == Absent)
      .then(lookUp(2, "early"))
      .expect(s.wallet.log.slice(1, 3) == [
        Got({ query: "early", obs: Pending, via: Some(1) }),
        Got({ query: "early", obs: NotFound, via: Some(2) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)

  /// K2e. The node rejects the transaction at flush. It was pending; now it
  /// is nowhere.
  run rejectedAtFlushTest =
    init
      .then(block)
      .then(submitTo(0, early))
      .then(lookUp(1, "early"))
      .then(block)
      .then(flush([early], Rejected))
      .expect(s.hub.queue == Set() and s.hub.held == Set() and s.onChain("early") == Absent)
      .then(lookUp(2, "early"))
      .expect(s.wallet.log.slice(1, 3) == [
        Got({ query: "early", obs: Pending, via: Some(1) }),
        Got({ query: "early", obs: NotFound, via: Some(2) }),
      ])
      .expect(not(statusNeverRegresses) and lookupValidityPerHub)
}

module byzHubScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import abstractHub.* from "../abstractHub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = byzHub).* from "../protocol"

  /// W16. A Byzantine hub serves a twin of the wallet's transaction at a
  /// height the chain has not reached. The shim checks the txid, which a twin
  /// shares, and nothing else: it serves both.
  run twinAtFalseHeightIsServedTest =
    init
      .then(block)
      .then(submitTo(0, early))
      .then(ask("early"))
      .then(hubReceiveWith(
        lookupMail(1, "early"), INotFound,
        { hub: s.hub, reply: AWire(render(FromIndexer(IFound({ body: Some(earlyTwin), height: 9 })))) },
      ))
      .then(deliverToShim(replyMail(1, WFound({ body: Some(earlyTwin), height: 9 }))))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: earlyTwin, height: 9 }), via: Some(1) }))
      .expect(s.height() == 2 and s.onChain("early") == Absent)
      .expect(wTwinServed and wFalseHeightServed)
      .expect(txidAuthenticity and not(lookupValidityPerHub))
}

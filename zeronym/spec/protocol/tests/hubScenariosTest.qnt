// -*- mode: Bluespec; -*-

/// Scripted runs of the hub specification.
///
/// A run is a sequence of the machine's own steps from `initWith(c)`. Where a
/// run says a property fails, it also says what in the state makes it fail.
/// A control is the same inputs in the same configuration with the one
/// decisive choice made the honest way; in the control the property holds.
///
/// The schedule in every configuration: the chain starts at height 1, a flush
/// is scheduled at heights 3, 6, 9 and 12, and the mining margin is 2 blocks.
module hubScenariosTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import hubMachine.* from "../hubMachine"

  // ------------------------------------------------------------------------
  // Run vocabulary
  // ------------------------------------------------------------------------
  //
  // Each is a `...With` step with the choice an honest hub and a truthful,
  // reachable indexer would make, or a short sequence of such steps. A run
  // that needs another choice uses the `...With` step itself.

  /// What an honest hub does with a submission of `payload` now.
  def honestly(payload: Payload): HubResult =
    hub(h, SubmitHInput({ nonce: 0, payload: payload }))

  /// The decision an honest hub would give `payload` now.
  def decision(payload: Payload): AckKind =
    admission(h, payload)

  /// A submission of `payload` reaches the hub, which acts as it should.
  action deliver(payload: Payload): bool =
    submitWith(payload, honestly(payload))

  /// The hub observes the true height.
  action see = observeWith(height)

  /// The hub of configuration `c`, running at the genesis height.
  run started(c: HubConfig): bool = initWith(c).then(see)

  /// One block arrives and the hub sees it.
  run block = advance.then(see)

  /// `count` blocks arrive, each seen by the hub. No flush may fall due on the
  /// way.
  run blocks(count: int): bool = count.reps(_ => block)

  /// `count` blocks arrive and the hub hears of none.
  run unseenBlocks(count: int): bool = count.reps(_ => advance)

  /// An honest indexer gives `given` on `payload`, with the effect that
  /// verdict has.
  action judge(payload: Payload, given: Verdict): bool =
    verdictWith(payload, given, given == Accepted)

  /// The hub flushes `batch`, and the indexer gives every entry `given`.
  run flush(batch: List[Payload], given: Verdict): bool =
    flushBegin
      .then(batch.length().reps(i => judge(batch[i], given)))
      .then(flushEnd)

  /// What the requeue at the end of the flush in flight would report.
  def requeueReport: HubOutput =
    hub(h, FlushDoneHInput).out

  /// The hub is publishing a scheduled batch for an epoch the chain has not
  /// reached: its cadence clock is ahead of the true height.
  def flushesAheadOfChain: bool =
    h.flush != Idle and h.cadenceEpoch() > height / cfg.params.flushInterval

  /// Whether a node would take `payload` now.
  def nodeWouldTake(payload: Payload): bool =
    verdictsOn(payload).contains((Accepted, true))

  /// What a Byzantine hub answers when it takes `payload` into its queue
  /// whatever the admission rules say.
  def admitting(payload: Payload): HubResult =
    { ...h, queue: h.queue.put(payload, 0) }.toAckOutput(0, Admitted)

  pure def requeued(held: int, droppedExpired: int, droppedExhausted: int): HubOutput =
    RequeuedOutput({ held: held, droppedExpired: droppedExpired, droppedExhausted: droppedExhausted })

  // ------------------------------------------------------------------------
  // Configurations
  // ------------------------------------------------------------------------

  /// Every named init has an initial state. A configuration that does not
  /// meet the assumptions in its guard stops this run.
  run liveInitsTest =
    initTimely
      .then(initFlakyTip)
      .then(initFlakyTipNoSlack)
      .then(initFlakyTipSlowFlight)
      .then(initStaleLag)
      .then(initStaleLagWithSlack)
      .then(initByzHub)
      .then(initByzIndexer)
      .then(initUnknownUpgrade)
      .expect(cfg == unknownUpgrade and h == startingHub(unknownUpgrade.params) and height == GENESIS_HEIGHT)

  // ------------------------------------------------------------------------
  // The schedule under a timely tip
  // ------------------------------------------------------------------------

  /// A supported wallet's transaction, from admission to the mempool: offered
  /// at the first boundary after it arrived, and accepted after a block in
  /// flight.
  run admittedThenPublishedTest =
    started(timely)
      .then(block)
      .then(deliver(early))
      .expect(h.queued() == Set(early) and seen == Set(early) and onTime == Set(early) and owed == Set(early))
      .then(block)
      .then(flushBegin)
      .expect(h.inFlight() == Set(early) and flightStart == 3)
      .expect(wOfferWithExpiry and wConformingFirstOffer)
      .then(advance)
      .expect(height == 4 and not(mayAdvance) and wConformingFirstOfferInFlightABlock and wBlockInFlight)
      .then(judge(early, Accepted))
      .then(flushEnd)
      .expect(onChain == Set(early) and offered == Set(early) and owed == Set() and flightStart == 0)
      .expect(offeredBeforeExpiry and conformingFirstOfferBeforeExpiry and conformingFirstOfferJudgedBeforeExpiry)
      .expect(conformingEveryOfferBeforeExpiry and ackedIsHeldOrSettled)

  /// K6, control. The wallet inputs of K6 under a timely tip. The second
  /// requeue is judged at tip 9, finds that an expiry of 11 does not survive
  /// the flush at 12, and drops the entry: it is never offered past its
  /// expiry.
  run requeueUnderTimelyTipDropsTest =
    started(timely)
      .then(blocks(2))
      .then(flushBegin)
      .then(blocks(2))
      .then(deliver(late))
      .then(block)
      .then(flush([late], Retryable))
      .expect(h.queue == Map(late -> 1) and wRequeued)
      .then(blocks(3))
      .then(flushBegin)
      .expect(flightStart == 9 and conformingEveryOfferBeforeExpiry)
      .then(judge(late, Retryable))
      .expect(requeueReport == requeued(0, 1, 0))
      .then(flushEnd)
      .expect(h.queue == Map())

  // ------------------------------------------------------------------------
  // K5. Acknowledged, then lost
  // ------------------------------------------------------------------------

  /// K5a. The hub crashes after the ack. The queue was in memory.
  run ackedThenCrashedTest =
    started(timely)
      .then(block)
      .then(deliver(early))
      .expect(owed == Set(early) and ackedIsHeldOrSettled)
      .then(crash)
      .expect(h == downHub(timely.params) and onChain == Set() and offered == Set())
      .expect(not(ackedIsHeldOrSettled))

  /// K5b. The final flush of a draining hub finds the indexer unreachable.
  /// The entry was offered and nothing judged it; the hub stops, and the
  /// transaction is held nowhere.
  run ackedThenLostAtDrainTest =
    started(timely)
      .then(block)
      .then(deliver(early))
      .then(drain)
      .then(flush([early], Retryable))
      .expect(h.phase == Stopped and h.queued() == Set() and h.inFlight() == Set() and onChain == Set())
      .expect(offered == Set(early) and owed == Set(early))
      .expect(not(ackedIsHeldOrSettled))

  /// K5c. A requeue gives an acknowledged entry up as expired. No crash and
  /// no shutdown: the flush at 3 comes back unjudged, and an expiry of 5 does
  /// not survive the flush at 6.
  run requeueDropsAckedAsExpiredTest =
    started(timely)
      .then(block)
      .then(deliver(tight))
      .then(block)
      .then(flushBegin)
      .then(judge(tight, Retryable))
      .expect(requeueReport == requeued(0, 1, 0))
      .then(flushEnd)
      .expect(h.phase == Running and h.queue == Map())
      .expect(owed == Set(tight) and not(ackedIsHeldOrSettled))

  // ------------------------------------------------------------------------
  // The unknown upgrade
  // ------------------------------------------------------------------------

  /// A payload the hub cannot parse has no txid and no expiry. The expiry
  /// rule never refuses it and never gives it up, so the attempt bound is the
  /// only limit on its requeue: after the third unjudged flush it is dropped
  /// as exhausted. On the way `tight`, acknowledged, is dropped as expired.
  run requeueAndDropTest =
    started(unknownUpgrade)
      .then(deliver(junk))
      .then(block)
      .then(deliver(tight))
      .then(block)
      .then(flushBegin)
      // A fresh admission while the batch is out.
      .then(deliver(early))
      .expect(h.queued() == Set(early) and h.inFlight() == Set(junk, tight))
      .then(judge(junk, Retryable))
      .then(judge(tight, Retryable))
      .expect(requeueReport == requeued(1, 1, 0))
      .then(flushEnd)
      .expect(h.queue == Map(early -> 0, junk -> 1) and wRequeued)
      .expect(owed.contains(tight) and not(ackedIsHeldOrSettled))
      .then(blocks(3))
      .then(flushBegin)
      .then(judge(early, Accepted))
      .then(judge(junk, Retryable))
      .then(flushEnd)
      .expect(h.queue == Map(junk -> 2))
      // The third failure is one more than the attempts allowed.
      .then(blocks(3))
      .then(flushBegin)
      .then(judge(junk, Retryable))
      .expect(requeueReport == requeued(0, 0, 1))
      .then(flushEnd)
      .expect(h.queue == Map() and owed.contains(junk))
      .expect(offeredBeforeExpiry)

  // ------------------------------------------------------------------------
  // A tip reported behind the chain
  // ------------------------------------------------------------------------

  /// K3. A tight-expiry transaction is admitted against a tip reported one
  /// block back, below a boundary the hub has already flushed. Admission
  /// reasons that the next flush is at 3; it is at 6, after the expiry.
  run tightExpiryAdmittedBehindFlushedBoundaryTest =
    started(flakyTip)
      .then(blocks(2))
      .then(flushBegin)
      .then(observeWith(2))
      .then(deliver(tight))
      .expect(height == 3 and h.tip == Some(2) and h.queued() == Set(tight))
      .then(blocks(3))
      .then(flushBegin)
      .expect(flightStart == 6 and h.inFlight() == Set(tight))
      // Not a supported wallet: the guarantee for those is untouched.
      .expect(not(conforming(tight, cfg.params.minWalletExpiry)))
      .expect(not(offeredBeforeExpiry) and conformingFirstOfferBeforeExpiry)

  /// A supported wallet's transaction, admitted the same way and then flushed
  /// one block late because the tip is reported one block back.
  run regressedTipDelaysFlush(c: HubConfig, payload: Payload): bool =
    started(c)
      .then(blocks(2))
      .then(flushBegin)
      .then(observeWith(2))
      .then(deliver(payload))
      .expect(height == 3 and onTime == Set(payload))
      .then(blocks(2))
      .then(unseenBlocks(2))
      .then(observeWith(6))
      .then(flushBegin)
      .expect(flightStart == 7 and h.inFlight() == Set(payload))
      .expect(conforming(payload, cfg.params.minWalletExpiry))

  /// K3', contrast. The reorg slack leaves the transaction exactly the mining
  /// margin at the offer. One block arrives while the batch is in flight,
  /// which the margin is there to pay for, and the node still accepts it.
  run conformingSurvivesRegressionTest =
    regressedTipDelaysFlush(flakyTip, early)
      .expect(early.expiry == Some(7 + cfg.params.miningMargin))
      .expect(offeredBeforeExpiry and conformingFirstOfferBeforeExpiry)
      .then(advance)
      .expect(height == 8 and not(mayAdvance))
      .expect(wConformingFirstOfferInFlightABlock and conformingFirstOfferJudgedBeforeExpiry)
      .then(judge(early, Accepted))
      .then(flushEnd)
      .expect(onChain == Set(early))

  /// K3'. The same steps with the expiry floor equal to the three-term
  /// budget. The flush is one block late and the transaction misses the
  /// mining margin by that block.
  run conformingMissesMarginWithoutSlackTest =
    regressedTipDelaysFlush(flakyTipNoSlack, orchard("early", 2, 8))
      .expect(not(reorgSlackFits(cfg)))
      .expect(not(conformingFirstOfferBeforeExpiry))

  /// K7. The same steps with the batch in flight for two blocks, as many as
  /// the margin reserves. The offer left the whole margin, and G6b holds. By
  /// the time the node looks, the transaction can no longer be mined: the
  /// budget had already spent the slack, and the flight spent the margin.
  run slowFlightSpendsTheMarginTest =
    regressedTipDelaysFlush(flakyTipSlowFlight, early)
      .then(unseenBlocks(2))
      .expect(height == 9 and early.expiry == Some(9) and not(flightWithinMargin(cfg)))
      .expect(not(nodeWouldTake(early)))
      .expect(conformingFirstOfferBeforeExpiry and not(conformingFirstOfferJudgedBeforeExpiry))
      .then(judge(early, Rejected))

  /// Finding 8. A supported wallet's transaction is admitted on time, and the
  /// hub crashes. It comes back at height 6, adopts that epoch without
  /// flushing, and is then told the tip is 5. A duplicate of the submission
  /// arrives; admission counts on the flush at 6, which will not happen. The
  /// hub is shut down at 8 and its final flush offers the transaction with
  /// one block of margin where two are reserved.
  run crashThenLateDuplicate(c: HubConfig): bool =
    started(c)
      .then(block)
      .then(deliver(early))
      .expect(onTime == Set(early))
      .then(crash)
      .then(unseenBlocks(4))
      .then(restart)
      .then(see)
      .expect(height == 6 and h.lastEpoch == Some(2))
      .then(observeWith(5))
      .then(deliver(early))
      .expect(h.queued() == Set(early))
      .then(see)
      .then(blocks(2))
      .then(drain)
      .then(flushBegin)
      .expect(flightStart == 8 and h.inFlight() == Set(early) and not(marginLeft(early, flightStart)))

  /// The transaction first entered the queue on time, so it counts as timely,
  /// and its first offer misses the margin.
  run crashThenLateDuplicateTest =
    crashThenLateDuplicate(flakyTip)
      .expect(reorgSlackFits(cfg) and onTime == Set(early) and offered == Set())
      .expect(not(conformingFirstOfferBeforeExpiry))
      .then(advance)
      .expect(not(conformingFirstOfferJudgedBeforeExpiry))

  // ------------------------------------------------------------------------
  // A hub that hears nothing for a while
  // ------------------------------------------------------------------------

  /// The hub hears nothing after height 5, one block short of the flush at 6,
  /// while the chain goes on. Its cadence still follows the tip it last saw,
  /// so nothing is flushed until it goes stale at height 8.
  run silenceAcrossBoundary(c: HubConfig, payload: Payload): bool =
    started(c)
      .then(blocks(2))
      .then(flushBegin)
      .then(deliver(payload))
      .expect(height == 3 and onTime == Set(payload))
      .then(blocks(2))
      .then(unseenBlocks(3))
      .expect(height == 8 and h.tip == Some(5) and not(h.isFlushDue()))
      .then(staleWith(8))
      .then(flushBegin)
      .expect(flightStart == 8 and h.inFlight() == Set(payload))
      .expect(conforming(payload, cfg.params.minWalletExpiry))

  /// K4. A supported wallet's transaction, expiring at 9, is offered at 8:
  /// not yet expired, with one block of margin where two are reserved. One
  /// block arrives while the batch is in flight, as the margin allows for,
  /// and the node can no longer take it.
  run silenceAcrossBoundaryMissesMarginTest =
    silenceAcrossBoundary(staleLag, early)
      .expect(not(conformingFirstOfferBeforeExpiry) and not(offeredBeforeExpiry))
      .then(advance)
      .expect(height == 9 and not(mayAdvance) and flightWithinMargin(cfg))
      .expect(not(nodeWouldTake(early)) and not(conformingFirstOfferJudgedBeforeExpiry))
      .then(judge(early, Rejected))

  /// K4, contrast. The same steps with an expiry floor one block higher,
  /// which is the relation `staleSlackFits` asks for. The same late flush
  /// leaves the margin, and after a block in flight the node accepts.
  run sameSilenceWithSlackKeepsMarginTest =
    silenceAcrossBoundary(staleLagWithSlack, orchard("early", 2, 10))
      .expect(staleSlackFits(cfg) and wConformingFirstOffer and conformingFirstOfferBeforeExpiry)
      .then(advance)
      .expect(height == 9 and conformingFirstOfferJudgedBeforeExpiry)
      .then(judge(orchard("early", 2, 10), Accepted))
      .expect(onChain == Set(orchard("early", 2, 10)))

  /// A stale hub's flush finds the indexer unreachable, twice. Requeue judges
  /// the entry at the observed tip, which stopped at 5: the next flush it
  /// knows of is the one at 6, so an expiry of 11 looks safe both times. The
  /// free-running schedule offers it again at 12.
  run requeuedTwiceWhileStale: bool =
    started(staleLag)
      .then(blocks(2))
      .then(flushBegin)
      .then(blocks(2))
      .then(deliver(late))
      .then(unseenBlocks(3))
      .then(staleWith(8))
      .then(flush([late], Retryable))
      .expect(h.queue == Map(late -> 1))
      .then(advance)
      .then(staleWith(9))
      .then(flush([late], Retryable))
      .expect(h.queue == Map(late -> 2) and h.tip == Some(5))
      .then(3.reps(i => advance.then(staleWith(10 + i))))
      .then(flushBegin)
      .expect(flightStart == 12 and late.expiry == Some(11))

  /// K6. The third offer is past the expiry. The first was in time, which is
  /// all G6b covers.
  run requeuedPastExpiryTest =
    requeuedTwiceWhileStale
      .expect(not(conformingEveryOfferBeforeExpiry) and conformingFirstOfferBeforeExpiry)

  /// Finding 9. The third flush comes back unjudged as well, and the entry,
  /// which has an expiry, is dropped as exhausted: the expiry rule, reading a
  /// tip that stopped at 5, still does not give it up.
  run expiringEntryDroppedAsExhaustedTest =
    requeuedTwiceWhileStale
      .then(judge(late, Retryable))
      .expect(isSome(late.expiry) and requeueReport == requeued(0, 0, 1))
      .then(flushEnd)
      .expect(h.queue == Map())

  /// A stale hub's free-running clock reads 6 at true height 5, and the flush
  /// scheduled for 6 runs a block early.
  run freeRunningClockFlushesEarlyTest =
    started(staleLag)
      .then(block)
      .then(deliver(early))
      .then(unseenBlocks(3))
      .then(staleWith(6))
      .then(flushBegin)
      .expect(height == 5 and h.cadence == FreeRunning(6) and h.inFlight() == Set(early))
      .expect(flushesAheadOfChain and conformingFirstOfferBeforeExpiry)

  /// A stale hub refuses submissions until it sees the tip move again.
  run staleHubRefusesTest =
    started(staleLag)
      .then(unseenBlocks(3))
      .then(staleWith(4))
      .expect(wStale and decision(early) == Refused(TipStale))
      .then(deliver(early))
      .expect(h.queued() == Set())
      .then(see)
      .expect(h.phase == Running and h.cadence == Tracking)
      .then(deliver(early))
      .expect(h.queued() == Set(early))

  /// Finding 2; the slack does not cover it. A stale hub's free-running clock
  /// reads 6 at true height 4, and the flush scheduled for 6 runs then, with
  /// nothing to publish. When the hub sees the tip again the chain is at 5,
  /// and admission, which knows the tip and not the schedule's history,
  /// counts on the flush at 6. That flush has already happened. The
  /// transaction waits for the one at 9, and a second, shorter silence makes
  /// that one late too: it is offered at 11 with expiry 12, one block of
  /// margin where two are reserved, and a block in flight uses that up.
  ///
  /// Neither half is enough alone at these numbers. Without the early flush
  /// the transaction goes out at 6; without the second silence, at 9.
  run earlyFlushSpendsTheNextEpochTest =
    val lateAtFloor = orchard("late", 4, 12)
    started(staleLagWithSlack)
      .then(unseenBlocks(3))
      .then(staleWith(6))
      .then(flushBegin)
      .expect(height == 4 and h.lastEpoch == Some(2))
      .then(block)
      .expect(h.cadence == Tracking and h.tip == Some(5))
      .then(deliver(lateAtFloor))
      .expect(onTime == Set(lateAtFloor))
      .then(blocks(3))
      // Height 8. The boundary at 6 has passed and nothing was flushed.
      .expect(offered == Set() and h.queued() == Set(lateAtFloor))
      .then(unseenBlocks(3))
      .then(staleWith(11))
      .then(flushBegin)
      .expect(conforming(lateAtFloor, cfg.params.minWalletExpiry) and staleSlackFits(cfg))
      .expect(flightStart == 11 and not(conformingFirstOfferBeforeExpiry))
      .then(advance)
      .expect(height == 12 and not(nodeWouldTake(lateAtFloor)))
      .expect(not(conformingFirstOfferJudgedBeforeExpiry))
      .then(judge(lateAtFloor, Rejected))

  // ------------------------------------------------------------------------
  // A Byzantine hub
  // ------------------------------------------------------------------------

  /// G6a needs the hub. It admits a transaction the expiry rule refuses: one
  /// expiring at 5, taken at tip 3, when the next flush is at 6.
  run hubAdmitsPastExpiryRuleTest =
    started(byzHub)
      .then(blocks(2))
      .then(flushBegin)
      .expect(decision(tight) == Refused(ExpiryTooTight))
      .then(submitWith(tight, admitting(tight)))
      .then(blocks(3))
      .then(flushBegin)
      .expect(flightStart == 6 and h.inFlight() == Set(tight))
      .expect(not(offeredBeforeExpiry))

  run hubAdmitsPastExpiryRuleControlTest =
    started(byzHub)
      .then(blocks(2))
      .then(flushBegin)
      .then(deliver(tight))
      .expect(h.queued() == Set())
      .then(blocks(3))
      .then(flushBegin)
      .expect(h.inFlight() == Set() and offeredBeforeExpiry)

  /// G6b and G6c need the hub. A supported wallet's transaction is refused by
  /// an honest hub only for reasons that are not about the transaction. This
  /// hub takes one while it has not yet seen a tip, when an honest hub
  /// refuses everything. It first sees the chain at height 7, adopts that
  /// epoch without flushing, and offers the transaction at 9, its expiry,
  /// when no node can take it.
  run hubAdmitsBeforeFirstTipTest =
    initWith(byzHub)
      .then(unseenBlocks(2))
      .expect(decision(early) == Refused(TipStale))
      .then(submitWith(early, admitting(early)))
      .expect(h.phase == Starting and height == 3 and onTime == Set(early))
      .then(unseenBlocks(4))
      .then(see)
      .then(blocks(2))
      .then(flushBegin)
      .expect(conforming(early, cfg.params.minWalletExpiry) and flightStart == 9)
      .expect(not(conformingFirstOfferBeforeExpiry))
      .expect(not(nodeWouldTake(early)) and not(conformingFirstOfferJudgedBeforeExpiry))
      .then(judge(early, Rejected))

  run hubAdmitsBeforeFirstTipControlTest =
    initWith(byzHub)
      .then(unseenBlocks(2))
      .then(deliver(early))
      .expect(h.queued() == Set())
      .then(unseenBlocks(4))
      .then(see)
      .then(blocks(2))
      .then(flushBegin)
      .expect(h.inFlight() == Set())
      .expect(conformingFirstOfferBeforeExpiry and conformingFirstOfferJudgedBeforeExpiry)

  /// A3 needs the hub. Once its drain has begun, it takes a submission into
  /// the queue: draining before and after, nothing in flight, and the queue
  /// grown by a payload no flush handed back.
  run hubAdmitsWhileDrainingTest =
    started(byzHub)
      .then(block)
      .then(deliver(early))
      .then(drain)
      .expect(h.phase == Draining and h.queued() == Set(early) and h.inFlight() == Set())
      .expect(decision(tight) == Refused(HubDraining))
      .then(submitWith(tight, admitting(tight)))
      .expect(h.phase == Draining and h.queued() == Set(early, tight))

  run hubAdmitsWhileDrainingControlTest =
    started(byzHub)
      .then(block)
      .then(deliver(early))
      .then(drain)
      .then(deliver(tight))
      .expect(h.phase == Draining and h.queued() == Set(early))

  // ------------------------------------------------------------------------
  // A Byzantine indexer
  // ------------------------------------------------------------------------

  /// Premature flush. The indexer reports tip 3 at true height 2; the hub
  /// believes the boundary has come and publishes what it holds. One lying
  /// endpoint is enough for this: the tip is the maximum over endpoints. The
  /// transaction is published early, which costs it nothing.
  run tipAheadOfChainFlushesEarlyTest =
    started(byzIndexer)
      .then(block)
      .then(deliver(early))
      .then(observeWith(3))
      .then(flushBegin)
      .expect(height == 2 and h.tip == Some(3) and h.inFlight() == Set(early))
      .expect(flushesAheadOfChain)
      .expect(offeredBeforeExpiry and conformingFirstOfferBeforeExpiry)

  /// The hub asks for the tip at every block. The indexer goes on answering 2
  /// while `silence` more blocks arrive, and then answers truthfully. The
  /// flush scheduled for 3 runs then.
  run tipWithheld(payload: Payload, silence: int): bool =
    started(byzIndexer)
      .then(block)
      .then(deliver(payload))
      .then((silence - 1).reps(_ => advance.then(observeWith(2))))
      .then(advance)
      .expect(height == 2 + silence and h.tip == Some(2) and not(mayAdvance))
      .then(see)
      .then(flushBegin)
      .expect(flightStart == 2 + silence and h.inFlight() == Set(payload))

  /// The same polls, answered truthfully: the flush runs at 3.
  run tipReported(payload: Payload): bool =
    started(byzIndexer)
      .then(block)
      .then(deliver(payload))
      .then(block)
      .then(flushBegin)
      .expect(flightStart == 3 and h.inFlight() == Set(payload))

  /// G6a needs the indexer. With the tip withheld until height 5, a
  /// transaction expiring at 5 is offered with no margin left.
  run indexerWithholdsTipTest =
    tipWithheld(tight, 3)
      .expect(not(offeredBeforeExpiry))

  run indexerWithholdsTipControlTest =
    tipReported(tight)
      .expect(offeredBeforeExpiry)
      .then(judge(tight, Accepted))
      .then(flushEnd)
      .then(blocks(2))
      .expect(height == 5 and offeredBeforeExpiry)

  /// G6b and G6c need the indexer. The same lie, kept up to height 8, does it
  /// to a supported wallet's transaction: it is offered at 8 with expiry 9,
  /// and after one block in flight the node cannot take it.
  run indexerWithholdsTipFromConformingTest =
    tipWithheld(early, 6)
      .expect(conforming(early, cfg.params.minWalletExpiry) and onTime == Set(early))
      .expect(not(conformingFirstOfferBeforeExpiry))
      .then(advance)
      .expect(not(nodeWouldTake(early)) and not(conformingFirstOfferJudgedBeforeExpiry))
      .then(judge(early, Rejected))

  /// Offered at 3, and accepted after a block in flight.
  run indexerWithholdsTipFromConformingControlTest =
    tipReported(early)
      .expect(conformingFirstOfferBeforeExpiry)
      .then(advance)
      .expect(conformingFirstOfferJudgedBeforeExpiry)
      .then(judge(early, Accepted))
      .then(flushEnd)
      .expect(onChain == Set(early))
}

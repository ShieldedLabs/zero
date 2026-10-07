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

  pure def floorOf(c: HubConfig): int = c.params.minWalletExpiry

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
      .expect(cfg == staleLagWithSlack and h == startingHub(staleLagWithSlack.params) and height == GENESIS_HEIGHT)

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
      .expect(requeueReport == RequeuedOutput({ held: 0, droppedExpired: 1, droppedExhausted: 0 }))
      .then(flushEnd)
      .expect(h.queue == Map())
}

// -*- mode: Bluespec; -*-

/// The hub function, checked on a small schedule: a flush every 3 blocks, a
/// mining margin of 1, at most 2 requeues.
module hubTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import hub.* from "../hub"
  import wire.* from "../wire"
  import abstractHub.* from "../abstractHub"

  pure val PARAMS: HubParams = {
    flushInterval: 3,
    miningMargin: 1,
    deliveryLag: 1,
    minWalletExpiry: 6,
    reorgAllowance: 1,
    maxAttempts: 2,
  }

  pure def orchard(id: str, expiry: Option[Height]): Payload =
    { id: id, txid: Some(id), created: 1, expiry: expiry, class: OrchardTouching, oversize: false }

  pure val pA = orchard("a", Some(9))
  pure val pB = orchard("b", Some(9))
  pure val pC = orchard("c", None)
  pure val pTight = orchard("tight", Some(6))
  pure val pJunk = { id: "junk", txid: None, created: 1, expiry: None, class: Unparseable, oversize: false }

  pure val PAYLOADS = Set(pA, pB, pC, pTight, pJunk)
  pure val HEIGHTS = Set(0, 4)
  pure val REFUSALS = Set(TipStale, HubDraining, ExpiryTooTight)

  /// The state after `input`.
  pure def after(state: HubState, input: HubInput): HubState =
    hub(state, input).state

  /// The output of `input`.
  pure def outputOf(state: HubState, input: HubInput): HubOutput =
    hub(state, input).out

  pure def isError(output: HubOutput): bool =
    match output {
      | HubErrorOutput(_) => true
      | _ => false
    }

  pure def submit(payload: Payload): HubInput =
    SubmitHInput({ nonce: 0, payload: payload })

  pure def verdict(payload: Payload, given: Verdict): HubInput =
    VerdictHInput({ payload: payload, verdict: given })

  pure val down = downHub(PARAMS)
  pure val starting = startingHub(PARAMS)
  pure def running(tip: Height): HubState = starting.after(TipHInput(tip))

  /// Running at tip 5 with `pA` and `pJunk` queued.
  pure val holding = running(5).after(submit(pA)).after(submit(pJunk))
  /// The same hub one block later, with its flush in flight.
  pure val flushing = holding.after(TipHInput(6)).after(FlushDueHInput)
  pure val stale = running(5).after(StaleHInput(8))
  pure val draining = holding.after(DrainHInput)
  pure val stopped = draining.after(FlushDueHInput).after(verdict(pA, Accepted))
    .after(verdict(pJunk, Rejected)).after(FlushDoneHInput)

  pure val STATES = Set(down, starting, running(2), running(5), holding, flushing, stale, draining, stopped,
    stale.after(DrainHInput), flushing.after(verdict(pA, Retryable)))

  pure val INPUTS: Set[HubInput] =
    PAYLOADS.map(payload => submit(payload))
      .union(Set(
        LookupHInput({ nonce: 1, txid: "a", answer: INotFound }),
        LookupHInput({ nonce: 1, txid: "zz", answer: IFound({ body: Some(pB), height: 4 }) }),
        TipHInput(0), TipHInput(4), TipHInput(5), TipHInput(6), TipHInput(9),
        StaleHInput(4), StaleHInput(8), StaleHInput(9),
        FlushDueHInput, FlushDoneHInput, DrainHInput, CrashHInput, RestartHInput,
      ))
      .union(tuples(Set(pA, pB, pJunk), Set(Accepted, AlreadyKnown, Rejected, Retryable))
        .map(((payload, given)) => verdict(payload, given)))

  // ------------------------------------------------------------------------
  // Admission
  // ------------------------------------------------------------------------

  /// F7. Under the startup budget, a conforming payload that arrives within
  /// the delivery lag passes the expiry check. The lemma is about admission at
  /// one tip. It does not say at what height the flush later happens.
  run conformingTimelyPayloadIsAdmissibleTest = all {
    assert(scheduleFitsBudget(PARAMS)),
    assert(tuples(0.to(9), 0.to(PARAMS.deliveryLag), 0.to(2)).forall(((created, lag, extra)) =>
      survivesNextFlush(
        Some(created + PARAMS.minWalletExpiry + extra), created + lag,
        PARAMS.flushInterval, PARAMS.miningMargin))),
    // The budget is what makes it true: one block less and a payload built
    // one block before a boundary, arriving on it, is refused.
    assert(not(scheduleFitsBudget({ ...PARAMS, minWalletExpiry: 4 }))),
    assert(not(survivesNextFlush(Some(2 + 4), 2 + 1, PARAMS.flushInterval, PARAMS.miningMargin))),
  }

  run nextFlushHeightTest = all {
    assert(nextFlushHeight(0, 3) == 3),
    assert(nextFlushHeight(2, 3) == 3),
    // Strictly after: a tip on a boundary waits a whole interval.
    assert(nextFlushHeight(3, 3) == 6),
    assert(survivesNextFlush(None, 100, 3, 1)),
    assert(survivesNextFlush(Some(7), 5, 3, 1)),
    assert(not(survivesNextFlush(Some(6), 5, 3, 1))),
  }

  /// F8. Each refusal, and the order the checks are made in.
  run admissionDecisionTableTest = all {
    // No tip yet, or a stale one: nothing else is looked at.
    assert(PAYLOADS.forall(payload => admission(starting, payload) == Refused(TipStale))),
    assert(PAYLOADS.forall(payload => admission(stale, payload) == Refused(TipStale))),
    assert(PAYLOADS.forall(payload => admission(stale.after(DrainHInput), payload) == Refused(TipStale))),
    // Draining comes before anything about the payload.
    assert(PAYLOADS.forall(payload => admission(draining, payload) == Refused(HubDraining))),
    // Expiry before the queue is looked at. At tip 5 the next flush is at 6.
    assert(admission(running(5), pTight) == Refused(ExpiryTooTight)),
    assert(admission(holding, pTight) == Refused(ExpiryTooTight)),
    assert(admission(running(2), pTight) == Admitted),
    // A duplicate is recognised; anything else that passes is admitted.
    assert(admission(holding, pA) == Duplicate),
    assert(admission(holding, pB) == Admitted),
    assert(admission(running(5), pA) == Admitted),
    // An unparseable payload has no expiry and is admitted like any other.
    assert(admission(running(5), pJunk) == Admitted),
    // Every refusal is produced, and a refusal changes nothing.
    assert(REFUSALS.forall(refusal =>
      tuples(STATES, PAYLOADS).exists(((state, payload)) =>
        state.isServing() and outputOf(state, submit(payload)) == AckOutput({ nonce: 0, kind: Refused(refusal) })))),
    assert(tuples(STATES, PAYLOADS).forall(((state, payload)) =>
      match outputOf(state, submit(payload)) {
        | AckOutput(ack) => ack.kind == Admitted or state.after(submit(payload)) == state
        | _ => true
      })),
  }

  /// F13. An accepted ack is given only for a payload the hub then holds.
  run ackImpliesQueuedTest = all {
    assert(tuples(STATES, PAYLOADS).forall(((state, payload)) =>
      match outputOf(state, submit(payload)) {
        | AckOutput(ack) => isAccepted(ack.kind) implies state.after(submit(payload)).queued().contains(payload)
        | _ => true
      })),
    assert(running(5).after(submit(pA)).queue == Map(pA -> 0)),
  }

  // ------------------------------------------------------------------------
  // Lookup
  // ------------------------------------------------------------------------

  run lookupTest = all {
    // A queue hit, whatever the indexer would have said.
    assert(outputOf(holding, LookupHInput({ nonce: 7, txid: "a", answer: IFound({ body: Some(pA), height: 4 }) }))
      == LookupReplyOutput({ nonce: 7, outcome: QueueHit })),
    // A miss forwards the indexer's answer as it came.
    assert(Set(INotFound, IUnavailable, IFound({ body: Some(pB), height: 4 }), IFound({ body: None, height: 0 }))
      .forall(answer =>
        outputOf(holding, LookupHInput({ nonce: 7, txid: "b", answer: answer }))
          == LookupReplyOutput({ nonce: 7, outcome: FromIndexer(answer) }))),
    // A queued payload that does not parse is never hit.
    assert(holding.queued().contains(pJunk) and not(holding.isQueuedTxid("junk"))),
    // Once the flush has taken the queue, the same lookup misses.
    assert(outputOf(flushing, LookupHInput({ nonce: 7, txid: "a", answer: INotFound }))
      == LookupReplyOutput({ nonce: 7, outcome: FromIndexer(INotFound) })),
    assert(STATES.forall(state =>
      after(state, LookupHInput({ nonce: 7, txid: "a", answer: INotFound })) == state)),
  }

  // ------------------------------------------------------------------------
  // Tip
  // ------------------------------------------------------------------------

  /// F14. The tip rule.
  run tipRuleTest = all {
    // The first observation is adopted with its epoch, and nothing is due.
    assert(running(5).tip == Some(5) and running(5).phase == Running),
    assert(running(5).lastEpoch == Some(1) and not(running(5).isFlushDue())),
    // Forward: followed.
    assert(running(5).after(TipHInput(6)).tip == Some(6)),
    // Back within the allowance: followed.
    assert(running(5).after(TipHInput(4)).tip == Some(4)),
    // Back beyond it: ignored.
    assert(running(5).after(TipHInput(3)) == running(5)),
    assert(outputOf(running(5), TipHInput(3)) == NoHubOutput),
    // A forward move ends staleness; a move back does not.
    assert(stale.after(TipHInput(6)).phase == Running and stale.after(TipHInput(6)).cadence == Tracking),
    assert(stale.after(TipHInput(4)).phase == Stale and stale.after(TipHInput(4)).cadence == FreeRunning(8)),
    // Never during a flush, and never without a cadence loop.
    assert(isError(outputOf(flushing, TipHInput(7)))),
    assert(isError(outputOf(down, TipHInput(7)))),
    assert(isError(outputOf(draining, TipHInput(7)))),
  }

  run staleCadenceTest = all {
    assert(stale.phase == Stale and stale.cadence == FreeRunning(8)),
    // The observed tip is untouched; only the schedule moves.
    assert(stale.observedTip() == 5 and stale.cadenceHeight() == 8 and stale.cadenceEpoch() == 2),
    assert(stale.isFlushDue()),
    assert(stale.after(StaleHInput(9)).cadence == FreeRunning(9)),
    assert(isError(outputOf(stale, StaleHInput(7)))),
    assert(isError(outputOf(running(5), StaleHInput(4)))),
    assert(isError(outputOf(starting, StaleHInput(4)))),
    assert(isError(outputOf(flushing, StaleHInput(9)))),
  }

  // ------------------------------------------------------------------------
  // Flush and requeue
  // ------------------------------------------------------------------------

  run flushCycleTest = all {
    assert(not(holding.isFlushDue()) and isError(outputOf(holding, FlushDueHInput))),
    assert(holding.after(TipHInput(6)).isFlushDue()),
    // The whole queue moves out at once.
    assert(outputOf(holding.after(TipHInput(6)), FlushDueHInput) == BroadcastOutput(Set(pA, pJunk))),
    assert(flushing.queue == Map() and flushing.inFlight() == Set(pA, pJunk)),
    // Published, already known and rejected entries leave.
    assert(Set(Accepted, AlreadyKnown, Rejected).forall(given =>
      flushing.after(verdict(pA, given)).inFlight() == Set(pJunk))),
    // A retryable entry stays out until the flush ends.
    assert(flushing.after(verdict(pA, Retryable)).inFlight() == Set(pA, pJunk)),
    assert(isError(outputOf(flushing, verdict(pB, Accepted)))),
    assert(isError(outputOf(flushing.after(verdict(pA, Accepted)), verdict(pA, Accepted)))),
    assert(isError(outputOf(holding, verdict(pA, Accepted)))),
    // The flush cannot end while a verdict is outstanding.
    assert(isError(outputOf(flushing, FlushDoneHInput))),
    assert(isError(outputOf(holding, FlushDoneHInput))),
    // An empty queue is still a flush event: the epoch is recorded.
    assert(running(5).after(TipHInput(6)).after(FlushDueHInput).lastEpoch == Some(2)),
    assert(outputOf(running(5).after(TipHInput(6)), FlushDueHInput) == NoHubOutput),
  }

  /// A flush at tip 5 that left four entries unjudged, with three resident.
  pure val pExpired = orchard("expired", Some(6))
  pure val pResident = orchard("resident", Some(9))
  pure val pWornOut = { ...pJunk, id: "worn-out" }
  pure val pWornOutTight = { ...pExpired, id: "worn-out-tight" }
  pure val returning: HubState = {
    ...running(5),
    queue: Map(pResident -> 0, pB -> 0, pC -> 0),
    flush: Broadcasting({
      batch: Map(),
      unplaced: Map(pA -> 0, pExpired -> 0, pWornOut -> 2, pResident -> 1, pJunk -> 1, pWornOutTight -> 2),
      final: false,
    }),
  }

  /// F9. Requeue, entry by entry, and the counts it reports.
  run requeueTest = all {
    assert(outputOf(returning, FlushDoneHInput)
      == RequeuedOutput({ held: 2, droppedExpired: 2, droppedExhausted: 1 })),
    // Held entries come back with one more attempt. The resident copy of the
    // same bytes wins and keeps its own count.
    assert(returning.after(FlushDoneHInput).queue
      == Map(pResident -> 0, pB -> 0, pC -> 0, pA -> 1, pJunk -> 2)),
    assert(returning.after(FlushDoneHInput).flush == Idle),
    assert(returning.after(FlushDoneHInput).lastEpoch == Some(1)),
    // Expiry is judged at the observed tip, not at the cadence height.
    assert({ ...returning, phase: Stale, cadence: FreeRunning(11) }.after(FlushDoneHInput).queued().contains(pA)),
    assert({ ...returning, phase: Stale, cadence: FreeRunning(11) }.after(FlushDoneHInput).lastEpoch == Some(3)),
    assert(not({ ...returning, tip: Some(8) }.after(FlushDoneHInput).queued().contains(pA))),
  }

  run drainTest = all {
    assert(draining.phase == Draining and draining.isFlushDue()),
    assert(isError(outputOf(starting, DrainHInput))),
    assert(isError(outputOf(draining, DrainHInput))),
    assert(stale.after(DrainHInput).phase == Draining),
    // The final flush publishes what is held; what it cannot place is lost.
    assert(stopped.phase == Stopped and stopped.queue == Map()),
    assert(draining.after(FlushDueHInput).after(verdict(pA, Retryable)).after(verdict(pJunk, Retryable))
      .after(FlushDoneHInput).queue == Map()),
    assert(outputOf(
      draining.after(FlushDueHInput).after(verdict(pA, Retryable)).after(verdict(pJunk, Retryable)),
      FlushDoneHInput) == RequeuedOutput({ held: 2, droppedExpired: 0, droppedExhausted: 0 })),
    // With nothing held, the final flush stops the hub at once.
    assert(running(5).after(DrainHInput).after(FlushDueHInput).phase == Stopped),
    // A flush already in flight finishes first, and the final one follows.
    assert(flushing.after(DrainHInput).after(verdict(pA, Accepted)).after(verdict(pJunk, Retryable))
      .after(FlushDoneHInput).phase == Draining),
    assert(flushing.after(DrainHInput).after(verdict(pA, Accepted)).after(verdict(pJunk, Retryable))
      .after(FlushDoneHInput).isFlushDue()),
  }

  run crashAndRestartTest = all {
    assert(STATES.exclude(Set(down)).forall(state => state.after(CrashHInput) == down)),
    assert(isError(outputOf(down, CrashHInput))),
    assert(down.after(RestartHInput) == starting),
    assert(STATES.exclude(Set(down)).forall(state => isError(outputOf(state, RestartHInput)))),
    // A hub that is not serving answers nothing.
    assert(Set(down, stopped).forall(state =>
      isError(outputOf(state, submit(pA)))
        and isError(outputOf(state, LookupHInput({ nonce: 1, txid: "a", answer: INotFound }))))),
  }

  // ------------------------------------------------------------------------
  // Totality and the Byzantine relation
  // ------------------------------------------------------------------------

  /// F11. The function answers every input in every state, and an input that
  /// is invalid in a state leaves that state unchanged.
  run totalityTest =
    assert(tuples(STATES, INPUTS).forall(((state, input)) =>
      val result = hub(state, input)
      and {
        result.state.params == state.params,
        isError(result.out) implies result.state == state,
      }))

  /// F12. The Byzantine relation contains the honest transition.
  run byzantineContainsHonestTest =
    assert(tuples(STATES, INPUTS).forall(((state, input)) =>
      byzHubResults(state, input, PAYLOADS, HEIGHTS).contains(hub(state, input))))

  /// F13, companion. A Byzantine hub can accept a payload it does not hold,
  /// can hold one admission would refuse, and can answer a lookup with a
  /// queued body.
  run byzantineHubTest = all {
    assert(byzHubResults(running(5), submit(pA), PAYLOADS, HEIGHTS).exists(result =>
      result.out == AckOutput({ nonce: 0, kind: Admitted }) and not(result.state.queued().contains(pA)))),
    assert(byzHubResults(running(5), submit(pTight), PAYLOADS, HEIGHTS).exists(result =>
      result.state.queued().contains(pTight))),
    assert(byzHubResults(holding, LookupHInput({ nonce: 1, txid: "a", answer: INotFound }), PAYLOADS, HEIGHTS)
      .contains(holding.toLookupReplyOutput(1, FromIndexer(IFound({ body: Some(pA), height: 0 }))))),
    // It still cannot act while it is not running.
    assert(byzHubResults(down, submit(pA), PAYLOADS, HEIGHTS) == Set(hub(down, submit(pA)))),
  }

  // ------------------------------------------------------------------------
  // Every reachable state
  // ------------------------------------------------------------------------
  //
  // `REACH` is every hub state reachable from a starting hub under
  // `REACH_INPUTS`, honest or through the Byzantine submit relation. The
  // properties below are exhaustive over it, for these parameters: two
  // payloads, one with no txid and no expiry; tips 0, 4, 5, 6 and 9; free-run
  // estimates 4, 8 and 9; every verdict on either payload.

  pure val REACH_PAYLOADS = Set(pA, pJunk)
  pure val REACH_INPUTS: Set[HubInput] =
    REACH_PAYLOADS.map(payload => submit(payload))
      .union(Set(
        TipHInput(0), TipHInput(4), TipHInput(5), TipHInput(6), TipHInput(9),
        StaleHInput(4), StaleHInput(8), StaleHInput(9),
        FlushDueHInput, FlushDoneHInput, DrainHInput, CrashHInput, RestartHInput,
      ))
      .union(tuples(REACH_PAYLOADS, Set(Accepted, AlreadyKnown, Rejected, Retryable))
        .map(((payload, given)) => verdict(payload, given)))

  /// The states a Byzantine hub may move to on a submission.
  pure def byzantineSteps(state: HubState): Set[HubState] =
    REACH_PAYLOADS.map(payload => byzHubResults(state, submit(payload), REACH_PAYLOADS, HEIGHTS)
      .map(result => result.state)).flatten()

  pure def successors(state: HubState): Set[HubState] =
    REACH_INPUTS.map(input => hub(state, input).state).union(byzantineSteps(state))

  /// The closure, breadth first. The bound on rounds is a budget, not a
  /// depth bound: `reachTest` fails unless the result is closed.
  pure val REACH: Set[HubState] =
    1.to(60).fold({ reached: Set(starting), frontier: Set(starting) }, (acc, _) =>
      val fresh = acc.frontier.map(state => successors(state)).flatten().exclude(acc.reached)
      { reached: acc.reached.union(fresh), frontier: fresh }).reached

  /// `REACH` is a fixpoint: no input, honest or Byzantine, leaves it.
  run reachTest = assert(REACH.forall(state => successors(state).subseteq(REACH)))

  /// A2. An entry leaves the queue only into a flush, or because the hub went
  /// down or exited after its final flush. Nothing evicts it, whatever the
  /// hub's role: the Byzantine submit relation only ever adds to the queue.
  /// The exit matters only to a Byzantine hub, which can admit while
  /// draining; an honest one has nothing queued by then.
  run neverEvictTest =
    assert(REACH.forall(state =>
      successors(state).forall(stepped =>
        stepped.phase == Down or stepped.phase == Stopped
          or state.queued().subseteq(stepped.queued().union(stepped.inFlight())))))

  /// A3. A draining honest hub admits nothing: its queue gains only what a
  /// flush hands back. A Byzantine hub is not bound by it
  /// (`hubAdmitsWhileDrainingTest`).
  run drainIsFinalTest =
    assert(REACH.forall(state =>
      state.phase == Draining implies REACH_INPUTS.forall(input =>
        hub(state, input).state.queued().subseteq(state.queued().union(state.inFlight())))))

  // ------------------------------------------------------------------------
  // The abstraction lemma
  // ------------------------------------------------------------------------

  pure def project(state: HubState): AHub =
    { queue: state.queued(), held: state.inFlight() }

  pure def requestOf(input: HubInput): Option[ARequest] =
    match input {
      | SubmitHInput(submitted) => Some(ASubmit(submitted.payload))
      | LookupHInput(lookup) => Some(ALookup({ txid: lookup.txid, answer: lookup.answer }))
      | _ => None
    }

  pure def replyOf(output: HubOutput): Option[AReply] =
    match output {
      | AckOutput(ack) => Some(AAck(renderAck(ack.kind)))
      | LookupReplyOutput(reply) => Some(AWire(render(reply.outcome)))
      | _ => None
    }

  pure val LOOKUPS: Set[HubInput] =
    Set(INotFound, IUnavailable, IFound({ body: Some(pA), height: 4 }))
      .map(answer => LookupHInput({ nonce: 1, txid: "a", answer: answer }))
      .union(Set(LookupHInput({ nonce: 1, txid: "zz", answer: IFound({ body: Some(pB), height: 4 }) })))

  /// An error changes nothing. A request is one of `answers`; anything else
  /// is an internal move with no reply.
  pure def isAbstracted(state: HubState, input: HubInput, result: HubResult, answers: (AHub, ARequest) => Set[AAnswer]): bool =
    val before = state.project()
    val after = result.state.project()
    if (result.out.isError()) result.state == state
    else match input.requestOf() {
      | Some(request) =>
        match result.out.replyOf() {
          | Some(reply) => answers(before, request).contains({ hub: after, reply: reply })
          | None => false
        }
      | None => result.out.replyOf() == None and isInternal(before, after)
    }

  /// Every step of the real hub from a reachable state is a step of the
  /// abstract one, honest against honest and Byzantine against Byzantine.
  run abstractionTest =
    assert(REACH.forall(state =>
      REACH_INPUTS.union(LOOKUPS).forall(input =>
        isAbstracted(state, input, hub(state, input), honestAnswers))))

  run byzantineAbstractionTest =
    assert(REACH.forall(state =>
      REACH_PAYLOADS.map(payload => submit(payload)).union(LOOKUPS).forall(input =>
        byzHubResults(state, input, PAYLOADS, HEIGHTS).forall(result =>
          isAbstracted(state, input, result, (h, request) => byzantineAnswers(h, request, PAYLOADS, HEIGHTS))))))

  /// Each abstract move has a concrete step that projects onto it.
  pure def realises(state: HubState, input: HubInput, move: AHub, reply: Option[AReply]): bool =
    val result = hub(state, input)
    and { not(result.out.isError()), result.state.project() == move, result.out.replyOf() == reply }

  run realisesTest = all {
    // accept, refuse
    assert(realises(running(5), submit(pA), { queue: Set(pA), held: Set() }, Some(AAck(WAccepted)))),
    assert(realises(draining, submit(pB), draining.project(), Some(AAck(WRefused(WQueueFull))))),
    // take, settle, a retryable verdict that moves nothing
    assert(realises(holding.after(TipHInput(6)), FlushDueHInput, holding.project().take(), None)),
    assert(realises(flushing, verdict(pA, Accepted), flushing.project().settle(pA), None)),
    assert(realises(flushing, verdict(pA, Retryable), flushing.project(), None)),
    // give back what is kept: `pJunk` is held, `pA` was settled
    val ending = flushing.after(verdict(pA, Accepted)).after(verdict(pJunk, Retryable))
    assert(realises(ending, FlushDoneHInput, ending.project().giveBack(Set(pJunk)), None)),
    // lose
    assert(realises(holding, CrashHInput, emptyAHub, None)),
  }
}

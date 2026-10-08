// -*- mode: Bluespec; -*-

/// The hub: it admits diverted transactions into a queue held in memory,
/// answers lookups, and publishes the whole queue at once on a block cadence.
///
/// The hub is one total function, `hub(state, input)`, from its state and one
/// input to its next state and one output. Each input is one thing that can
/// happen to a hub (a frame arrives, a tip is observed, a flush becomes due, a
/// verdict comes back) and corresponds to one seam in the implementation. An
/// input that makes no sense in the current state returns `HubErrorOutput` and
/// leaves the state alone.
///
/// `byzHubResults` is the wider relation a Byzantine hub draws from.
module hub {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"

  // ------------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------------

  /// The schedule a hub is started with.
  ///
  /// `deliveryLag` and `minWalletExpiry` are not read by any transition. They
  /// are the wallet-side budget the schedule is validated against at startup
  /// (`scheduleFitsBudget`), and the properties read them from here.
  type HubParams = {
    flushInterval: int,     // blocks between scheduled flushes
    miningMargin: int,      // blocks a published transaction needs to be mined
    deliveryLag: int,       // blocks a submission may take to arrive
    minWalletExpiry: int,   // the smallest expiry delta a supported wallet sets
    reorgAllowance: int,    // how far back a tip report is followed
    maxAttempts: int,       // requeues an entry is allowed
  }

  /// The startup check: a transaction that takes `deliveryLag` blocks to
  /// arrive, waits a full interval and needs `miningMargin` blocks to be mined
  /// still fits inside the smallest supported expiry.
  pure def scheduleFitsBudget(params: HubParams): bool =
    params.flushInterval + params.miningMargin + params.deliveryLag <= params.minWalletExpiry

  /// Where the process is in its life. `Starting` has not seen a tip yet;
  /// `Stale` has seen no forward tip progress for the staleness window;
  /// `Draining` has had its shutdown signal and owes one final flush;
  /// `Stopped` has done it.
  type Phase = Down | Starting | Running | Stale | Draining | Stopped

  /// The clock the flush schedule runs on. It follows the observed tip until
  /// the hub is stale, and then free-runs from an estimate of elapsed time.
  /// Admission and requeue never use it: they read the observed tip.
  type Cadence = Tracking | FreeRunning(Height)

  /// The flush cycle. While broadcasting, `batch` holds the entries still
  /// waiting for a verdict and `unplaced` those nothing judged; both map a
  /// payload to the requeues it has had. `final` marks the shutdown flush.
  type Flush =
    | Idle
    | Broadcasting({ batch: Payload -> int, unplaced: Payload -> int, final: bool })

  /// - `tip`: the last tip observed, absent until the first observation.
  /// - `queue`: the admitted entries, keyed by their bytes, each with the
  ///   number of requeues it has had.
  /// - `lastEpoch`: the flush epoch last acted on, absent until adopted.
  type HubState = {
    params: HubParams,
    phase: Phase,
    tip: Option[Height],
    cadence: Cadence,
    queue: Payload -> int,
    flush: Flush,
    lastEpoch: Option[int],
  }

  /// A hub that is not running. It holds nothing: the queue lives in memory.
  pure def downHub(params: HubParams): HubState = {
    params: params,
    phase: Down,
    tip: None,
    cadence: Tracking,
    queue: Map(),
    flush: Idle,
    lastEpoch: None,
  }

  /// A hub that has just started and has not yet seen a tip.
  pure def startingHub(params: HubParams): HubState =
    { ...downHub(params), phase: Starting }

  // ------------------------------------------------------------------------
  // Inputs and outputs
  // ------------------------------------------------------------------------

  type HubInput =
    | SubmitHInput({ nonce: Nonce, payload: Payload })
    // A lookup, together with what the hub's indexer would answer. The answer
    // is used only when the queue misses.
    | LookupHInput({ nonce: Nonce, txid: TxId, answer: IndexerAnswer })
    | TipHInput(Height)        // the cadence loop observes a tip
    // No forward tip progress for the staleness window. The height is what
    // the free-running cadence clock now reads.
    | StaleHInput(Height)
    | FlushDueHInput           // the cadence loop finds a flush due
    | VerdictHInput({ payload: Payload, verdict: Verdict })
    | FlushDoneHInput          // every entry of the batch has a verdict
    | DrainHInput              // shutdown signal
    | CrashHInput
    | RestartHInput

  type HubOutput =
    | AckOutput({ nonce: Nonce, kind: AckKind })
    | LookupReplyOutput({ nonce: Nonce, outcome: HubOutcome })
    | BroadcastOutput(Set[Payload])
    | RequeuedOutput({ held: int, droppedExpired: int, droppedExhausted: int })
    | NoHubOutput
    | HubErrorOutput(str)

  type HubResult = Result[HubState, HubOutput]

  pure def toAckOutput(state: HubState, nonce: Nonce, kind: AckKind): HubResult =
    { state: state, out: AckOutput({ nonce: nonce, kind: kind }) }

  pure def toLookupReplyOutput(state: HubState, nonce: Nonce, outcome: HubOutcome): HubResult =
    { state: state, out: LookupReplyOutput({ nonce: nonce, outcome: outcome }) }

  pure def toBroadcastOutput(state: HubState, batch: Set[Payload]): HubResult =
    { state: state, out: BroadcastOutput(batch) }

  pure def toRequeuedOutput(state: HubState, held: int, droppedExpired: int, droppedExhausted: int): HubResult = {
    state: state,
    out: RequeuedOutput({ held: held, droppedExpired: droppedExpired, droppedExhausted: droppedExhausted }),
  }

  pure def toNoHubOutput(state: HubState): HubResult =
    { state: state, out: NoHubOutput }

  pure def toHubErrorOutput(state: HubState, reason: str): HubResult =
    { state: state, out: HubErrorOutput(reason) }

  // ------------------------------------------------------------------------
  // Views
  // ------------------------------------------------------------------------

  /// Whether the hub answers frames at all.
  pure def isServing(state: HubState): bool =
    state.phase != Down and state.phase != Stopped

  /// Whether the tip cannot be trusted: none has been observed, or the hub is
  /// stale and its cadence is free-running.
  pure def isTipStale(state: HubState): bool =
    state.tip == None or state.cadence != Tracking

  /// The last observed tip. Read only where a tip has been observed.
  pure def observedTip(state: HubState): Height =
    state.tip.unwrapOr(0)

  /// The height the flush schedule runs on.
  pure def cadenceHeight(state: HubState): Height =
    match state.cadence {
      | Tracking => state.observedTip()
      | FreeRunning(height) => height
    }

  /// The flush epoch the cadence clock is in.
  pure def cadenceEpoch(state: HubState): int =
    state.cadenceHeight() / state.params.flushInterval

  /// The payloads waiting in the queue.
  pure def queued(state: HubState): Set[Payload] =
    state.queue.keys()

  /// The payloads out with a flush: awaiting a verdict, or awaiting requeue.
  pure def inFlight(state: HubState): Set[Payload] =
    match state.flush {
      | Idle => Set()
      | Broadcasting(flush) => flush.batch.keys().union(flush.unplaced.keys())
    }

  /// Whether a lookup for `txid` hits the queue. An entry whose bytes do not
  /// parse has no txid and is never hit.
  pure def isQueuedTxid(state: HubState, txid: TxId): bool =
    state.queued().exists(payload => payload.txid == Some(txid))

  /// Whether the cadence loop would start a flush now: the cadence clock has
  /// crossed into a new epoch, or the hub is draining and owes its final one.
  pure def isFlushDue(state: HubState): bool =
    and {
      state.flush == Idle,
      or {
        state.phase == Draining,
        and {
          state.phase == Running or state.phase == Stale,
          match state.lastEpoch {
            | Some(epoch) => state.cadenceEpoch() > epoch
            | None => false
          },
        },
      },
    }

  // ------------------------------------------------------------------------
  // Admission
  // ------------------------------------------------------------------------

  /// The next height at which a flush is scheduled, strictly after `height`.
  pure def nextFlushHeight(height: Height, flushInterval: int): Height =
    (height / flushInterval + 1) * flushInterval

  /// Whether an entry with `expiry`, judged at `tip`, provably survives the
  /// flush that would publish it with `miningMargin` blocks to spare. A
  /// transaction with no expiry always does.
  pure def survivesNextFlush(expiry: Option[Height], tip: Height, flushInterval: int, miningMargin: int): bool =
    match expiry {
      | None => true
      | Some(height) => height >= nextFlushHeight(tip, flushInterval) + miningMargin
    }

  /// The hub's decision on a submission, in the order the checks are made. No
  /// check asks a node anything: admission must not leak a transaction's
  /// arrival.
  pure def admission(state: HubState, payload: Payload): AckKind =
    if (state.isTipStale())
      Refused(TipStale)
    else if (state.phase == Draining)
      Refused(HubDraining)
    else if (not(survivesNextFlush(payload.expiry, state.observedTip(), state.params.flushInterval, state.params.miningMargin)))
      Refused(ExpiryTooTight)
    else if (state.queued().contains(payload))
      Duplicate
    else
      Admitted

  // ------------------------------------------------------------------------
  // Tip
  // ------------------------------------------------------------------------

  /// The cadence loop observes `height`. The first observation is adopted,
  /// with its epoch, and nothing is flushed. After that a forward move is
  /// followed and ends staleness; a move back within the reorg allowance is
  /// followed and does not; a larger move back is ignored.
  pure def observeTip(state: HubState, height: Height): HubResult =
    match state.tip {
      | None =>
        if (state.phase == Starting)
          { ...state,
            phase: Running,
            tip: Some(height),
            lastEpoch: Some(height / state.params.flushInterval),
          }.toNoHubOutput()
        else state.toHubErrorOutput("no cadence loop is running")
      | Some(tip) =>
        if (state.phase != Running and state.phase != Stale)
          state.toHubErrorOutput("no cadence loop is running")
        else if (state.flush != Idle)
          state.toHubErrorOutput("the tip is not observed during a flush")
        else if (height > tip)
          { ...state, phase: Running, tip: Some(height), cadence: Tracking }.toNoHubOutput()
        else if (tip - height <= state.params.reorgAllowance)
          { ...state, tip: Some(height) }.toNoHubOutput()
        else
          state.toNoHubOutput()
    }

  /// The staleness window passes with no forward progress, or passes further:
  /// the cadence clock free-runs and now reads `height`. The estimate starts at
  /// or above the observed tip and never runs backwards.
  pure def freeRun(state: HubState, height: Height): HubResult =
    if (state.flush != Idle)
      state.toHubErrorOutput("the cadence clock is not read during a flush")
    else
      match state.cadence {
        | Tracking =>
          if (state.phase == Running and height >= state.observedTip())
            { ...state, phase: Stale, cadence: FreeRunning(height) }.toNoHubOutput()
          else state.toHubErrorOutput("not a running hub, or an estimate below the observed tip")
        | FreeRunning(estimate) =>
          if (state.phase == Stale and height >= estimate)
            { ...state, cadence: FreeRunning(height) }.toNoHubOutput()
          else state.toHubErrorOutput("not a stale hub, or an estimate that runs backwards")
      }

  // ------------------------------------------------------------------------
  // Flush
  // ------------------------------------------------------------------------

  /// A flush begins: the whole queue moves out at once. An empty queue is
  /// still a flush event, and the epoch is recorded. The final flush of a
  /// draining hub with nothing to publish stops it.
  pure def beginFlush(state: HubState): HubResult =
    if (not(state.isFlushDue()))
      state.toHubErrorOutput("no flush is due")
    else if (state.queue == Map())
      if (state.phase == Draining)
        { ...state, phase: Stopped }.toNoHubOutput()
      else
        { ...state, lastEpoch: Some(state.cadenceEpoch()) }.toNoHubOutput()
    else
      { ...state,
        queue: Map(),
        flush: Broadcasting({ batch: state.queue, unplaced: Map(), final: state.phase == Draining }),
      }.toBroadcastOutput(state.queued())

  /// The indexer's verdict on one entry. Accepted and already-known entries
  /// are published and leave; a rejected entry is dropped; a retryable one is
  /// set aside for requeue.
  pure def takeVerdict(state: HubState, payload: Payload, verdict: Verdict): HubResult =
    match state.flush {
      | Idle => state.toHubErrorOutput("no flush is in flight")
      | Broadcasting(flush) =>
        if (not(flush.batch.keys().contains(payload)))
          state.toHubErrorOutput("the payload is not awaiting a verdict")
        else
          val attempts = flush.batch.get(payload)
          { ...state,
            flush: Broadcasting({ ...flush,
              batch: flush.batch.mapRemove(payload),
              unplaced: if (verdict == Retryable) flush.unplaced.put(payload, attempts) else flush.unplaced,
            }),
          }.toNoHubOutput()
    }

  /// A flush ends: the entries nothing judged go back into the queue, each
  /// decided on its own.
  ///
  /// - The same bytes already resident (resubmitted during the flush) win, and
  ///   the returning copy is dropped without being counted.
  /// - An entry that no longer survives the next flush, judged at the observed
  ///   tip exactly as admission judges it, is dropped as expired.
  /// - An entry out of attempts is dropped as exhausted. Only an entry with no
  ///   expiry gets that far.
  /// - The rest are held.
  ///
  /// After the final flush the process exits and whatever was held is lost.
  pure def endFlush(state: HubState): HubResult =
    match state.flush {
      | Idle => state.toHubErrorOutput("no flush is in flight")
      | Broadcasting(flush) =>
        if (flush.batch != Map())
          state.toHubErrorOutput("entries are still awaiting a verdict")
        else
          val params = state.params
          val returning = flush.unplaced.keys().exclude(state.queued())
          val expired = returning.filter(payload =>
            not(survivesNextFlush(payload.expiry, state.observedTip(), params.flushInterval, params.miningMargin)))
          val exhausted = returning.exclude(expired).filter(payload =>
            flush.unplaced.get(payload) + 1 > params.maxAttempts)
          val held = returning.exclude(expired).exclude(exhausted)
          val queue = state.queued().union(held).mapBy(payload =>
            if (held.contains(payload)) flush.unplaced.get(payload) + 1 else state.queue.get(payload))
          val idle =
            if (flush.final) { ...state, phase: Stopped, queue: Map(), flush: Idle }
            else { ...state, queue: queue, flush: Idle, lastEpoch: Some(state.cadenceEpoch()) }
          idle.toRequeuedOutput(held.size(), expired.size(), exhausted.size())
    }

  // ------------------------------------------------------------------------
  // The hub function
  // ------------------------------------------------------------------------

  pure def hub(state: HubState, input: HubInput): HubResult =
    match input {
      | SubmitHInput(submit) =>
        if (not(state.isServing()))
          state.toHubErrorOutput("the hub is not serving")
        else
          val kind = admission(state, submit.payload)
          // The entry is resident before the ack exists: an accepted ack is a
          // promise that the hub holds the bytes.
          val admitted =
            if (kind == Admitted) { ...state, queue: state.queue.put(submit.payload, 0) } else state
          admitted.toAckOutput(submit.nonce, kind)

      | LookupHInput(lookup) =>
        if (not(state.isServing()))
          state.toHubErrorOutput("the hub is not serving")
        // The queue first: a diverted transaction that has not been flushed
        // exists nowhere else. Then the indexer, whose answer is forwarded.
        else if (state.isQueuedTxid(lookup.txid))
          state.toLookupReplyOutput(lookup.nonce, QueueHit)
        else
          state.toLookupReplyOutput(lookup.nonce, FromIndexer(lookup.answer))

      | TipHInput(height) => observeTip(state, height)

      | StaleHInput(height) => freeRun(state, height)

      | FlushDueHInput => beginFlush(state)

      | VerdictHInput(judged) => takeVerdict(state, judged.payload, judged.verdict)

      | FlushDoneHInput => endFlush(state)

      | DrainHInput =>
        // Admission closes first; a flush already in flight finishes, and then
        // the final one runs.
        if (state.phase == Running or state.phase == Stale)
          { ...state, phase: Draining }.toNoHubOutput()
        else state.toHubErrorOutput("only a running hub can be drained")

      | CrashHInput =>
        if (state.phase == Down) state.toHubErrorOutput("the hub is already down")
        else downHub(state.params).toNoHubOutput()

      | RestartHInput =>
        if (state.phase == Down) startingHub(state.params).toNoHubOutput()
        else state.toHubErrorOutput("the hub is already up")
    }

  // ------------------------------------------------------------------------
  // Byzantine relation
  // ------------------------------------------------------------------------

  /// The transitions of a Byzantine hub. It keeps the honest schedule, and is
  /// free in what it tells its clients and in what it admits:
  ///
  /// - a submission is answered with any decision, and the payload is queued
  ///   or not, independently of the answer and of every admission rule;
  /// - a lookup is answered with any outcome: a queue hit, or anything an
  ///   indexer could say, with a body drawn from `universe` or none, at any
  ///   height.
  ///
  /// The honest transition is always a member.
  pure def byzHubResults(
    state: HubState,
    input: HubInput,
    universe: Set[Payload],
  ): Set[HubResult] =
    val honest = Set(hub(state, input))
    if (not(state.isServing())) honest
    else
      match input {
        | SubmitHInput(submit) =>
          val kinds = Set(Admitted, Duplicate)
            .union(Set(TipStale, HubDraining, ExpiryTooTight).map(refusal => Refused(refusal)))
          val holding =
            if (state.queued().contains(submit.payload)) state
            else { ...state, queue: state.queue.put(submit.payload, 0) }
          honest.union(tuples(Set(state, holding), kinds).map(((after, kind)) =>
            after.toAckOutput(submit.nonce, kind)))
        | LookupHInput(lookup) =>
          val bodies = Set(None).union(universe.map(payload => Some(payload)))
          val answers = tuples(bodies, WIRE_HEIGHTS)
            .map(((body, claimed)) => IFound({ body: body, height: claimed }))
            .union(Set(INotFound, IUnavailable))
          val outcomes = Set(QueueHit).union(answers.map(answer => FromIndexer(answer)))
          honest.union(outcomes.map(outcome => state.toLookupReplyOutput(lookup.nonce, outcome)))
        | _ => honest
      }
}

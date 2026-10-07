// -*- mode: Bluespec; -*-

/// The hub specification: one hub, the chain it publishes to, and the two
/// things it asks its indexer (what the tip is, and what became of a
/// broadcast). Submissions are inputs: who sent one, and over what network,
/// is the protocol specification's business.
///
/// This is where the schedule is checked: whether a transaction a hub admits
/// is offered to the chain, and judged by a node, before it expires. The
/// state is small enough for TLC to visit every reachable state of every
/// configuration, so "holds" here means exactly that.
///
/// How it is built. The module holds no hub logic. Every step picks an input,
/// hands it to `hub(state, input)` and records what an observer needs. A step
/// exists in two forms: `xWith(..)`, which takes every choice as a parameter
/// and is what a scripted run is made of, and `x`, which picks the choices and
/// is what `step` is made of.
///
/// A configuration is a value, held in the state and set once by `initWith`.
/// One named init per configuration (`initTimely`, ...) selects it, guarded by
/// the assumptions that configuration is checked under.
///
/// The observer's memory is four sets of payloads and one height. It is never
/// read by a step: only the properties read it.
module hubMachine {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"
  import indexer.* from "./indexer"
  import hub.* from "./hub"

  // ------------------------------------------------------------------------
  // Configuration
  // ------------------------------------------------------------------------

  /// - `payloads`: the transactions that may be submitted.
  /// - `params`: the schedule the hub is started with.
  /// - `staleWindow`: blocks without a forward tip observation after which a
  ///   hub is stale.
  /// - `maxFlightBlocks`: blocks that may arrive while one flush is in flight.
  /// - `maxHeight`: where the chain stops growing; a bound of the model.
  /// - `tip`: how the hub's view of the tip relates to the true height.
  type HubConfig = {
    payloads: Set[Payload],
    params: HubParams,
    staleWindow: int,
    maxFlightBlocks: int,
    maxHeight: Height,
    tip: TipModel,
    hubRole: Role,
    indexerRole: Role,
  }

  /// The height the chain starts at. Height 0 is kept for "in the mempool".
  pure val GENESIS_HEIGHT = 1

  /// Every height a tip report or a free-running clock may carry, in any
  /// configuration. Picks are drawn from this fixed range and filtered by the
  /// configuration, so no range has a bound that depends on the state.
  pure val CLOCK_HEIGHTS = 0.to(16)

  // ------------------------------------------------------------------------
  // Assumptions
  // ------------------------------------------------------------------------
  //
  // Each is a predicate over a configuration. A named init asserts the ones
  // its configuration is checked under, so a configuration that does not meet
  // them has no initial state and every check of it fails.

  /// The hub's startup check: delivery, one full interval and the mining
  /// margin fit inside the smallest supported expiry.
  pure def budgetFits(c: HubConfig): bool =
    scheduleFitsBudget(c.params)

  /// The same budget with the reorg allowance added. A hub that follows a tip
  /// report up to the allowance behind the chain can flush that much late.
  /// Nothing in the implementation checks this; its shipped constants meet it
  /// with equality.
  pure def reorgSlackFits(c: HubConfig): bool =
    c.params.flushInterval + c.params.miningMargin + c.params.deliveryLag + c.params.reorgAllowance
      <= c.params.minWalletExpiry

  /// Fewer blocks arrive while a flush is in flight than the mining margin
  /// reserves. The margin is measured from the height at which a flush begins;
  /// every block that arrives before the node judges a transaction is taken
  /// out of it. The implementation bounds each call to the indexer
  /// (`RPC_TIMEOUT`, `zeronym/hub/src/chain.rs`) and not the batch as a whole,
  /// and bounds neither in blocks, so this is an assumption about the
  /// environment that the code does not enforce.
  pure def flightWithinMargin(c: HubConfig): bool =
    c.maxFlightBlocks < c.params.miningMargin

  /// The same budget with the longest silence that does not yet make a hub
  /// stale. The shipped constants do not meet it.
  pure def staleSlackFits(c: HubConfig): bool =
    c.params.flushInterval + c.params.miningMargin + c.params.deliveryLag + (c.staleWindow - 1)
      <= c.params.minWalletExpiry

  /// The relations between the shipped constants that the scaled-down schedule
  /// keeps: the slack left by the startup budget equals the reorg allowance;
  /// the staleness window exceeds both the margin and that slack; the budget
  /// with the longest non-stale silence added exceeds the expiry floor by
  /// exactly one block, and without the margin it fits.
  pure def shippedRelationsKept(c: HubConfig): bool =
    val budget = c.params.flushInterval + c.params.miningMargin + c.params.deliveryLag
    and {
      c.params.minWalletExpiry - budget == c.params.reorgAllowance,
      c.staleWindow > c.params.miningMargin,
      c.staleWindow > c.params.reorgAllowance,
      budget + (c.staleWindow - 1) == c.params.minWalletExpiry + 1,
      budget - c.params.miningMargin + (c.staleWindow - 1) <= c.params.minWalletExpiry,
    }

  /// What every configuration meets: the startup budget; a schedule that
  /// moves; payloads told apart by their bytes and, where they parse, by their
  /// txid, none built before the chain starts; and a clock range wide enough
  /// for a free-running clock one interval past the last block.
  pure def standingAssumptions(c: HubConfig): bool = and {
    budgetFits(c),
    c.params.flushInterval > 0,
    c.payloads.size() == c.payloads.map(payload => payload.id).size(),
    c.payloads.filter(payload => isSome(payload.txid)).size() == txidsOf(c.payloads).size(),
    c.payloads.forall(payload => payload.created >= GENESIS_HEIGHT),
    CLOCK_HEIGHTS.contains(c.maxHeight + c.params.flushInterval),
  }

  /// The shipped relations, with the slack and the flight bound they give.
  pure def shippedAssumptions(c: HubConfig): bool = and {
    standingAssumptions(c),
    shippedRelationsKept(c),
    reorgSlackFits(c),
    flightWithinMargin(c),
    not(staleSlackFits(c)),
  }

  // ------------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------------

  /// The configuration. Written by `initWith` and by nothing else.
  var cfg: HubConfig
  /// The hub. Only ever the state of a result of `hub`.
  var h: HubState
  /// The true chain height.
  var height: Height
  /// The payloads a node has taken.
  var onChain: Set[Payload]
  /// Whether the cadence loop has asked for the tip since the last block.
  var polled: bool
  /// The true height at which the flush in flight began; 0 while none is.
  var flightStart: Height

  // The observer's memory.

  /// The payloads that have entered this hub's queue since it last came up.
  var seen: Set[Payload]
  /// Those that first did so within the delivery lag of the height they were
  /// built at. Both are forgotten when the hub goes down, as its queue is: a
  /// hub cannot be held to an arrival it no longer knows of.
  var onTime: Set[Payload]
  /// The payloads a flight has carried, once that flight has had a node's
  /// answer for them or has ended.
  var offered: Set[Payload]
  /// The payloads acknowledged as accepted that no node has judged since.
  var owed: Set[Payload]

  action initWith(c: HubConfig): bool = all {
    cfg' = c,
    h' = startingHub(c.params),
    height' = GENESIS_HEIGHT,
    onChain' = Set(),
    polled' = false,
    flightStart' = 0,
    seen' = Set(),
    onTime' = Set(),
    offered' = Set(),
    owed' = Set(),
  }

  // ------------------------------------------------------------------------
  // Views
  // ------------------------------------------------------------------------

  /// The chain as the indexer relations read it. Every transaction a node has
  /// taken is in the mempool: nothing here depends on its being mined.
  def chain: IndexerState = {
    height: height,
    txs: onChain.fold(Map(), (txs, payload) =>
      match payload.txid {
        | Some(txid) => txs.put(txid, { payload: payload, at: InMempool })
        | None => txs
      }),
    offered: Set(),
  }

  /// How far behind the chain the hub's observed tip is.
  def lag: int = height - h.observedTip()

  /// The entries of the batch still waiting for a verdict.
  def awaiting: Set[Payload] =
    match h.flush {
      | Broadcasting(flush) => flush.batch.keys()
      | Idle => Set()
    }

  pure def isError(output: HubOutput): bool =
    match output {
      | HubErrorOutput(_) => true
      | _ => false
    }

  /// Whether `output` is an ack that promises the hub holds the payload.
  pure def isAcceptedAck(output: HubOutput): bool =
    match output {
      | AckOutput(ack) => isAccepted(ack.kind)
      | _ => false
    }

  // ------------------------------------------------------------------------
  // The oracles
  // ------------------------------------------------------------------------
  //
  // The hub asks its indexer two things. Both answers are the relations of
  // `indexer.qnt`, not a second model of them: the honest one, or, for a
  // Byzantine indexer, the wider one.

  /// The tips the indexer may report now.
  def tips: Set[Height] =
    match cfg.indexerRole {
      | Honest =>
        honestTips(chain, if (cfg.tip == TipMayRegress) cfg.params.reorgAllowance else 0, CLOCK_HEIGHTS)
      | Byzantine => byzTips(CLOCK_HEIGHTS)
    }

  /// What a broadcast of `payload` may come to: the verdict the hub is given,
  /// and whether the network took the transaction.
  def broadcastResults(payload: Payload): Set[IndexerResult] =
    match cfg.indexerRole {
      | Honest => honestIndexerResults(chain, BroadcastIInput(payload))
      | Byzantine => byzIndexerResults(chain, BroadcastIInput(payload), cfg.payloads, CLOCK_HEIGHTS)
    }

  /// The indexer's transition that gives `given` on `payload` and relays the
  /// transaction to the network, or does not.
  def broadcastResult(payload: Payload, given: Verdict, relayed: bool): IndexerResult = {
    state: indexerApply(chain, BroadcastIInput(payload), VerdictOutput(if (relayed) Accepted else Retryable)),
    out: VerdictOutput(given),
  }

  /// The verdicts on `payload`, each with whether the network took it.
  def verdictsOn(payload: Payload): Set[(Verdict, bool)] =
    tuples(Set(Accepted, AlreadyKnown, Rejected, Retryable), Set(true, false)).filter(((given, relayed)) =>
      broadcastResults(payload).contains(broadcastResult(payload, given, relayed)))

  /// The heights a stale hub's free-running clock may read now: not behind
  /// the chain, and at most one flush interval ahead of it. The
  /// implementation relies on the first ("during a real stall blocks arrive
  /// slower than this", `zeronym/hub/src/batcher.rs:64-67`) and enforces
  /// neither.
  def freeRunEstimates: Set[Height] =
    CLOCK_HEIGHTS.filter(estimate => height <= estimate and estimate <= height + cfg.params.flushInterval)

  /// The transitions the hub may take on a submission of `payload`: the one
  /// the protocol prescribes, or, for a Byzantine hub, any decision with the
  /// payload queued or not. A Byzantine hub keeps the honest schedule.
  def submitResults(payload: Payload): Set[HubResult] =
    val input = SubmitHInput({ nonce: 0, payload: payload })
    match cfg.hubRole {
      | Honest => Set(hub(h, input))
      | Byzantine => byzHubResults(h, input, cfg.payloads, Set())
    }

  // ------------------------------------------------------------------------
  // Frames
  // ------------------------------------------------------------------------

  action chainKept = all { height' = height, onChain' = onChain, polled' = polled }
  action admissionsKept = all { seen' = seen, onTime' = onTime }
  action admissionsForgotten = all { seen' = Set(), onTime' = Set() }
  action memoryKept = all { admissionsKept, offered' = offered, owed' = owed }

  /// The hub takes `input` on its own schedule. A step that would change
  /// nothing is not taken.
  action hubTakes(input: HubInput): bool =
    val result = hub(h, input)
    all {
      not(isError(result.out)),
      result.state != h,
      h' = result.state,
      cfg' = cfg,
    }

  // ------------------------------------------------------------------------
  // Steps
  // ------------------------------------------------------------------------

  /// The cadence loop observes the tip its indexer reports.
  ///
  /// The poll is recorded whether or not the answer moves the hub's tip: a hub
  /// that asked and was told nothing new has still asked. A second poll in the
  /// same block that changes nothing is not taken.
  action observeWith(tip: Height): bool =
    val result = hub(h, TipHInput(tip))
    all {
      tips.contains(tip),
      not(isError(result.out)),
      result.state != h or not(polled),
      h' = result.state,
      polled' = true,
      height' = height,
      onChain' = onChain,
      flightStart' = flightStart,
      memoryKept,
      cfg' = cfg,
    }

  action observe = {
    nondet tip = oneOf(tips)
    observeWith(tip)
  }

  /// A hub that has seen no tip progress for the staleness window goes stale,
  /// or, already stale, reads its free-running clock again.
  action staleWith(estimate: Height): bool = all {
    cfg.tip == TipMayLag,
    h.phase == Stale or lag >= cfg.staleWindow,
    freeRunEstimates.contains(estimate),
    hubTakes(StaleHInput(estimate)),
    flightStart' = flightStart,
    chainKept,
    memoryKept,
  }

  action stale = {
    nondet estimate = oneOf(freeRunEstimates)
    staleWith(estimate)
  }

  /// A submission of `payload` reaches the hub, which takes `result`. A
  /// payload does not exist before it is built, whoever submits it; after
  /// that it may arrive at any time and any number of times.
  action submitWith(payload: Payload, result: HubResult): bool =
    val entered = result.state.queued().contains(payload) and not(seen.contains(payload))
    all {
      cfg.payloads.contains(payload),
      height >= payload.created,
      h.isServing(),
      submitResults(payload).contains(result),
      not(isError(result.out)),
      h' = result.state,
      seen' = if (entered) seen.union(Set(payload)) else seen,
      onTime' =
        if (entered and height <= payload.created + cfg.params.deliveryLag) onTime.union(Set(payload))
        else onTime,
      owed' = if (isAcceptedAck(result.out)) owed.union(Set(payload)) else owed,
      offered' = offered,
      flightStart' = flightStart,
      chainKept,
      cfg' = cfg,
    }

  action submit = all {
    h.isServing(),
    {
      nondet payload = oneOf(cfg.payloads)
      nondet result = oneOf(submitResults(payload))
      submitWith(payload, result)
    },
  }

  /// A flush begins: the hub's whole queue goes out at once. With nothing
  /// queued the epoch is recorded, or a draining hub stops, and no flight
  /// starts.
  action flushBegin =
    val result = hub(h, FlushDueHInput)
    all {
      hubTakes(FlushDueHInput),
      flightStart' = if (result.state.flush == Idle) 0 else height,
      if (result.state.phase == Stopped) admissionsForgotten else admissionsKept,
      offered' = offered,
      owed' = owed,
      chainKept,
    }

  /// The indexer returns `given` on one entry of the batch, and the network
  /// has taken the transaction or has not. A verdict other than `Retryable`
  /// is a node's judgement: it settles the entry.
  action verdictWith(payload: Payload, given: Verdict, relayed: bool): bool =
    val judged = given != Retryable
    all {
      awaiting.contains(payload),
      verdictsOn(payload).contains((given, relayed)),
      hubTakes(VerdictHInput({ payload: payload, verdict: given })),
      onChain' = if (relayed) onChain.union(Set(payload)) else onChain,
      offered' = if (judged) offered.union(Set(payload)) else offered,
      owed' = if (judged) owed.exclude(Set(payload)) else owed,
      height' = height,
      polled' = polled,
      flightStart' = flightStart,
      admissionsKept,
    }

  action verdict = all {
    awaiting != Set(),
    {
      nondet payload = oneOf(awaiting)
      nondet outcome = oneOf(verdictsOn(payload))
      verdictWith(payload, outcome._1, outcome._2)
    },
  }

  /// A flush ends: what nothing judged is requeued or dropped. The flight is
  /// over for every entry it carried. The final flush stops the hub.
  action flushEnd =
    val result = hub(h, FlushDoneHInput)
    all {
      hubTakes(FlushDoneHInput),
      offered' = offered.union(h.inFlight()),
      flightStart' = 0,
      owed' = owed,
      if (result.state.phase == Stopped) admissionsForgotten else admissionsKept,
      chainKept,
    }

  /// The hub gets its shutdown signal.
  action drain = all {
    hubTakes(DrainHInput),
    flightStart' = flightStart,
    chainKept,
    memoryKept,
  }

  /// The hub's process dies, or exits after its final flush. A flight it had
  /// out is over.
  action crash = all {
    hubTakes(CrashHInput),
    offered' = offered.union(h.inFlight()),
    flightStart' = 0,
    owed' = owed,
    admissionsForgotten,
    chainKept,
  }

  action restart = all {
    hubTakes(RestartHInput),
    flightStart' = flightStart,
    chainKept,
    memoryKept,
  }

  /// Whether the next block may arrive. This is where the timing assumptions
  /// live: a block is held back until the hub has done what the tip model says
  /// it does within a block.
  ///
  /// In every model, a flush that is due has begun, and no flush has been in
  /// flight for `maxFlightBlocks` blocks already. A hub whose flush is in
  /// flight is not looking at the tip, so the clauses below bind it only while
  /// it is idle.
  ///
  /// - `TipTimely`: a running hub has asked for the tip since the last block.
  ///   An honest indexer answers with the true height, so the hub's lag is
  ///   zero at every block. A Byzantine one is asked just as often; what it
  ///   controls is the answer.
  /// - `TipMayRegress`: a running hub is no further behind than the allowance.
  /// - `TipMayLag`: a hub whose silence has reached the staleness window has
  ///   become stale, and a stale hub's free-running clock has caught up with
  ///   the current block.
  def mayAdvance: bool = and {
    height < cfg.maxHeight,
    not(h.isFlushDue()),
    h.flush == Idle or height - flightStart < cfg.maxFlightBlocks,
    h.flush == Idle implies
      match cfg.tip {
        | TipTimely => h.phase == Running implies polled
        | TipMayRegress => h.phase == Running implies lag <= cfg.params.reorgAllowance
        | TipMayLag =>
          and {
            h.phase == Running implies lag < cfg.staleWindow,
            h.phase == Stale implies h.cadenceHeight() >= height,
          }
      },
  }

  /// The chain grows by one block.
  action advance = all {
    mayAdvance,
    height' = height + 1,
    polled' = false,
    onChain' = onChain,
    h' = h,
    flightStart' = flightStart,
    memoryKept,
    cfg' = cfg,
  }

  /// One step of the hub and its environment.
  action step = any {
    observe, stale, submit, flushBegin, verdict, flushEnd, advance,
    drain, crash, restart,
  }

  // The relations below are parts of `step`. A property that holds under
  // `step` holds under each; one that fails under a part fails under `step`,
  // and the part says which faults the failure does not need.

  /// No crash: the hub may still be shut down and started again.
  action noCrashStep = any {
    observe, stale, submit, flushBegin, verdict, flushEnd, advance,
    drain, restart,
  }

  /// No shutdown, no crash, no restart.
  action quietStep = any {
    observe, stale, submit, flushBegin, verdict, flushEnd, advance,
  }

  // ------------------------------------------------------------------------
  // Guarantees
  // ------------------------------------------------------------------------
  //
  // G6 comes in three parts. G6a and G6b are about the moment a flush begins:
  // how much of the mining margin is left when the hub hands the batch over.
  // They do not say a node accepts the transaction, because the chain may move
  // while the batch is in flight. G6c is about the moment a node judges it.
  //
  // Each is a predicate on the current state. That is enough: the whole batch
  // is in flight in the state `flushBegin` produces, with `flightStart` the
  // height it began at; and an entry awaiting a verdict can be given one, a
  // rejection at least, at whatever height the chain has reached.

  /// Whether `payload`, published at height `at`, can still be mined
  /// `miningMargin` blocks later.
  def marginLeft(payload: Payload, at: Height): bool =
    match payload.expiry {
      | Some(expiry) => expiry >= at + cfg.params.miningMargin
      | None => true
    }

  /// Whether `payload` reached this hub as a supported wallet's would: it
  /// honours the expiry floor, and it first entered the queue of this run of
  /// the hub within the delivery lag of the height it was built at. A copy
  /// that arrives late at a hub that has been down since the first one is not
  /// timely, whatever happened before (K8).
  def isConformingAndOnTime(payload: Payload): bool =
    conforming(payload, cfg.params.minWalletExpiry) and onTime.contains(payload)

  /// Whether the flight `payload` is on, if any, is its first.
  def isFirstOffer(payload: Payload): bool =
    not(offered.contains(payload))

  /// G6a. Every transaction the hub offers is offered with the mining margin
  /// to spare: whatever was admitted, on every attempt. A claim about the
  /// margin left at the offer, not about acceptance.
  val offeredBeforeExpiry =
    h.inFlight().forall(payload => marginLeft(payload, flightStart))

  /// G6b. The same, for supported wallets only and for the first time the hub
  /// offers the transaction. It says nothing about a later offer of an entry
  /// that was requeued; see `conformingEveryOfferBeforeExpiry`.
  val conformingFirstOfferBeforeExpiry =
    h.inFlight().forall(payload =>
      isConformingAndOnTime(payload) and isFirstOffer(payload) implies marginLeft(payload, flightStart))

  /// G6c. The end-to-end claim: when a node judges the first offer of a
  /// supported wallet's transaction, the transaction has not expired. It can
  /// still be mined in the next block, so the node does not turn it away for
  /// its expiry.
  ///
  /// The margin is what pays for the blocks that arrive while the batch is in
  /// flight. G6c therefore needs G6b and one thing more: that fewer blocks
  /// than the mining margin arrive during a flush (`flightWithinMargin`).
  val conformingFirstOfferJudgedBeforeExpiry =
    awaiting.forall(payload =>
      isConformingAndOnTime(payload) and isFirstOffer(payload) implies
        match payload.expiry {
          | Some(expiry) => expiry > height
          | None => true
        })

  // ------------------------------------------------------------------------
  // Known gaps: invariants that do not hold
  // ------------------------------------------------------------------------

  /// K6. G6b without its restriction to the first offer. This does not hold
  /// once a hub is stale: requeue judges an entry at the observed tip, which
  /// has stopped, while the flush schedule runs on.
  val conformingEveryOfferBeforeExpiry =
    h.inFlight().forall(payload => isConformingAndOnTime(payload) implies marginLeft(payload, flightStart))

  /// K5. A payload the hub has acknowledged is accounted for: the hub still
  /// holds it, queued or in flight; or the chain has it; or a node has judged
  /// it since the ack. Being offered is not enough: an offer nothing judged
  /// settles nothing.
  ///
  /// This does not hold. The queue lives in memory: a crash after the ack
  /// loses it; so does a final flush that finds the indexer unreachable; and
  /// so does a requeue that gives the entry up as expired.
  val ackedIsHeldOrSettled =
    owed.subseteq(h.queued().union(h.inFlight()).union(onChain))

  // ------------------------------------------------------------------------
  // Reachability
  // ------------------------------------------------------------------------
  //
  // States that must be reachable, each checked as `not(..)` expected to be
  // violated. The first three are the antecedents of G6a, G6b and G6c: without
  // them a "holds" could be vacuous. The rest are one per family of steps: TLC
  // is run with deadlock checking off, so a configuration whose steps died
  // after `init` would otherwise hold everything on a handful of states.

  /// An entry with an expiry is in flight.
  val wOfferWithExpiry =
    h.inFlight().exists(payload => isSome(payload.expiry))

  /// A supported wallet's transaction, with an expiry, is on its first flight.
  val wConformingFirstOffer =
    h.inFlight().exists(payload =>
      isSome(payload.expiry) and isConformingAndOnTime(payload) and isFirstOffer(payload))

  /// The same entry is still unjudged after a block has arrived.
  val wConformingFirstOfferInFlightABlock =
    height > flightStart and awaiting.exists(payload =>
      isSome(payload.expiry) and isConformingAndOnTime(payload) and isFirstOffer(payload))

  /// A flight has been answered or has ended.
  val wOffered = offered != Set()
  /// An entry nothing judged is back in the queue.
  val wRequeued = h.queued().exists(payload => h.queue.get(payload) > 0)
  val wDown = h.phase == Down
  /// The hub has been started again, owing a payload from before.
  val wRestartedOwing = h.phase == Starting and owed != Set()
  /// A block has arrived while a flush is in flight.
  val wBlockInFlight = flightStart > 0 and height > flightStart
  val wStopped = h.phase == Stopped
  val wStale = h.phase == Stale
  /// A timely payload is queued while the cadence clock is in an epoch before
  /// the one the hub last recorded: the tip went back across a boundary the
  /// hub has already acted on.
  val wTimelyQueuedBehindEpoch =
    h.queued().exists(payload => onTime.contains(payload)) and
      match h.lastEpoch {
        | Some(epoch) => h.cadenceEpoch() < epoch
        | None => false
      }

  // ------------------------------------------------------------------------
  // Configurations
  // ------------------------------------------------------------------------
  //
  // The schedule flushes every 3 blocks with a mining margin of 2 and a
  // delivery lag of 1, and supports wallets that set an expiry 7 blocks out.
  // It is the shipped one scaled down (shipped: interval 20, margin 4, lag 6,
  // reorg allowance 10, staleness window 12 blocks, expiry floor 40), keeping
  // the relations `shippedRelationsKept` names. The margin is 2, the smallest
  // that leaves room for a block to arrive while a flush is in flight.

  pure def orchard(id: str, created: Height, expiry: Height): Payload =
    { id: id, txid: Some(id), created: created, expiry: Some(expiry), class: OrchardTouching, oversize: false }

  /// Two migrations from supported wallets, built at heights 2 and 4.
  pure val early = orchard("early", 2, 9)
  pure val late = orchard("late", 4, 11)
  /// A migration whose wallet set its expiry tighter than the supported floor.
  pure val tight = orchard("tight", 2, 5)

  /// Every component honest, and a hub that sees each block before the next.
  pure val timely: HubConfig = {
    payloads: Set(early, late, tight),
    params: {
      flushInterval: 3,
      miningMargin: 2,
      deliveryLag: 1,
      minWalletExpiry: 7,
      reorgAllowance: 1,
      maxAttempts: 2,
      queueCap: 2,
    },
    staleWindow: 3,
    maxFlightBlocks: 1,
    maxHeight: 12,
    tip: TipTimely,
    hubRole: Honest,
    indexerRole: Honest,
  }

  // A tip that may be reported up to the reorg allowance behind the chain:
  // with the slack that covers it, and without. In the second the expiry
  // floor is the three-term budget exactly.
  pure val flakyTip: HubConfig = { ...timely, tip: TipMayRegress }
  pure val flakyTipNoSlack: HubConfig = {
    ...flakyTip,
    params: { ...flakyTip.params, minWalletExpiry: 6 },
    payloads: Set(orchard("early", 2, 8), late, tight),
  }

  // The same tip, and a flush that may stay in flight for as many blocks as
  // the mining margin reserves.
  pure val flakyTipSlowFlight: HubConfig = { ...flakyTip, maxFlightBlocks: 2 }

  // A hub that may go without a tip for a while: on the shipped relation
  // between the staleness window and the expiry floor, and on the relation
  // that would cover the silence. These two carry the supported wallets'
  // migrations only. A free-running clock multiplies the states, and with
  // `tight` as well TLC does not exhaust either in five minutes. `tight` gives
  // G6a a shorter counterexample on them (8 states against 13) and no check
  // of them a different verdict.
  pure val staleLag: HubConfig = { ...timely, tip: TipMayLag, payloads: Set(early, late) }
  pure val staleLagWithSlack: HubConfig = {
    ...staleLag,
    params: { ...staleLag.params, minWalletExpiry: 8 },
    payloads: Set(orchard("early", 2, 10), orchard("late", 4, 12)),
  }

  // One Byzantine component at a time, on the schedule of `timely`.
  pure val byzHub: HubConfig = { ...timely, hubRole: Byzantine }
  pure val byzIndexer: HubConfig = { ...timely, indexerRole: Byzantine }

  /// Bytes neither the shim nor the hub can parse: no txid, no expiry. After
  /// a network upgrade a build does not know, every transaction looks like
  /// this to it.
  pure val junk: Payload =
    { id: "junk", txid: None, created: 1, expiry: None, class: Unparseable, oversize: false }
  pure val unknownUpgrade: HubConfig = { ...timely, payloads: Set(early, tight, junk) }

  // One named init per configuration. A configuration built to show what a
  // relation buys asserts that the relation is false of it, so that its known
  // gap stays pinned on the missing relation.

  action initTimely = all { shippedAssumptions(timely), initWith(timely) }
  action initFlakyTip = all { shippedAssumptions(flakyTip), initWith(flakyTip) }
  action initFlakyTipNoSlack = all {
    standingAssumptions(flakyTipNoSlack),
    flightWithinMargin(flakyTipNoSlack),
    not(reorgSlackFits(flakyTipNoSlack)),
    initWith(flakyTipNoSlack),
  }
  action initFlakyTipSlowFlight = all {
    standingAssumptions(flakyTipSlowFlight),
    shippedRelationsKept(flakyTipSlowFlight),
    reorgSlackFits(flakyTipSlowFlight),
    not(flightWithinMargin(flakyTipSlowFlight)),
    initWith(flakyTipSlowFlight),
  }
  action initStaleLag = all { shippedAssumptions(staleLag), initWith(staleLag) }
  action initByzHub = all { shippedAssumptions(byzHub), initWith(byzHub) }
  action initByzIndexer = all { shippedAssumptions(byzIndexer), initWith(byzIndexer) }
  action initUnknownUpgrade = all { shippedAssumptions(unknownUpgrade), initWith(unknownUpgrade) }
  action initStaleLagWithSlack = all {
    standingAssumptions(staleLagWithSlack),
    reorgSlackFits(staleLagWithSlack),
    flightWithinMargin(staleLagWithSlack),
    staleSlackFits(staleLagWithSlack),
    not(shippedRelationsKept(staleLagWithSlack)),
    initWith(staleLagWithSlack),
  }
}

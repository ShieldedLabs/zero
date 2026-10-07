// -*- mode: Bluespec; -*-

/// The zeronym protocol as a state machine: a wallet, a shim, one or more
/// hubs, the chain behind the hubs' indexer, an unreliable network between
/// them, and a third party that can reach the hubs.
///
/// What is assumed:
///
/// - Roles. The shim and the hubs run in enclaves and are honest in the
///   baseline. The shim, each hub and the hubs' indexer can each be made
///   Byzantine through `ROLES`; a Byzantine component draws its transitions
///   from a wider relation and is not marked in any other way.
/// - Network. Frames may be lost, duplicated, delayed and reordered. They
///   cannot be forged or read in transit.
/// - Third party. A client of the hubs' public address. It looks up txids it
///   knows and submits payloads it has learned or made. It cannot read or
///   forge frames, so it does not know a nonce and cannot answer the shim.
/// - Nonces are unique. A counter stands for an unguessable random value.
/// - Chain. A transaction's status only moves forward: no reorg of an
///   included transaction, no mempool eviction. How a hub's view of the tip
///   relates to the true height is the constant `TIP`.
/// - Wallets. A supported wallet sets an expiry at least `MIN_WALLET_EXPIRY`
///   blocks after the height it builds at, and its frame reaches a hub within
///   `DELIVERY_LAG` blocks.
/// - Time. There is no clock. A timeout is an event that may happen at any
///   moment, and the staleness window is counted in blocks.
///
/// How it is built. This module holds no protocol logic. Every step picks an
/// input, hands it to one component function (`shim`, `hub`, or the indexer
/// relation) and puts the output where it goes. Each step exists in two
/// forms: `xWith(..)`, which takes every choice as a parameter and is what a
/// scripted run is made of, and `x`, which picks the choices and is what
/// `step` is made of. All state is written by `commit`, so no action has a
/// frame condition.
module protocol {
  import basicSpells.* from "./spells/basicSpells"
  import soup.* from "./spells/soup"
  import types.* from "./types"
  import wire.* from "./wire"
  import indexer.* from "./indexer"
  import hub.* from "./hub"
  import shim.* from "./shim"
  import state.* from "./state"
  import properties.* from "./properties"

  // ------------------------------------------------------------------------
  // Constants
  // ------------------------------------------------------------------------

  /// The configuration. It is one record so that a configuration is a value
  /// that can be named, derived from another and shared between the module
  /// that simulates it and the modules that script runs against it. The names
  /// below are its fields, and are what the rest of the module uses.
  const CONFIG: Config

  /// The transactions a wallet may send.
  pure val PAYLOADS = CONFIG.payloads
  /// Twins of wallet transactions: other bytes with the same txid. No honest
  /// party sends one.
  pure val TWINS = CONFIG.twins
  /// Payloads of the third party's own making.
  pure val TP_PAYLOADS = CONFIG.tpPayloads
  /// The hub addresses, in the order the shim is configured with.
  pure val HUB_ORDER = CONFIG.hubs
  pure val HUBS = HUB_ORDER.indices().map(i => HUB_ORDER[i])

  /// Blocks between scheduled flushes.
  pure val FLUSH_INTERVAL = CONFIG.flushInterval
  /// Blocks a published transaction needs to be mined.
  pure val MINING_MARGIN = CONFIG.miningMargin
  /// Blocks a submission may take to reach a hub.
  pure val DELIVERY_LAG = CONFIG.deliveryLag
  /// How far behind the chain a tip report may be and still be followed.
  pure val REORG_ALLOWANCE = CONFIG.reorgAllowance
  /// Blocks without a forward tip observation after which a hub is stale.
  pure val STALE_WINDOW = CONFIG.staleWindow
  /// How a stale hub's free-running clock relates to the true height.
  pure val FREE_RUN = CONFIG.freeRun
  /// The smallest expiry delta a supported wallet sets.
  pure val MIN_WALLET_EXPIRY = CONFIG.minWalletExpiry
  /// Requeues an entry is allowed.
  pure val MAX_ATTEMPTS = CONFIG.maxAttempts
  /// Entries a hub's admission will hold.
  pure val QUEUE_CAP = CONFIG.queueCap

  /// Blocks that may arrive while one flush is in flight.
  pure val MAX_FLIGHT_BLOCKS = CONFIG.maxFlightBlocks

  /// Bounds of the model: the chain stops growing at `MAX_HEIGHT`, and the
  /// wallet makes at most `MAX_REQUESTS` sends and as many lookups, as does
  /// the third party.
  pure val MAX_HEIGHT = CONFIG.maxHeight
  pure val MAX_REQUESTS = CONFIG.maxRequests

  pure val ROLES = CONFIG.roles
  pure val TIP = CONFIG.tip

  /// Every payload that exists. A Byzantine component builds its lies from it.
  pure val UNIVERSE = PAYLOADS.union(TWINS).union(TP_PAYLOADS)
  /// Every height a Byzantine component may claim.
  pure val HEIGHTS = 0.to(MAX_HEIGHT)
  /// Every height a free-running cadence clock may read: up to one flush
  /// interval past the last block.
  pure val CLOCK_HEIGHTS = 0.to(MAX_HEIGHT + FLUSH_INTERVAL)
  /// What a wallet may send.
  pure val SEND_INPUTS = PAYLOADS.map(payload => Clean(payload)).union(Set(Unreadable, EmptyBody))
  /// Every transaction id there is.
  pure val TXIDS = txidsOf(UNIVERSE)
  /// The height the chain starts at. Height 0 is kept for "in the mempool".
  pure val GENESIS_HEIGHT = 1

  pure val HUB_PARAMS: HubParams = {
    flushInterval: FLUSH_INTERVAL,
    miningMargin: MINING_MARGIN,
    deliveryLag: DELIVERY_LAG,
    minWalletExpiry: MIN_WALLET_EXPIRY,
    reorgAllowance: REORG_ALLOWANCE,
    maxAttempts: MAX_ATTEMPTS,
    queueCap: QUEUE_CAP,
  }

  pure val HONEST_HUBS = HUBS.filter(hub => ROLES.hubs.get(hub) == Honest)

  // ------------------------------------------------------------------------
  // Assumptions
  // ------------------------------------------------------------------------
  //
  // Each is a named value, so that an instance's `assumptionsTest` can assert
  // it: the simulator does not enforce `assume`.

  /// The hub's startup check: delivery, one full interval and the mining
  /// margin fit inside the smallest supported expiry.
  pure val budgetFits = scheduleFitsBudget(HUB_PARAMS)

  /// The same budget with the reorg allowance added. A hub that follows a tip
  /// report up to the allowance behind the chain can flush that much late.
  /// Nothing in the implementation checks this; its shipped constants meet it
  /// with equality.
  pure val reorgSlackFits =
    FLUSH_INTERVAL + MINING_MARGIN + DELIVERY_LAG + REORG_ALLOWANCE <= MIN_WALLET_EXPIRY

  /// Fewer blocks arrive while a flush is in flight than the mining margin
  /// reserves. The margin is measured from the height at which a flush begins;
  /// every block that arrives before the node judges a transaction is taken
  /// out of it. The implementation bounds each call to the indexer
  /// (`RPC_TIMEOUT`, `zeronym/hub/src/chain.rs`) and not the batch as a whole,
  /// and bounds neither in blocks, so this is an assumption about the
  /// environment that the code does not enforce.
  pure val flightWithinMargin = MAX_FLIGHT_BLOCKS < MINING_MARGIN

  /// The relations between the shipped constants that the scaled-down schedule
  /// is meant to keep: the slack left by the startup budget equals the reorg
  /// allowance; the staleness window exceeds both the margin and that slack;
  /// the budget with the longest non-stale silence added exceeds the expiry
  /// floor by exactly one block, and without the margin it fits.
  pure val shippedRelationsKept = and {
    MIN_WALLET_EXPIRY - (FLUSH_INTERVAL + MINING_MARGIN + DELIVERY_LAG) == REORG_ALLOWANCE,
    STALE_WINDOW > MINING_MARGIN,
    STALE_WINDOW > REORG_ALLOWANCE,
    FLUSH_INTERVAL + MINING_MARGIN + DELIVERY_LAG + (STALE_WINDOW - 1) == MIN_WALLET_EXPIRY + 1,
    FLUSH_INTERVAL + DELIVERY_LAG + (STALE_WINDOW - 1) <= MIN_WALLET_EXPIRY,
  }

  /// The same budget with the longest silence that does not yet make a hub
  /// stale. It is named, not assumed: the shipped constants do not meet it.
  pure val staleSlackFits =
    FLUSH_INTERVAL + MINING_MARGIN + DELIVERY_LAG + (STALE_WINDOW - 1) <= MIN_WALLET_EXPIRY

  /// A stale hub's free-running cadence clock is never behind the true height.
  /// The implementation relies on this ("during a real stall blocks arrive
  /// slower than this, so the free-running clock runs ahead of the true
  /// height") and does not enforce it.
  pure val freeRunNotSlowerThanChain = FREE_RUN == NotSlower

  pure val flushIntervalPositive = FLUSH_INTERVAL > 0

  pure val hubsNonEmpty = HUBS != Set() and HUBS.size() == HUB_ORDER.length()

  /// Payloads are told apart by their bytes; a twin is a twin of something a
  /// wallet sends; the third party's payloads are its own; every hub has a
  /// role.
  pure val payloadsWellFormed = and {
    UNIVERSE.size() == UNIVERSE.map(payload => payload.id).size(),
    PAYLOADS.intersect(TP_PAYLOADS) == Set(),
    TWINS.forall(twin => PAYLOADS.exists(payload => areTwins(twin, payload))),
    UNIVERSE.forall(payload => payload.created >= GENESIS_HEIGHT),
    ROLES.hubs.keys() == HUBS,
  }

  /// The assumptions every configuration meets. `reorgSlackFits` and
  /// `flightWithinMargin` are kept apart: a configuration says whether it
  /// relies on each, and some exist to show what happens without one.
  pure val standingAssumptions = and {
    budgetFits,
    freeRunNotSlowerThanChain,
    flushIntervalPositive,
    hubsNonEmpty,
    payloadsWellFormed,
  }

  assume _ = budgetFits
  assume _ = CONFIG.reliesOnReorgSlack implies reorgSlackFits
  assume _ = CONFIG.reliesOnFlightWithinMargin implies flightWithinMargin
  assume _ = freeRunNotSlowerThanChain
  assume _ = flushIntervalPositive
  assume _ = hubsNonEmpty
  assume _ = payloadsWellFormed

  // ------------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------------

  /// The protocol state.
  var s: System
  /// The observer's record of the run. No step reads it.
  var audit: Audit
  /// The step that produced the current state.
  var lastAction: Label

  /// The only writer of the variables.
  action commit(post: System, label: Label): bool = all {
    s' = post,
    audit' = advance(audit, s, post),
    lastAction' = label,
  }

  pure val INITIAL = initialSystem(CONFIG, HUB_PARAMS, GENESIS_HEIGHT)

  action init = all {
    s' = INITIAL,
    audit' = initialAudit(INITIAL),
    lastAction' = Init,
  }

  // ------------------------------------------------------------------------
  // Roles
  // ------------------------------------------------------------------------

  pure def hubResults(id: HubId, state: HubState, input: HubInput): Set[HubResult] =
    match ROLES.hubs.get(id) {
      | Honest => Set(hub(state, input))
      | Byzantine => byzHubResults(state, input, UNIVERSE, HEIGHTS)
    }

  pure def indexerResults(state: IndexerState, input: IndexerInput): Set[IndexerResult] =
    match ROLES.indexer {
      | Honest => honestIndexerResults(state, input)
      | Byzantine => byzIndexerResults(state, input, UNIVERSE, HEIGHTS)
    }

  /// What the indexer may answer a hub's lookup for the transaction `msg`
  /// asks about. A frame that is not a lookup needs no answer.
  pure def lookupAnswers(state: IndexerState, msg: Msg): Set[IndexerAnswer] =
    match msg {
      | Lookup(lookup) =>
        indexerResults(state, LookupIInput(lookup.txid)).fold(Set(), (acc, result) =>
          match result.out {
            | AnswerOutput(answer) => acc.union(Set(answer))
            | _ => acc
          })
      | _ => Set(INotFound)
    }

  pure def isShimError(output: ShimOutput): bool =
    match output {
      | ShimErrorOutput(_) => true
      | _ => false
    }

  pure def isHubError(output: HubOutput): bool =
    match output {
      | HubErrorOutput(_) => true
      | _ => false
    }

  // ------------------------------------------------------------------------
  // Tip models
  // ------------------------------------------------------------------------

  /// The tips the indexer may report to a hub now.
  def reportableTips: Set[Height] =
    match ROLES.indexer {
      | Byzantine => byzTips(HEIGHTS)
      | Honest =>
        match TIP {
          | TipTimely => honestTips(s.indexer, 0, HEIGHTS)
          | TipMayRegress => honestTips(s.indexer, REORG_ALLOWANCE, HEIGHTS)
          | TipMayLag => honestTips(s.indexer, 0, HEIGHTS)
        }
    }

  /// How far behind the chain `hub`'s observed tip is.
  def lagOf(id: HubId): int =
    s.height() - s.hubs.get(id).observedTip()

  /// The heights a stale hub's free-running clock may read now: up to an
  /// interval ahead of the chain, and not behind it unless `FREE_RUN` allows.
  def freeRunEstimates(id: HubId): Set[Height] =
    val earliest = match FREE_RUN {
      | NotSlower => s.height()
      | MayBeSlower => s.hubs.get(id).observedTip()
    }
    CLOCK_HEIGHTS.filter(estimate => earliest <= estimate and estimate <= s.height() + FLUSH_INTERVAL)

  /// Whether the next block may arrive. This is where the timing assumptions
  /// live: a block is held back until every hub has done what the tip model
  /// says it does within a block.
  ///
  /// In every model, a flush that is due has begun, and no flush has been in
  /// flight for `MAX_FLIGHT_BLOCKS` blocks already. A hub whose flush is in
  /// flight is not looking at the tip, so the clauses below bind a hub only
  /// while it is idle.
  ///
  /// - `TipTimely`: every running hub has asked for the tip since the last
  ///   block. An honest indexer answers with the true height, so the hub's lag
  ///   is zero at every block. A Byzantine one is asked just as often; what it
  ///   controls is the answer.
  /// - `TipMayRegress`: no running hub is further behind than the allowance.
  /// - `TipMayLag`: a hub whose silence has reached the staleness window has
  ///   become stale, and a stale hub's free-running clock has caught up with
  ///   the current block.
  def chainMayAdvance: bool = and {
    s.height() < MAX_HEIGHT,
    HUBS.forall(id => not(s.hubs.get(id).isFlushDue())),
    HUBS.forall(id => s.flightBlocks.get(id) < MAX_FLIGHT_BLOCKS or s.hubs.get(id).flush == Idle),
    HUBS.forall(id =>
      val state = s.hubs.get(id)
      state.flush == Idle implies
        match TIP {
          | TipTimely =>
            state.phase == Running implies s.polled.contains(id)
          | TipMayRegress =>
            state.phase == Running implies lagOf(id) <= REORG_ALLOWANCE
          | TipMayLag =>
            and {
              state.phase == Running implies lagOf(id) < STALE_WINDOW,
              (state.phase == Stale and FREE_RUN == NotSlower) implies state.cadenceHeight() >= s.height(),
            }
        }),
  }

  // ------------------------------------------------------------------------
  // Wallet
  // ------------------------------------------------------------------------

  /// The wallet sends a transaction. The shim routes it and, unless it is
  /// waiting for a hub's decision, answers.
  action walletSendWith(input: SendInput, handedOver: int): bool = all {
    s.wallet.sends < MAX_REQUESTS,
    SEND_INPUTS.contains(input),
    // A wallet cannot send a transaction before it has built it.
    match input {
      | Clean(payload) => payload.created <= s.height()
      | _ => true
    },
    val result = shim(s.shim, SendTxSInput({ input: input, handedOver: handedOver }))
    all {
      not(isShimError(result.out)),
      commit(s.countSend().shimStepped(result, None), WalletSend({ input: input, handedOver: handedOver })),
    },
  }

  action walletSend = all {
    s.wallet.sends < MAX_REQUESTS,
    {
      nondet input = oneOf(SEND_INPUTS)
      nondet handedOver = oneOf(0.to(HUB_ORDER.length()))
      walletSendWith(input, handedOver)
    },
  }

  /// The txids of the transactions the wallet has handed the shim. A wallet
  /// asks only about its own transactions.
  def walletTxids: Set[TxId] =
    s.sentByWallet().map(payload => walletTxid(payload))

  /// The wallet asks for a transaction it has sent. The shim asks the hub its
  /// cursor points at.
  action walletGetWith(query: TxId, start: int): bool = all {
    s.wallet.gets < MAX_REQUESTS,
    walletTxids.contains(query),
    val result = shim(s.shim, GetTxSInput({ query: query, start: start }))
    all {
      not(isShimError(result.out)),
      commit(s.countGet().shimStepped(result, None), WalletGet({ query: query, start: start })),
    },
  }

  action walletGet = all {
    s.wallet.gets < MAX_REQUESTS,
    walletTxids != Set(),
    {
      nondet query = oneOf(walletTxids)
      nondet start = oneOf(HUB_ORDER.indices())
      walletGetWith(query, start)
    },
  }

  // ------------------------------------------------------------------------
  // Shim
  // ------------------------------------------------------------------------

  /// The frames whose delivery to the shim would do something: replies under
  /// a nonce it is waiting on. Any other frame is dropped without effect.
  def shimDeliverable: Set[Mail] =
    s.net.inbox(ShimAddr).filter(mail =>
      match nonceOf(FrameSInput(mail.msg)) {
        | Some(nonce) => s.shim.hasWaiter(nonce)
        | None => false
      })

  /// The network delivers a frame to the shim.
  action shimReceiveWith(mail: Mail): bool = all {
    shimDeliverable.contains(mail),
    val result = shim(s.shim, FrameSInput(mail.msg))
    all {
      not(isShimError(result.out)),
      commit(s.shimStepped(result, nonceOf(FrameSInput(mail.msg))), ShimReceive(mail)),
    },
  }

  action shimReceive = all {
    shimDeliverable != Set(),
    {
      nondet mail = oneOf(shimDeliverable)
      shimReceiveWith(mail)
    },
  }

  /// The shim gives up waiting for a lookup reply.
  action shimLookupTimeoutWith(nonce: Nonce): bool = all {
    s.shim.hasWaiter(nonce),
    val result = shim(s.shim, LookupTimeoutSInput(nonce))
    all {
      not(isShimError(result.out)),
      commit(s.shimStepped(result, None), ShimLookupTimeout(nonce)),
    },
  }

  action shimLookupTimeout = all {
    s.shim.waiters.keys() != Set(),
    {
      nondet nonce = oneOf(s.shim.waiters.keys())
      shimLookupTimeoutWith(nonce)
    },
  }

  // ------------------------------------------------------------------------
  // Hub
  // ------------------------------------------------------------------------

  /// The request frames that may be delivered to `id`: all of them, as often
  /// as the network likes, while the hub is serving.
  def hubDeliverable(id: HubId): Set[Mail] =
    if (s.hubs.get(id).isServing())
      s.net.inbox(HubAddr(id)).filter(mail => requestInput(mail.msg, INotFound) != Set())
    else Set()

  /// The transitions `id` may take when `mail` is delivered and its indexer
  /// would answer a lookup with `answer`.
  def hubReceipts(id: HubId, mail: Mail, answer: IndexerAnswer): Set[HubResult] =
    requestInput(mail.msg, answer).map(input => hubResults(id, s.hubs.get(id), input)).flatten()

  /// The network delivers a request to a hub, which answers its sender. For a
  /// lookup, `answer` is what the hub's indexer says if the queue misses.
  action hubReceiveWith(id: HubId, mail: Mail, answer: IndexerAnswer, result: HubResult): bool = all {
    HUBS.contains(id),
    hubDeliverable(id).contains(mail),
    lookupAnswers(s.indexer, mail.msg).contains(answer),
    hubReceipts(id, mail, answer).contains(result),
    not(isHubError(result.out)),
    commit(s.hubReplied(id, mail.src, result), HubReceive({ hub: id, mail: mail })),
  }

  action hubReceive = {
    nondet id = oneOf(HUBS)
    all {
      hubDeliverable(id) != Set(),
      {
        nondet mail = oneOf(hubDeliverable(id))
        nondet answer = oneOf(lookupAnswers(s.indexer, mail.msg))
        nondet result = oneOf(hubReceipts(id, mail, answer))
        hubReceiveWith(id, mail, answer, result)
      },
    }
  }

  /// A step of the hub's own schedule, taken in the system `around` (the
  /// current one, or the current one after its indexer moved). A Byzantine hub
  /// keeps the honest schedule, so these take the hub function's transition
  /// whatever the role. A step that would change nothing is not taken.
  action hubTakesIn(around: System, id: HubId, input: HubInput, label: Label): bool =
    val result = hub(s.hubs.get(id), input)
    all {
      HUBS.contains(id),
      not(isHubError(result.out)),
      result.state != s.hubs.get(id),
      commit(around.hubMoved(id, result.state), label),
    }

  action hubTakes(id: HubId, input: HubInput, label: Label): bool =
    hubTakesIn(s, id, input, label)

  /// A hub's cadence loop observes the tip its indexer reports.
  ///
  /// The poll is recorded whether or not the answer moves the hub's tip: a hub
  /// that asked and was told nothing new has still asked. A second poll in the
  /// same block that changes nothing is not taken.
  action hubObserveTipWith(id: HubId, tip: Height): bool =
    val result = hub(s.hubs.get(id), TipHInput(tip))
    all {
      HUBS.contains(id),
      reportableTips.contains(tip),
      not(isHubError(result.out)),
      result.state != s.hubs.get(id) or not(s.polled.contains(id)),
      commit(
        { ...s.hubMoved(id, result.state), polled: s.polled.union(Set(id)) },
        HubObserveTip({ hub: id, tip: tip }),
      ),
    }

  action hubObserveTip = {
    nondet id = oneOf(HUBS)
    nondet tip = oneOf(reportableTips)
    hubObserveTipWith(id, tip)
  }

  /// A hub that has seen no tip progress for the staleness window goes stale,
  /// or, already stale, reads its free-running clock again.
  action hubTipStaleWith(id: HubId, estimate: Height): bool = all {
    TIP == TipMayLag,
    HUBS.contains(id),
    s.hubs.get(id).phase == Stale or lagOf(id) >= STALE_WINDOW,
    freeRunEstimates(id).contains(estimate),
    hubTakes(id, StaleHInput(estimate), HubTipStale({ hub: id, estimate: estimate })),
  }

  action hubTipStale = {
    nondet id = oneOf(HUBS)
    nondet estimate = oneOf(freeRunEstimates(id))
    hubTipStaleWith(id, estimate)
  }

  /// A flush begins: the hub's whole queue goes out at once.
  action hubFlushBeginWith(id: HubId): bool =
    hubTakes(id, FlushDueHInput, HubFlushBegin(id))

  action hubFlushBegin = {
    nondet id = oneOf(HUBS)
    hubFlushBeginWith(id)
  }

  /// The verdict an indexer output carries. Read only where it carries one.
  pure def verdictIn(output: IndexerOutput): Verdict =
    match output {
      | VerdictOutput(verdict) => verdict
      | _ => Retryable
    }

  /// The indexer returns its verdict on one entry of a batch. `result` is the
  /// indexer's transition: the verdict, and what became of the transaction.
  action indexerVerdictWith(id: HubId, payload: Payload, result: IndexerResult): bool = all {
    indexerResults(s.indexer, BroadcastIInput(payload)).contains(result),
    result.out == VerdictOutput(verdictIn(result.out)),
    hubTakesIn(
      { ...s, indexer: result.state },
      id,
      VerdictHInput({ payload: payload, verdict: verdictIn(result.out) }),
      IndexerVerdict({ hub: id, payload: payload }),
    ),
  }

  /// The entries of `id`'s batch still waiting for a verdict.
  def awaitingVerdict(id: HubId): Set[Payload] =
    match s.hubs.get(id).flush {
      | Broadcasting(flush) => flush.batch.keys()
      | Idle => Set()
    }

  action indexerVerdict = {
    nondet id = oneOf(HUBS)
    all {
      awaitingVerdict(id) != Set(),
      {
        nondet payload = oneOf(awaitingVerdict(id))
        nondet result = oneOf(indexerResults(s.indexer, BroadcastIInput(payload)))
        indexerVerdictWith(id, payload, result)
      },
    }
  }

  /// A flush ends: what nothing judged is requeued or dropped.
  action hubFlushEndWith(id: HubId): bool =
    hubTakes(id, FlushDoneHInput, HubFlushEnd(id))

  action hubFlushEnd = {
    nondet id = oneOf(HUBS)
    hubFlushEndWith(id)
  }

  /// A hub gets its shutdown signal.
  action hubBeginDrainWith(id: HubId): bool =
    hubTakes(id, DrainHInput, HubBeginDrain(id))

  action hubBeginDrain = {
    nondet id = oneOf(HUBS)
    hubBeginDrainWith(id)
  }

  /// A hub's process dies, or exits after its final flush.
  action hubCrashWith(id: HubId): bool =
    hubTakes(id, CrashHInput, HubCrash(id))

  action hubCrash = {
    nondet id = oneOf(HUBS)
    hubCrashWith(id)
  }

  action hubRestartWith(id: HubId): bool =
    hubTakes(id, RestartHInput, HubRestart(id))

  action hubRestart = {
    nondet id = oneOf(HUBS)
    hubRestartWith(id)
  }

  // ------------------------------------------------------------------------
  // Chain
  // ------------------------------------------------------------------------

  /// The chain grows by one block.
  action chainAdvance = all {
    chainMayAdvance,
    commit(s.blockArrived(), ChainAdvance),
  }

  /// The transactions waiting in the mempool.
  def mempool: Set[TxId] =
    s.indexer.txs.keys().filter(txid => s.onChain(txid) == InMempool)

  /// A mempool transaction is included in the current block.
  action chainMineWith(txid: TxId): bool = all {
    mempool.contains(txid),
    commit({ ...s, indexer: indexerApply(s.indexer, MineIInput(txid), NoIndexerOutput) }, ChainMine(txid)),
  }

  action chainMine = all {
    mempool != Set(),
    {
      nondet txid = oneOf(mempool)
      chainMineWith(txid)
    },
  }

  // ------------------------------------------------------------------------
  // Third party
  // ------------------------------------------------------------------------

  /// The txids of wallet transactions the third party has yet to learn.
  def unknownTxids: Set[TxId] =
    txidsOf(PAYLOADS).exclude(s.tpTxids())

  /// The third party learns a txid out of band, before the transaction is
  /// published. An operator can recover one from a wallet's transparent-pool
  /// queries.
  action thirdPartyLearnsTxidWith(txid: TxId): bool = all {
    unknownTxids.contains(txid),
    commit(
      { ...s, thirdParty: { ...s.thirdParty, txids: s.thirdParty.txids.union(Set(txid)) } },
      ThirdPartyLearnsTxid(txid),
    ),
  }

  action thirdPartyLearnsTxid = all {
    unknownTxids != Set(),
    {
      nondet txid = oneOf(unknownTxids)
      thirdPartyLearnsTxidWith(txid)
    },
  }

  /// The third party asks a hub about a txid it knows. The lookup is not
  /// authenticated.
  action thirdPartyLookupWith(txid: TxId, id: HubId): bool = all {
    s.thirdParty.requests < MAX_REQUESTS,
    HUBS.contains(id),
    s.tpTxids().contains(txid),
    commit(
      s.thirdPartySent(id, Lookup({ nonce: s.thirdParty.nextNonce, txid: txid })),
      ThirdPartyLookup({ txid: txid, hub: id }),
    ),
  }

  action thirdPartyLookup = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpTxids() != Set(),
    {
      nondet txid = oneOf(s.tpTxids())
      nondet id = oneOf(HUBS)
      thirdPartyLookupWith(txid, id)
    },
  }

  /// The third party submits a payload it has: one it made, or one it has
  /// learned. Submission is not authenticated either.
  action thirdPartySubmitWith(payload: Payload, id: HubId): bool = all {
    s.thirdParty.requests < MAX_REQUESTS,
    HUBS.contains(id),
    s.tpPayloads().contains(payload),
    commit(
      s.thirdPartySent(id, Submit({ nonce: s.thirdParty.nextNonce, payload: payload })),
      ThirdPartySubmit({ payload: payload, hub: id }),
    ),
  }

  action thirdPartySubmit = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpPayloads() != Set(),
    {
      nondet payload = oneOf(s.tpPayloads())
      nondet id = oneOf(HUBS)
      thirdPartySubmitWith(payload, id)
    },
  }

  /// What a Byzantine component could still reveal.
  def undisclosed: Set[Payload] =
    s.disclosable(ROLES).exclude(s.disclosed)

  /// A Byzantine component reveals a payload it has seen.
  action byzDiscloseWith(payload: Payload): bool = all {
    undisclosed.contains(payload),
    commit({ ...s, disclosed: s.disclosed.union(Set(payload)) }, ByzDisclose(payload)),
  }

  action byzDisclose = all {
    undisclosed != Set(),
    {
      nondet payload = oneOf(undisclosed)
      byzDiscloseWith(payload)
    },
  }

  // ------------------------------------------------------------------------
  // Step
  // ------------------------------------------------------------------------

  /// A fault: a timeout, a shutdown, a crash, a restart.
  action faultStep = any {
    shimLookupTimeout, hubBeginDrain, hubCrash, hubRestart,
  }

  /// A step by someone outside the protocol: the third party, or a Byzantine
  /// component revealing what it has seen.
  action outsiderStep = any {
    thirdPartyLearnsTxid, thirdPartyLookup, thirdPartySubmit, byzDisclose,
  }

  /// One step of the system.
  ///
  /// Faults and outsiders are grouped so that the simulator, which chooses
  /// uniformly among the alternatives it is given, takes one of each about as
  /// often as it takes any single step of the protocol.
  action step = any {
    walletSend, walletGet,
    shimReceive,
    hubReceive, hubObserveTip, hubTipStale, hubFlushBegin, indexerVerdict, hubFlushEnd,
    chainAdvance, chainMine,
    faultStep,
    outsiderStep,
  }

  /// The indexer cannot be reached: an entry of a batch gets no verdict.
  action indexerUnreachable = {
    nondet id = oneOf(HUBS)
    all {
      awaitingVerdict(id) != Set(),
      {
        nondet payload = oneOf(awaitingVerdict(id))
        indexerVerdictWith(id, payload, {
          state: indexerApply(s.indexer, BroadcastIInput(payload), VerdictOutput(Retryable)),
          out: VerdictOutput(Retryable),
        })
      },
    }
  }

  // The relations below are parts of `step`. Everything one of them reaches,
  // `step` reaches, so they are sound for showing that a state is reachable
  // and for nothing else. They keep a simulation on one part of the behaviour
  // long enough to get deep into it.

  /// The protocol with no faults and no outsiders.
  action quietStep = any {
    walletSend, walletGet,
    shimReceive,
    hubReceive, hubObserveTip, hubTipStale, hubFlushBegin, indexerVerdict, hubFlushEnd,
    chainAdvance, chainMine,
  }

  /// The protocol during an indexer outage: every flush comes back unjudged,
  /// while outsiders go on submitting.
  action outageStep = any {
    walletSend, walletGet,
    shimReceive,
    hubReceive, hubObserveTip, hubTipStale, hubFlushBegin, indexerUnreachable, hubFlushEnd,
    chainAdvance,
    outsiderStep,
  }

  // ------------------------------------------------------------------------
  // Guarantees
  // ------------------------------------------------------------------------
  //
  // The predicates are defined, and documented, in `properties.qnt`, each as
  // `<name>In(..)`: a predicate over a system and an audit record. These are
  // their values in the current state, under the names the gate checks.

  val operatorBlind = operatorBlindIn(s)
  val queuedBytesConfidential = queuedBytesConfidentialIn(s)
  val txidAuthenticity = txidAuthenticityIn(s)
  val lookupValidityPerHub = lookupValidityPerHubIn(s, audit)
  val offeredBeforeExpiry = offeredBeforeExpiryIn(s, audit, HUBS)
  val conformingFirstOfferBeforeExpiry = conformingFirstOfferBeforeExpiryIn(s, audit, HUBS)
  val conformingFirstOfferJudgedBeforeExpiry = conformingFirstOfferJudgedBeforeExpiryIn(s, audit, HUBS)
  val wellFormed = wellFormedIn(s)
  val ackImpliesQueued = ackImpliesQueuedIn(s, audit, HUBS)

  // The per-hub guarantees, claimed of the honest hubs only. Each is the same
  // predicate as its namesake above, restricted to the hubs whose role is
  // `Honest`; it is not a weaker property. With every hub honest the two
  // coincide.
  val offeredBeforeExpiryForHonestHubs = offeredBeforeExpiryIn(s, audit, HONEST_HUBS)
  val conformingFirstOfferBeforeExpiryForHonestHubs = conformingFirstOfferBeforeExpiryIn(s, audit, HONEST_HUBS)
  val conformingFirstOfferJudgedBeforeExpiryForHonestHubs =
    conformingFirstOfferJudgedBeforeExpiryIn(s, audit, HONEST_HUBS)
  val ackImpliesQueuedForHonestHubs = ackImpliesQueuedIn(s, audit, HONEST_HUBS)

  // ------------------------------------------------------------------------
  // Known gaps: invariants that do not hold
  // ------------------------------------------------------------------------

  val statusNeverRegresses = statusNeverRegressesIn(s)
  val ackedIsHeldOrSettled = ackedIsHeldOrSettledIn(s, audit)
  val conformingEveryOfferBeforeExpiry = conformingEveryOfferBeforeExpiryIn(s, audit)

  // ------------------------------------------------------------------------
  // Witnesses
  // ------------------------------------------------------------------------

  val wToldRefusedEverywhere = wToldRefusedEverywhereIn(s, audit)
  val wToldNeverDelivered = wToldNeverDeliveredIn(s, audit)
  val wToldPrefixOnly = wToldPrefixOnlyIn(s)

  val wPending = wPendingIn(s)
  val wTxInMempool = wTxInMempoolIn(s)
  val wTxMined = wTxMinedIn(s)
  val wRefusedTipStale = wRefused(audit, TipStale)
  val wRefusedDraining = wRefused(audit, HubDraining)
  val wRefusedTooLarge = wRefused(audit, TooLarge)
  val wRefusedExpiryTooTight = wRefused(audit, ExpiryTooTight)
  val wRefusedFull = wRefused(audit, Full)
  val wRequeued = wRequeuedIn(s)
  val wDroppedExpired = wDroppedExpiredIn(s, audit)
  val wDroppedExhausted = wDroppedExhaustedIn(audit)
  val wQueuedDisclosed = wQueuedDisclosedIn(s)
  val wUnparseableMissed = wUnparseableMissedIn(s)
  val wQueueOverCapacity = wQueueOverCapacityIn(s)
  val wFailoverAnswered = wFailoverAnsweredIn(s)
  val wPublishedByTwoHubs = wPublishedByTwoHubsIn(s, audit)
  val wPrematureFlush = wPrematureFlushIn(s)
  val wTwinServed = wTwinServedIn(s)
  val wFalseHeightServed = wFalseHeightServedIn(s)
  val wThirdPartyPayloadQueued = wThirdPartyPayloadQueuedIn(s)
  val wEarlyFreeRunFlush = wEarlyFreeRunFlushIn(s)

  // Non-vacuity: the antecedent of each guarantee is reachable.
  val vOperatorBlind = vOperatorBlindIn(s)
  val vQueuedBytesConfidential = vQueuedBytesConfidentialIn(s)
  val vTxidAuthenticity = vTxidAuthenticityIn(s)
  val vLookupValidityPerHub = vLookupValidityPerHubIn(s)
  val vOfferedBeforeExpiry = vOfferedBeforeExpiryIn(audit)
  val vConformingFirstOfferBeforeExpiry = vConformingFirstOfferBeforeExpiryIn(s, audit)
  val vConformingOfferAdmittedBehind = vConformingOfferAdmittedBehindIn(s, audit)
  val vConformingFirstOfferJudged = vConformingFirstOfferJudgedIn(s, audit)
  val vAckImpliesQueued = vAckImpliesQueuedIn(s)

  // ------------------------------------------------------------------------
  // Two-state properties
  // ------------------------------------------------------------------------
  //
  // These relate a state to its successor, which an invariant cannot. The
  // simulator does not check them. `next` has to wrap a complete expression
  // that reads the state, which is why they are written out here and not as
  // predicates over a pair of systems.

  /// How far along the chain a transaction is.
  def chainRank(txid: TxId): int =
    match s.onChain(txid) {
      | Absent => 0
      | InMempool => 1
      | MinedAt(_) => 2
    }

  def chainStatus(txid: TxId): Inclusion = s.onChain(txid)
  def queueOf(id: HubId): Set[Payload] = s.queuedAt(id)
  def flightOf(id: HubId): Set[Payload] = s.inFlightAt(id)
  def phaseOf(id: HubId): Phase = s.hubs.get(id).phase

  /// A1. A transaction's status on the chain never moves backwards, and a
  /// mined transaction stays where it was mined. An assumption about the
  /// environment, true by construction of the chain steps.
  temporal chainMonotone = always(
    TXIDS.forall(txid =>
      and {
        chainRank(txid) <= next(chainRank(txid)),
        chainRank(txid) == 2 implies next(chainStatus(txid)) == chainStatus(txid),
      }
    ).orKeep(s)
  )

  /// A2. An entry leaves a hub's queue only into a flush, or because the hub
  /// went down. Nothing evicts it.
  temporal neverEvict = always(
    HUBS.forall(id =>
      or {
        queueOf(id).subseteq(next(queueOf(id)).union(next(flightOf(id)))),
        next(phaseOf(id)) == Down,
      }
    ).orKeep(s)
  )

  /// A3. A draining honest hub admits nothing: its queue gains only what a
  /// flush hands back. Draining is an admission rule like the others, so a
  /// Byzantine hub is not bound by it (`hubAdmitsWhileDrainingTest`).
  temporal drainIsFinal = always(
    HONEST_HUBS.forall(id =>
      phaseOf(id) == Draining implies next(queueOf(id)).subseteq(queueOf(id).union(flightOf(id)))
    ).orKeep(s)
  )

  // ------------------------------------------------------------------------
  // Run vocabulary
  // ------------------------------------------------------------------------
  //
  // Shorthands for scripted runs. Each is a `...With` step with the choices an
  // honest component and a truthful, reachable indexer would make, or a short
  // sequence of such steps. A run that needs another choice (a Byzantine
  // transition, an indexer that is down) uses the `...With` step itself.

  /// A submission of `payload` from the shim to `id` under `nonce`.
  pure def submitMail(id: HubId, nonce: Nonce, payload: Payload): Mail =
    { src: ShimAddr, dst: HubAddr(id), msg: Submit({ nonce: nonce, payload: payload }) }

  /// A lookup of `txid` from the shim to `id` under `nonce`.
  pure def lookupMail(id: HubId, nonce: Nonce, txid: TxId): Mail =
    { src: ShimAddr, dst: HubAddr(id), msg: Lookup({ nonce: nonce, txid: txid }) }

  /// The same frame, sent by the third party instead.
  pure def fromThirdParty(mail: Mail): Mail =
    { ...mail, src: ThirdPartyAddr }

  /// `id`'s ack to the shim under `nonce`.
  pure def ackMail(id: HubId, nonce: Nonce, ack: WireAck): Mail =
    { src: HubAddr(id), dst: ShimAddr, msg: Ack({ nonce: nonce, ack: ack }) }

  /// `id`'s lookup reply to the shim under `nonce`.
  pure def replyMail(id: HubId, nonce: Nonce, reply: WireReply): Mail =
    { src: HubAddr(id), dst: ShimAddr, msg: LookupReply({ nonce: nonce, reply: reply }) }

  /// The wallet sends, and the shim routes the transaction as it should.
  action sends(input: SendInput, handedOver: int): bool =
    walletSendWith(input, handedOver)

  /// The wallet sends a transaction and every hub address takes a frame.
  action sendToAll(payload: Payload): bool =
    sends(Clean(payload), HUB_ORDER.length())

  /// The wallet asks, and the shim asks the hub at `start`.
  action ask(query: TxId, start: int): bool =
    walletGetWith(query, start)

  /// `client`'s submission of `payload` under `nonce` reaches `id`, which acts
  /// as it should.
  action deliverSubmitFrom(client: Addr, id: HubId, nonce: Nonce, payload: Payload): bool =
    val submit = { nonce: nonce, payload: payload }
    hubReceiveWith(
      id,
      { src: client, dst: HubAddr(id), msg: Submit(submit) },
      INotFound,
      hub(s.hubs.get(id), SubmitHInput(submit)),
    )

  /// The shim's submission of `payload` under `nonce` reaches `id`.
  action deliverSubmit(id: HubId, nonce: Nonce, payload: Payload): bool =
    deliverSubmitFrom(ShimAddr, id, nonce, payload)

  /// `client`'s lookup of `txid` under `nonce` reaches `id`, which acts as it
  /// should on the answer `answer` from its indexer.
  action deliverLookupFrom(client: Addr, id: HubId, nonce: Nonce, txid: TxId, answer: IndexerAnswer): bool =
    hubReceiveWith(
      id,
      { src: client, dst: HubAddr(id), msg: Lookup({ nonce: nonce, txid: txid }) },
      answer,
      hub(s.hubs.get(id), LookupHInput({ nonce: nonce, txid: txid, answer: answer })),
    )

  /// The shim's lookup of `txid` under `nonce` reaches `id`, whose indexer is
  /// reachable and truthful.
  action deliverLookup(id: HubId, nonce: Nonce, txid: TxId): bool =
    deliverLookupFrom(ShimAddr, id, nonce, txid, chainAnswer(s.indexer, txid))

  /// `mail` reaches the shim, which acts as it should.
  action deliverToShim(mail: Mail): bool =
    shimReceiveWith(mail)

  /// The reply an honest `id` with a truthful indexer gives, in the current
  /// state, to a lookup of `txid`.
  def honestReply(id: HubId, txid: TxId): WireReply =
    render(if (s.hubs.get(id).isQueuedTxid(txid)) QueueHit else FromIndexer(chainAnswer(s.indexer, txid)))

  /// The shim's wait under `nonce` times out.
  action timeOutLookup(nonce: Nonce): bool =
    shimLookupTimeoutWith(nonce)

  /// An honest indexer gives `verdict` on `payload`, with the effect that
  /// verdict has.
  action judge(id: HubId, payload: Payload, verdict: Verdict): bool =
    indexerVerdictWith(id, payload, {
      state: indexerApply(s.indexer, BroadcastIInput(payload), VerdictOutput(verdict)),
      out: VerdictOutput(verdict),
    })

  /// `id` observes the true height.
  action observe(id: HubId): bool =
    hubObserveTipWith(id, s.height())

  /// Every hub observes the true height.
  run allObserve = HUB_ORDER.length().reps(i => observe(HUB_ORDER[i]))

  /// The system with every hub running at the genesis height.
  run started = init.then(allObserve)

  /// One block arrives and every hub sees it.
  run block = chainAdvance.then(allObserve)

  /// `count` blocks arrive, each seen by every hub. No flush may fall due.
  run blocks(count: int): bool = count.reps(_ => block)

  /// The wallet sends `payload`, and the frame under `nonce` reaches `id`.
  run submitTo(id: HubId, nonce: Nonce, payload: Payload): bool =
    sendToAll(payload).then(deliverSubmit(id, nonce, payload))

  /// The wallet asks for `query`; the lookup under `nonce` reaches `id`; the
  /// reply reaches the shim. The hub's state does not change in between.
  run lookUp(id: HubId, nonce: Nonce, query: TxId): bool =
    ask(query, 0)
      .then(deliverLookup(id, nonce, query))
      .then(deliverToShim(replyMail(id, nonce, honestReply(id, query))))

  /// `id` flushes `batch`, and the indexer gives every entry `verdict`.
  run flush(id: HubId, batch: List[Payload], verdict: Verdict): bool =
    hubFlushBeginWith(id)
      .then(batch.length().reps(i => judge(id, batch[i], verdict)))
      .then(hubFlushEndWith(id))

  /// The wallet's most recent answer.
  def lastEvent: WalletEvent =
    s.wallet.log[s.wallet.log.length() - 1]
}

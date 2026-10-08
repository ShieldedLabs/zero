// -*- mode: Bluespec; -*-

/// The zeronym protocol as a state machine: a wallet, a shim, one hub, the
/// chain behind the hub's indexer, an unreliable network between them, and a
/// third party that can reach the hub.
///
/// The spec checks one hub; production runs one or more, replicated: every
/// shim sends every submission to every hub, and each hub that receives a
/// migration queues and broadcasts it.
///
/// What is assumed:
///
/// - Roles. The shim and the hub run in enclaves and are honest in the
///   baseline. The shim is honest in every configuration; the hub and its
///   indexer can each be made Byzantine through `ROLES`. A Byzantine
///   component draws its transitions from a wider relation and is not marked
///   in any other way.
/// - Network. Frames may be lost, duplicated, delayed and reordered. They
///   cannot be forged or read in transit.
/// - Third party. A client of the hub's public address. It looks up txids it
///   knows and submits payloads it has learned or made. It cannot read or
///   forge frames, so it does not know a nonce and cannot answer the shim.
/// - Nonces are unique. A counter stands for an unguessable random value.
/// - Chain. A transaction's status only moves forward: no reorg of an
///   included transaction, no mempool eviction.
/// - Hub. The hub is abstract (`abstractHub.qnt`): a queue, the entries out
///   with a flush, and its replies, with no schedule. `hubTest` checks that
///   the real hub function refines it. Its timing is the hub specification's.
/// - Time. There is no clock. A timeout is an event that may happen at any
///   moment.
///
/// How it is built. This module holds no protocol logic. Every step picks an
/// input, hands it to one component (`shim`, the abstract hub, or the indexer
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
  import abstractHub.* from "./abstractHub"
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

  /// The bound of the model: the wallet makes at most `MAX_REQUESTS` sends
  /// and as many lookups, as does the third party.
  pure val MAX_REQUESTS = CONFIG.maxRequests

  pure val ROLES = CONFIG.roles

  /// Every payload that exists. A Byzantine component builds its lies from it.
  pure val UNIVERSE = PAYLOADS.union(TWINS).union(TP_PAYLOADS)
  /// What a wallet may send.
  pure val SEND_INPUTS = PAYLOADS.map(payload => Clean(payload)).union(Set(Unreadable, EmptyBody))
  /// Every transaction id there is.
  pure val TXIDS = txidsOf(UNIVERSE)

  // ------------------------------------------------------------------------
  // Assumptions
  // ------------------------------------------------------------------------
  //
  // A named value, so that an instance's `assumptionsTest` can assert it: the
  // simulator does not enforce `assume`.

  /// Payloads are told apart by their bytes; a twin is a twin of something a
  /// wallet sends; the third party's payloads are its own.
  pure val payloadsWellFormed = and {
    UNIVERSE.size() == UNIVERSE.map(payload => payload.id).size(),
    PAYLOADS.intersect(TP_PAYLOADS) == Set(),
    TWINS.forall(twin => PAYLOADS.exists(payload => areTwins(twin, payload))),
  }

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

  pure val INITIAL = initialSystem(CONFIG)

  action init = all {
    s' = INITIAL,
    audit' = initialAudit,
    lastAction' = Init,
  }

  // ------------------------------------------------------------------------
  // Roles
  // ------------------------------------------------------------------------

  pure def hubAnswers(h: AHub, request: ARequest): Set[AAnswer] =
    match ROLES.hub {
      | Honest => honestAnswers(h, request)
      | Byzantine => byzantineAnswers(h, request, UNIVERSE)
    }

  pure def indexerResults(state: IndexerState, input: IndexerInput): Set[IndexerResult] =
    match ROLES.indexer {
      | Honest => heightlessIndexerResults(state, input)
      | Byzantine => byzIndexerResults(state, input, UNIVERSE)
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

  // ------------------------------------------------------------------------
  // Wallet
  // ------------------------------------------------------------------------

  /// The wallet sends a transaction, and the shim routes it and answers.
  /// `handedOver` is whether the transport takes the frame for the hub.
  action walletSendWith(input: SendInput, handedOver: bool): bool = all {
    s.wallet.sends < MAX_REQUESTS,
    SEND_INPUTS.contains(input),
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
      nondet handedOver = oneOf(Set(true, false))
      walletSendWith(input, handedOver)
    },
  }

  /// The txids of the transactions the wallet has handed the shim. A wallet
  /// asks only about its own transactions.
  def walletTxids: Set[TxId] =
    s.sentByWallet().map(payload => walletTxid(payload))

  /// The wallet asks for a transaction it has sent. The shim asks the hub.
  action walletGetWith(query: TxId): bool = all {
    s.wallet.gets < MAX_REQUESTS,
    walletTxids.contains(query),
    val result = shim(s.shim, GetTxSInput(query))
    all {
      not(isShimError(result.out)),
      commit(s.countGet().shimStepped(result, None), WalletGet(query)),
    },
  }

  action walletGet = all {
    s.wallet.gets < MAX_REQUESTS,
    walletTxids != Set(),
    {
      nondet query = oneOf(walletTxids)
      walletGetWith(query)
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

  /// The request frames that may be delivered to the hub: all of them, as
  /// often as the network likes.
  def hubDeliverable: Set[Mail] =
    s.net.inbox(HubAddr).filter(mail => requestOf(mail.msg, INotFound) != Set())

  /// What the hub may answer when `mail` is delivered and its indexer would
  /// answer a lookup with `answer`.
  def hubReceipts(mail: Mail, answer: IndexerAnswer): Set[AAnswer] =
    requestOf(mail.msg, answer).map(request => hubAnswers(s.hub, request)).flatten()

  /// The network delivers a request to the hub, which answers its sender. For
  /// a lookup, `answer` is what the hub's indexer says if the queue misses.
  action hubReceiveWith(mail: Mail, answer: IndexerAnswer, result: AAnswer): bool = all {
    hubDeliverable.contains(mail),
    lookupAnswers(s.indexer, mail.msg).contains(answer),
    hubReceipts(mail, answer).contains(result),
    commit(s.hubReplied(mail, result), HubReceive(mail)),
  }

  action hubReceive = all {
    hubDeliverable != Set(),
    {
      nondet mail = oneOf(hubDeliverable)
      nondet answer = oneOf(lookupAnswers(s.indexer, mail.msg))
      nondet result = oneOf(hubReceipts(mail, answer))
      hubReceiveWith(mail, answer, result)
    },
  }

  /// A flush begins: the whole queue goes out at once. A Byzantine hub keeps
  /// the schedule, so these steps are the same whatever its role.
  action hubTake = all {
    s.hub.queue != Set(),
    commit({ ...s, hub: s.hub.take() }, HubTake),
  }

  /// The verdict an indexer output carries. Read only where it carries one.
  pure def verdictIn(output: IndexerOutput): Verdict =
    match output {
      | VerdictOutput(verdict) => verdict
      | _ => Retryable
    }

  /// The indexer returns its verdict on one entry out with a flush. `result`
  /// is the indexer's transition: the verdict, and what became of the
  /// transaction. A node's verdict settles the entry; a retryable one leaves it
  /// out with the flush.
  action indexerVerdictWith(payload: Payload, result: IndexerResult): bool = all {
    s.hub.held.contains(payload),
    indexerResults(s.indexer, BroadcastIInput(payload)).contains(result),
    result.out == VerdictOutput(verdictIn(result.out)),
    val hub = if (verdictIn(result.out) == Retryable) s.hub else s.hub.settle(payload)
    commit({ ...s, indexer: result.state, hub: hub }, IndexerVerdict(payload)),
  }

  action indexerVerdict = all {
    s.hub.held != Set(),
    {
      nondet payload = oneOf(s.hub.held)
      nondet result = oneOf(indexerResults(s.indexer, BroadcastIInput(payload)))
      indexerVerdictWith(payload, result)
    },
  }

  /// A flush ends: of what nothing judged, `kept` goes back into the queue and
  /// the rest is dropped.
  action hubReturnWith(kept: Set[Payload]): bool = all {
    s.hub.held != Set(),
    kept.subseteq(s.hub.held),
    commit({ ...s, hub: s.hub.giveBack(kept) }, HubReturn(kept)),
  }

  action hubReturn = all {
    s.hub.held != Set(),
    {
      nondet kept = oneOf(s.hub.held.powerset())
      hubReturnWith(kept)
    },
  }

  /// The hub loses everything: its process dies, or exits after its final
  /// flush.
  action hubLose = all {
    s.hub != emptyAHub,
    commit({ ...s, hub: emptyAHub }, HubLose),
  }

  // ------------------------------------------------------------------------
  // Chain
  // ------------------------------------------------------------------------

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

  /// The third party asks the hub about a txid it knows. The lookup is not
  /// authenticated.
  action thirdPartyLookupWith(txid: TxId): bool = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpTxids().contains(txid),
    commit(s.thirdPartySent(Lookup({ nonce: s.thirdParty.nextNonce, txid: txid })), ThirdPartyLookup(txid)),
  }

  action thirdPartyLookup = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpTxids() != Set(),
    {
      nondet txid = oneOf(s.tpTxids())
      thirdPartyLookupWith(txid)
    },
  }

  /// The third party submits a payload it has: one it made, or one it has
  /// learned. Submission is not authenticated either.
  action thirdPartySubmitWith(payload: Payload): bool = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpPayloads().contains(payload),
    commit(
      s.thirdPartySent(Submit({ nonce: s.thirdParty.nextNonce, payload: payload })),
      ThirdPartySubmit(payload),
    ),
  }

  action thirdPartySubmit = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpPayloads() != Set(),
    {
      nondet payload = oneOf(s.tpPayloads())
      thirdPartySubmitWith(payload)
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

  /// A fault: a timeout, or the hub losing what it holds.
  action faultStep = any {
    shimLookupTimeout, hubLose,
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
    hubReceive, hubTake, indexerVerdict, hubReturn,
    chainMine,
    faultStep,
    outsiderStep,
  }

  // The relations below are parts of `step`. Everything one of them reaches,
  // `step` reaches, so they are sound for showing that a state is reachable
  // and for nothing else. They keep a simulation on one part of the behaviour
  // long enough to get deep into it.

  /// The protocol with no faults and no outsiders.
  action quietStep = any {
    walletSend, walletGet,
    shimReceive,
    hubReceive, hubTake, indexerVerdict, hubReturn,
    chainMine,
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
  val wellFormed = wellFormedIn(s)
  val ackImpliesQueued = ackImpliesQueuedIn(s, audit)

  // ------------------------------------------------------------------------
  // Known gaps: invariants that do not hold
  // ------------------------------------------------------------------------

  val statusNeverRegresses = statusNeverRegressesIn(s)

  // ------------------------------------------------------------------------
  // Witnesses
  // ------------------------------------------------------------------------

  val wToldRefusedEverywhere = wToldRefusedEverywhereIn(s, audit)
  val wToldNeverDelivered = wToldNeverDeliveredIn(s, audit)

  val wPending = wPendingIn(s)
  val wTxInMempool = wTxInMempoolIn(s)
  val wTxMined = wTxMinedIn(s)
  val wQueuedDisclosed = wQueuedDisclosedIn(s)
  val wUnparseableMissed = wUnparseableMissedIn(s)
  val wTwinServed = wTwinServedIn(s)
  val wFalseHeightServed = wFalseHeightServedIn(s)
  val wThirdPartyPayloadQueued = wThirdPartyPayloadQueuedIn(s)

  // Non-vacuity: the antecedent of each guarantee is reachable.
  val vOperatorBlind = vOperatorBlindIn(s)
  val vQueuedBytesConfidential = vQueuedBytesConfidentialIn(s)
  val vTxidAuthenticity = vTxidAuthenticityIn(s)
  val vLookupValidityPerHub = vLookupValidityPerHubIn(s)
  val vAckImpliesQueued = vAckImpliesQueuedIn(s)

  // ------------------------------------------------------------------------
  // Run vocabulary
  // ------------------------------------------------------------------------
  //
  // Shorthands for scripted runs. Each is a `...With` step with the choices an
  // honest component and a truthful, reachable indexer would make, or a short
  // sequence of such steps. A run that needs another choice (a Byzantine
  // transition, an indexer that is down) uses the `...With` step itself.

  /// A submission of `payload` from the shim to the hub under `nonce`.
  pure def submitMail(nonce: Nonce, payload: Payload): Mail =
    { src: ShimAddr, dst: HubAddr, msg: Submit({ nonce: nonce, payload: payload }) }

  /// A lookup of `txid` from the shim to the hub under `nonce`.
  pure def lookupMail(nonce: Nonce, txid: TxId): Mail =
    { src: ShimAddr, dst: HubAddr, msg: Lookup({ nonce: nonce, txid: txid }) }

  /// The same frame, sent by the third party instead.
  pure def fromThirdParty(mail: Mail): Mail =
    { ...mail, src: ThirdPartyAddr }

  /// The hub's ack to the shim under `nonce`.
  pure def ackMail(nonce: Nonce, ack: WireAck): Mail =
    { src: HubAddr, dst: ShimAddr, msg: Ack({ nonce: nonce, ack: ack }) }

  /// The hub's lookup reply to the shim under `nonce`.
  pure def replyMail(nonce: Nonce, reply: WireReply): Mail =
    { src: HubAddr, dst: ShimAddr, msg: LookupReply({ nonce: nonce, reply: reply }) }

  /// The wallet sends, and the shim routes the transaction as it should.
  action sends(input: SendInput, handedOver: bool): bool =
    walletSendWith(input, handedOver)

  /// The wallet sends a transaction and the transport takes the frame.
  action sendToHub(payload: Payload): bool =
    sends(Clean(payload), true)

  /// The wallet asks, and the shim asks the hub.
  action ask(query: TxId): bool =
    walletGetWith(query)

  /// `client`'s submission of `payload` under `nonce` reaches the hub, which
  /// answers `ack`, and queues the payload if it accepts it.
  action answerSubmitFrom(client: Addr, nonce: Nonce, payload: Payload, ack: WireAck): bool =
    hubReceiveWith(
      { src: client, dst: HubAddr, msg: Submit({ nonce: nonce, payload: payload }) },
      INotFound,
      { hub: if (ack == WAccepted) { ...s.hub, queue: s.hub.queue.union(Set(payload)) } else s.hub, reply: AAck(ack) },
    )

  /// The same, accepted.
  action deliverSubmitFrom(client: Addr, nonce: Nonce, payload: Payload): bool =
    answerSubmitFrom(client, nonce, payload, WAccepted)

  /// The shim's submission of `payload` under `nonce` reaches the hub.
  action deliverSubmit(nonce: Nonce, payload: Payload): bool =
    deliverSubmitFrom(ShimAddr, nonce, payload)

  /// `client`'s lookup of `txid` under `nonce` reaches the hub, which acts as
  /// it should on the answer `answer` from its indexer.
  action deliverLookupFrom(client: Addr, nonce: Nonce, txid: TxId, answer: IndexerAnswer): bool =
    hubReceiveWith(
      { src: client, dst: HubAddr, msg: Lookup({ nonce: nonce, txid: txid }) },
      answer,
      { hub: s.hub, reply: AWire(honestReplyOn(txid, answer)) },
    )

  /// The shim's lookup of `txid` under `nonce` reaches the hub, whose indexer
  /// is reachable and truthful.
  action deliverLookup(nonce: Nonce, txid: TxId): bool =
    deliverLookupFrom(ShimAddr, nonce, txid, chainAnswer(s.indexer, txid))

  /// `mail` reaches the shim, which acts as it should.
  action deliverToShim(mail: Mail): bool =
    shimReceiveWith(mail)

  /// The reply an honest hub gives, in the current state, to a lookup of
  /// `txid` on which its indexer would answer `answer`.
  def honestReplyOn(txid: TxId, answer: IndexerAnswer): WireReply =
    render(if (s.hub.queue.exists(payload => payload.txid == Some(txid))) QueueHit else FromIndexer(answer))

  /// The same, with a truthful indexer.
  def honestReply(txid: TxId): WireReply =
    honestReplyOn(txid, chainAnswer(s.indexer, txid))

  /// The shim's wait under `nonce` times out.
  action timeOutLookup(nonce: Nonce): bool =
    shimLookupTimeoutWith(nonce)

  /// An honest indexer gives `verdict` on `payload`, with the effect that
  /// verdict has.
  action judge(payload: Payload, verdict: Verdict): bool =
    indexerVerdictWith(payload, {
      state: indexerApply(s.indexer, BroadcastIInput(payload), VerdictOutput(verdict)),
      out: VerdictOutput(verdict),
    })

  /// The wallet sends `payload`, and the frame under `nonce` reaches the hub.
  run submitTo(nonce: Nonce, payload: Payload): bool =
    sendToHub(payload).then(deliverSubmit(nonce, payload))

  /// The wallet asks for `query`; the lookup under `nonce` reaches the hub;
  /// the reply reaches the shim. The hub's state does not change in between.
  run lookUp(nonce: Nonce, query: TxId): bool =
    ask(query)
      .then(deliverLookup(nonce, query))
      .then(deliverToShim(replyMail(nonce, honestReply(query))))

  /// The hub flushes `batch`, and the indexer gives every entry `verdict`.
  run flush(batch: List[Payload], verdict: Verdict): bool =
    hubTake.then(batch.length().reps(i => judge(batch[i], verdict)))

  /// The wallet's most recent answer.
  def lastEvent: WalletEvent =
    s.wallet.log[s.wallet.log.length() - 1]
}

// -*- mode: Bluespec; -*-

/// The shim: it sits in front of an operator's indexer, forwards what is not
/// a migration, and diverts every migration, and every transaction lookup, to
/// the hubs.
///
/// Like the hub it is one total function, `shim(state, input)`. Its state is
/// only the requests it is waiting on: it keeps no record of a migration once
/// the wallet has its answer, which is why every lookup has to go to a hub.
///
/// `byzShimResults` is the wider relation a Byzantine shim draws from.
module shim {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"
  import wire.* from "./wire"

  // ------------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------------

  /// A frame on its way to a hub.
  type Frame = { hub: HubId, msg: Msg }

  /// What the shim remembers about a lookup it has sent: where the sweep over
  /// the hubs started and how many hubs it has tried. A submission leaves
  /// nothing behind: the wallet has its answer when the frames are handed
  /// over, and an ack, if one comes, is discarded.
  type Waiter = { query: TxId, start: int, attempt: int }

  /// - `hubs`: the hub addresses, in configured order.
  /// - `waiters`: the outstanding lookups, by nonce.
  /// - `nextNonce`: the source of fresh nonces. A counter stands for a random
  ///   value nobody else can guess.
  type ShimState = {
    hubs: List[HubId],
    waiters: Nonce -> Waiter,
    nextNonce: Nonce,
  }

  pure def initialShim(hubs: List[HubId]): ShimState =
    { hubs: hubs, waiters: Map(), nextNonce: 0 }

  // ------------------------------------------------------------------------
  // Inputs and outputs
  // ------------------------------------------------------------------------

  type ShimInput =
    // A wallet's `SendTransaction`. `handedOver` is how many hub addresses, in
    // order, take a frame before the transport stops taking them.
    | SendTxSInput({ input: SendInput, handedOver: int })
    // A wallet's `GetTransaction`. `start` is where the rotating cursor points:
    // the index of the hub asked first.
    | GetTxSInput({ query: TxId, start: int })
    | FrameSInput(Msg)             // a frame arrives from the network
    | LookupTimeoutSInput(Nonce)   // no reply to a lookup in time

  type ShimOutput =
    | ForwardOutput(Payload)                                      // to the operator's indexer
    // `payload` is what the wallet sent; `told` is its answer.
    | DivertedOutput({ payload: Payload, frames: Set[Frame], told: SendObs })
    | SendDoneOutput({ input: SendInput, obs: SendObs })
    | LookupSentOutput({ frame: Frame })
    | LookupDoneOutput({ query: TxId, result: LookupObs })
    | NoShimOutput
    | ShimErrorOutput(str)

  type ShimResult = Result[ShimState, ShimOutput]

  pure def toForwardOutput(state: ShimState, payload: Payload): ShimResult =
    { state: state, out: ForwardOutput(payload) }

  pure def toDivertedOutput(state: ShimState, payload: Payload, frames: Set[Frame], told: SendObs): ShimResult =
    { state: state, out: DivertedOutput({ payload: payload, frames: frames, told: told }) }

  pure def toSendDoneOutput(state: ShimState, input: SendInput, obs: SendObs): ShimResult =
    { state: state, out: SendDoneOutput({ input: input, obs: obs }) }

  pure def toLookupSentOutput(state: ShimState, frame: Frame): ShimResult =
    { state: state, out: LookupSentOutput({ frame: frame }) }

  pure def toLookupDoneOutput(state: ShimState, query: TxId, result: LookupObs): ShimResult =
    { state: state, out: LookupDoneOutput({ query: query, result: result }) }

  pure def toNoShimOutput(state: ShimState): ShimResult =
    { state: state, out: NoShimOutput }

  pure def toShimErrorOutput(state: ShimState, reason: str): ShimResult =
    { state: state, out: ShimErrorOutput(reason) }

  // ------------------------------------------------------------------------
  // Views
  // ------------------------------------------------------------------------

  /// Whether a transaction of this class is diverted. A body the shim cannot
  /// parse is treated as a migration: forwarding it is the one outcome the
  /// shim exists to prevent.
  pure def treatAsMigration(class: Class): bool =
    class != PassThrough

  pure def hasWaiter(state: ShimState, nonce: Nonce): bool =
    state.waiters.keys().contains(nonce)

  /// The hub a lookup sweep that started at `start` asks on its `attempt`-th
  /// try.
  pure def sweepTarget(state: ShimState, start: int, attempt: int): HubId =
    state.hubs[(start + attempt) % state.hubs.length()]

  // ------------------------------------------------------------------------
  // SendTransaction
  // ------------------------------------------------------------------------

  /// Divert a migration: one frame per hub address, each under a fresh nonce,
  /// to the first `handedOver` addresses. The wallet is told ok as soon as one
  /// frame has been handed over, whether or not the sweep reached the later
  /// addresses, and no ack is waited for.
  pure def divert(state: ShimState, payload: Payload, handedOver: int): ShimResult =
    val targets = state.hubs.slice(0, handedOver)
    if (targets.length() == 0)
      state.toSendDoneOutput(Clean(payload), SendUnavailable)
    else
      val frames = targets.indices().map(i =>
        { hub: targets[i], msg: Submit({ nonce: state.nextNonce + i, payload: payload }) })
      { ...state, nextNonce: state.nextNonce + targets.length() }
        .toDivertedOutput(payload, frames, SentOk)

  /// Route a `SendTransaction`. Nothing but a cleanly read pass-through
  /// transaction ever reaches the operator; everything else is diverted or
  /// fails closed.
  pure def sendTransaction(state: ShimState, input: SendInput, handedOver: int): ShimResult =
    if (handedOver < 0 or handedOver > state.hubs.length())
      state.toShimErrorOutput("more frames handed over than there are hubs")
    else
      match input {
        | Unreadable => state.toSendDoneOutput(input, SendUnavailable)
        | EmptyBody => state.toSendDoneOutput(input, SendInvalid)
        | Clean(payload) =>
          if (not(treatAsMigration(payload.class))) state.toForwardOutput(payload)
          else if (payload.oversize) state.toSendDoneOutput(input, SendTooLarge)
          else divert(state, payload, handedOver)
      }

  // ------------------------------------------------------------------------
  // GetTransaction
  // ------------------------------------------------------------------------

  /// Ask one hub, under a fresh nonce.
  pure def askHub(state: ShimState, query: TxId, start: int, attempt: int): ShimResult =
    val nonce = state.nextNonce
    { ...state,
      waiters: state.waiters.put(nonce, { query: query, start: start, attempt: attempt }),
      nextNonce: nonce + 1,
    }.toLookupSentOutput({ hub: state.sweepTarget(start, attempt), msg: Lookup({ nonce: nonce, txid: query }) })

  /// Every lookup goes to a hub, starting at the hub the cursor points at.
  pure def getTransaction(state: ShimState, query: TxId, start: int): ShimResult =
    if (state.hubs.length() == 0)
      state.toLookupDoneOutput(query, Unavailable)
    else if (start < 0 or start >= state.hubs.length())
      state.toShimErrorOutput("the cursor points at no hub")
    else
      askHub(state, query, start, 0)

  /// A lookup got no reply in time. Only a timeout moves the sweep on to the
  /// next hub; when every hub has been tried the lookup fails closed.
  pure def lookupTimeout(state: ShimState, nonce: Nonce): ShimResult =
    if (not(state.hasWaiter(nonce)))
      state.toShimErrorOutput("no request is waiting under this nonce")
    else
      val waiter = state.waiters.get(nonce)
      val forgotten = { ...state, waiters: state.waiters.mapRemove(nonce) }
      if (waiter.attempt + 1 < state.hubs.length())
        askHub(forgotten, waiter.query, waiter.start, waiter.attempt + 1)
      else
        forgotten.toLookupDoneOutput(waiter.query, Unavailable)

  // ------------------------------------------------------------------------
  // Frames from the network
  // ------------------------------------------------------------------------

  /// A frame arrives. A lookup reply is matched to its lookup by its nonce
  /// alone; under an unknown nonce it is dropped. An ack is dropped: nobody
  /// waits for one.
  pure def receive(state: ShimState, msg: Msg): ShimResult =
    match msg {
      | Ack(_) => state.toNoShimOutput()
      | LookupReply(reply) =>
        if (not(state.hasWaiter(reply.nonce))) state.toNoShimOutput()
        else
          val waiter = state.waiters.get(reply.nonce)
          { ...state, waiters: state.waiters.mapRemove(reply.nonce) }
            .toLookupDoneOutput(waiter.query, interpretReply(reply.reply, waiter.query))
      | Submit(_) => state.toShimErrorOutput("not a reply frame")
      | Lookup(_) => state.toShimErrorOutput("not a reply frame")
    }

  // ------------------------------------------------------------------------
  // The shim function
  // ------------------------------------------------------------------------

  pure def shim(state: ShimState, input: ShimInput): ShimResult =
    match input {
      | SendTxSInput(send) => sendTransaction(state, send.input, send.handedOver)
      | GetTxSInput(lookup) => getTransaction(state, lookup.query, lookup.start)
      | FrameSInput(msg) => receive(state, msg)
      | LookupTimeoutSInput(nonce) => lookupTimeout(state, nonce)
    }

  // ------------------------------------------------------------------------
  // Byzantine relation
  // ------------------------------------------------------------------------

  pure val SEND_OBSERVATIONS =
    Set(SentOk, SentToOperator, SendUnavailable, SendInvalid, SendTooLarge)

  /// Every lookup answer that can be built from `universe` and `heights`.
  pure def lookupObservations(universe: Set[Payload], heights: Set[Height]): Set[LookupObs] =
    tuples(universe, heights)
      .map(((payload, height)) => Tx({ payload: payload, height: height }))
      .union(Set(Pending, NotFound, Unavailable))

  /// The nonce a reply frame or a timeout refers to, if any.
  pure def nonceOf(input: ShimInput): Option[Nonce] =
    match input {
      | FrameSInput(msg) =>
        match msg {
          | Ack(ack) => Some(ack.nonce)
          | LookupReply(reply) => Some(reply.nonce)
          | _ => None
        }
      | LookupTimeoutSInput(nonce) => Some(nonce)
      | _ => None
    }

  /// The transitions of a Byzantine shim. It sees every transaction in the
  /// clear and is the only thing the wallet talks to, so it is free in what it
  /// tells the wallet, what it hands the operator and what it sends the hubs:
  ///
  /// - a send is answered with anything, with nothing sent; or handed to the
  ///   operator whatever its class; or answered with anything while a frame
  ///   carrying any payload of `universe` goes to any one hub;
  /// - a lookup is answered with anything, at once or when a reply or a
  ///   timeout would have resolved it.
  ///
  /// The honest transition is always a member.
  pure def byzShimResults(
    state: ShimState,
    input: ShimInput,
    universe: Set[Payload],
    heights: Set[Height],
  ): Set[ShimResult] =
    val honest = Set(shim(state, input))
    val lies = lookupObservations(universe, heights)
    match input {
      | SendTxSInput(send) =>
        val unsent = SEND_OBSERVATIONS.map(obs => state.toSendDoneOutput(send.input, obs))
        val misrouted = match send.input {
          | Clean(sent) =>
            tuples(universe, state.hubs.indices(), SEND_OBSERVATIONS).map(((payload, i, obs)) =>
              { ...state, nextNonce: state.nextNonce + 1 }.toDivertedOutput(
                sent,
                Set({ hub: state.hubs[i], msg: Submit({ nonce: state.nextNonce, payload: payload }) }),
                obs))
              .union(Set(state.toForwardOutput(sent)))
          | _ => Set()
        }
        honest.union(unsent).union(misrouted)
      | GetTxSInput(lookup) =>
        honest.union(lies.map(result => state.toLookupDoneOutput(lookup.query, result)))
      | _ =>
        match nonceOf(input) {
          | None => honest
          | Some(nonce) =>
            if (not(state.hasWaiter(nonce))) honest
            else
              val forgotten = { ...state, waiters: state.waiters.mapRemove(nonce) }
              val waiter = state.waiters.get(nonce)
              honest.union(lies.map(result => forgotten.toLookupDoneOutput(waiter.query, result)))
        }
    }
}

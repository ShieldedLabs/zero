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

  /// What the shim remembers about a request it has sent.
  ///
  /// - An ack waiter exists for every submission. Only under `AwaitVerdict` is
  ///   anyone waiting on it (`awaited`); under `DispatchOnly` the wallet has
  ///   its answer already and the ack, if it comes, is discarded.
  /// - A lookup waiter remembers where the sweep over the hubs started and how
  ///   many hubs it has tried.
  type Waiter =
    | AckWaiter({ payload: Payload, awaited: bool })
    | LookupWaiter({ query: TxId, start: int, attempt: int })

  /// - `hubs`: the hub addresses, in configured order.
  /// - `waiters`: the outstanding requests, by nonce.
  /// - `nextNonce`: the source of fresh nonces. A counter stands for a random
  ///   value nobody else can guess.
  type ShimState = {
    hubs: List[HubId],
    mode: SubmitMode,
    waiters: Nonce -> Waiter,
    nextNonce: Nonce,
  }

  pure def initialShim(hubs: List[HubId], mode: SubmitMode): ShimState =
    { hubs: hubs, mode: mode, waiters: Map(), nextNonce: 0 }

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
    | AckTimeoutSInput(Nonce)      // no ack in time

  type ShimOutput =
    | ForwardOutput(Payload)                                      // to the operator's indexer
    | DivertedOutput({ frames: Set[Frame], told: Option[SendObs] })
    | SendDoneOutput({ input: SendInput, obs: SendObs })
    | LookupSentOutput({ frame: Frame })
    | LookupDoneOutput({ query: TxId, result: LookupObs })
    | NoShimOutput
    | ShimErrorOutput(str)

  type ShimResult = Result[ShimState, ShimOutput]

  pure def toForwardOutput(state: ShimState, payload: Payload): ShimResult =
    { state: state, out: ForwardOutput(payload) }

  pure def toDivertedOutput(state: ShimState, frames: Set[Frame], told: Option[SendObs]): ShimResult =
    { state: state, out: DivertedOutput({ frames: frames, told: told }) }

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
  /// to the first `handedOver` addresses.
  ///
  /// Under `DispatchOnly` the wallet is told ok as soon as one frame has been
  /// handed over, whether or not the sweep reached the later addresses, and
  /// no ack is waited for. Under `AwaitVerdict` there is one hub, and the
  /// wallet's answer is that hub's.
  pure def divert(state: ShimState, payload: Payload, handedOver: int): ShimResult =
    val reach = match state.mode {
      | DispatchOnly => handedOver
      | AwaitVerdict => if (handedOver > 0) 1 else 0
    }
    val targets = state.hubs.slice(0, reach)
    if (targets.length() == 0)
      state.toSendDoneOutput(Clean(payload), SendUnavailable)
    else
      val awaited = state.mode == AwaitVerdict
      val frames = targets.indices().map(i =>
        { hub: targets[i], msg: Submit({ nonce: state.nextNonce + i, payload: payload }) })
      val waiting = targets.indices().fold(state.waiters, (waiters, i) =>
        waiters.put(state.nextNonce + i, AckWaiter({ payload: payload, awaited: awaited })))
      { ...state, waiters: waiting, nextNonce: state.nextNonce + targets.length() }
        .toDivertedOutput(frames, if (awaited) None else Some(SentOk))

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
      waiters: state.waiters.put(nonce, LookupWaiter({ query: query, start: start, attempt: attempt })),
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
      match state.waiters.get(nonce) {
        | AckWaiter(_) => state.toShimErrorOutput("not a lookup")
        | LookupWaiter(waiter) =>
          val forgotten = { ...state, waiters: state.waiters.mapRemove(nonce) }
          if (waiter.attempt + 1 < state.hubs.length())
            askHub(forgotten, waiter.query, waiter.start, waiter.attempt + 1)
          else
            forgotten.toLookupDoneOutput(waiter.query, Unavailable)
      }

  /// A submission got no ack in time.
  pure def ackTimeout(state: ShimState, nonce: Nonce): ShimResult =
    if (not(state.hasWaiter(nonce)))
      state.toShimErrorOutput("no request is waiting under this nonce")
    else
      match state.waiters.get(nonce) {
        | LookupWaiter(_) => state.toShimErrorOutput("not a submission")
        | AckWaiter(waiter) =>
          val forgotten = { ...state, waiters: state.waiters.mapRemove(nonce) }
          if (waiter.awaited) forgotten.toSendDoneOutput(Clean(waiter.payload), SendUnavailable)
          else forgotten.toNoShimOutput()
      }

  // ------------------------------------------------------------------------
  // Frames from the network
  // ------------------------------------------------------------------------

  /// A frame arrives. It is matched to a request by its nonce alone. A frame
  /// under an unknown nonce is dropped. A frame of the wrong kind for a known
  /// nonce is ignored and the request keeps waiting.
  pure def receive(state: ShimState, msg: Msg): ShimResult =
    match msg {
      | Ack(ack) =>
        if (not(state.hasWaiter(ack.nonce))) state.toNoShimOutput()
        else
          match state.waiters.get(ack.nonce) {
            | LookupWaiter(_) => state.toNoShimOutput()
            | AckWaiter(waiter) =>
              val forgotten = { ...state, waiters: state.waiters.mapRemove(ack.nonce) }
              if (waiter.awaited)
                forgotten.toSendDoneOutput(
                  Clean(waiter.payload),
                  if (ack.ack == WAccepted) SentOk else SentRejected)
              else forgotten.toNoShimOutput()
          }
      | LookupReply(reply) =>
        if (not(state.hasWaiter(reply.nonce))) state.toNoShimOutput()
        else
          match state.waiters.get(reply.nonce) {
            | AckWaiter(_) => state.toNoShimOutput()
            | LookupWaiter(waiter) =>
              { ...state, waiters: state.waiters.mapRemove(reply.nonce) }
                .toLookupDoneOutput(waiter.query, interpretReply(reply.reply, waiter.query))
          }
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
      | AckTimeoutSInput(nonce) => ackTimeout(state, nonce)
    }

  // ------------------------------------------------------------------------
  // Byzantine relation
  // ------------------------------------------------------------------------

  pure val SEND_OBSERVATIONS =
    Set(SentOk, SentRejected, SentToOperator, SendUnavailable, SendInvalid, SendTooLarge)

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
      | AckTimeoutSInput(nonce) => Some(nonce)
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
  ///   timeout would have resolved it;
  /// - an awaited submission is resolved with anything.
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
        val forwarded = match send.input {
          | Clean(payload) => Set(state.toForwardOutput(payload))
          | _ => Set()
        }
        val unsent = SEND_OBSERVATIONS.map(obs => state.toSendDoneOutput(send.input, obs))
        val stray = tuples(universe, state.hubs.indices(), SEND_OBSERVATIONS).map(((payload, i, obs)) =>
          { ...state, nextNonce: state.nextNonce + 1 }.toDivertedOutput(
            Set({ hub: state.hubs[i], msg: Submit({ nonce: state.nextNonce, payload: payload }) }),
            Some(obs)))
        honest.union(forwarded).union(unsent).union(stray)
      | GetTxSInput(lookup) =>
        honest.union(lies.map(result => state.toLookupDoneOutput(lookup.query, result)))
      | _ =>
        match nonceOf(input) {
          | None => honest
          | Some(nonce) =>
            if (not(state.hasWaiter(nonce))) honest
            else
              val forgotten = { ...state, waiters: state.waiters.mapRemove(nonce) }
              match state.waiters.get(nonce) {
                | LookupWaiter(waiter) =>
                  honest.union(lies.map(result => forgotten.toLookupDoneOutput(waiter.query, result)))
                | AckWaiter(waiter) =>
                  if (waiter.awaited)
                    honest.union(SEND_OBSERVATIONS.map(obs =>
                      forgotten.toSendDoneOutput(Clean(waiter.payload), obs)))
                  else honest
              }
        }
    }
}

// -*- mode: Bluespec; -*-

/// The shim: it sits in front of an operator's indexer, forwards what is not
/// a migration, and diverts every migration, and every transaction lookup, to
/// the hub.
///
/// Like the hub it is one total function, `shim(state, input)`. Its state is
/// only the lookups it is waiting on: it keeps no record of a migration once
/// the wallet has its answer, which is why every lookup has to go to the hub.
///
/// The shim is honest in every configuration: it sees every migration in the
/// clear and controls everything the wallet observes, so a Byzantine shim
/// would void every wallet-facing guarantee and nothing else.
module shim {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"
  import wire.* from "./wire"

  // ------------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------------

  /// - `waiters`: the outstanding lookups, by nonce, with the txid each asks
  ///   about. A submission leaves nothing behind: the wallet has its answer
  ///   when the frame is handed over, and an ack, if one comes, is discarded.
  /// - `nextNonce`: the source of fresh nonces. A counter stands for a random
  ///   value nobody else can guess.
  type ShimState = { waiters: Nonce -> TxId, nextNonce: Nonce }

  pure val initialShim: ShimState = { waiters: Map(), nextNonce: 0 }

  // ------------------------------------------------------------------------
  // Inputs and outputs
  // ------------------------------------------------------------------------

  type ShimInput =
    // A wallet's `SendTransaction`. `handedOver` is whether the transport took
    // the frame for the hub.
    | SendTxSInput({ input: SendInput, handedOver: bool })
    | GetTxSInput(TxId)            // a wallet's `GetTransaction`
    | FrameSInput(Msg)             // a frame arrives from the network
    | LookupTimeoutSInput(Nonce)   // no reply to a lookup in time

  type ShimOutput =
    | ForwardOutput(Payload)                                      // to the operator's indexer
    // `payload` is what the wallet sent; `frame` goes to the hub; `told` is
    // the wallet's answer.
    | DivertedOutput({ payload: Payload, frame: Msg, told: SendObs })
    | SendDoneOutput({ input: SendInput, obs: SendObs })
    | LookupSentOutput(Msg)                                       // to the hub
    | LookupDoneOutput({ query: TxId, result: LookupObs })
    | NoShimOutput
    | ShimErrorOutput(str)

  type ShimResult = Result[ShimState, ShimOutput]

  pure def toForwardOutput(state: ShimState, payload: Payload): ShimResult =
    { state: state, out: ForwardOutput(payload) }

  pure def toDivertedOutput(state: ShimState, payload: Payload, frame: Msg, told: SendObs): ShimResult =
    { state: state, out: DivertedOutput({ payload: payload, frame: frame, told: told }) }

  pure def toSendDoneOutput(state: ShimState, input: SendInput, obs: SendObs): ShimResult =
    { state: state, out: SendDoneOutput({ input: input, obs: obs }) }

  pure def toLookupSentOutput(state: ShimState, frame: Msg): ShimResult =
    { state: state, out: LookupSentOutput(frame) }

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

  // ------------------------------------------------------------------------
  // SendTransaction
  // ------------------------------------------------------------------------

  /// Divert a migration: one frame to the hub, under a fresh nonce. The wallet
  /// is told ok as soon as the frame has been handed over, and no ack is
  /// waited for.
  pure def divert(state: ShimState, payload: Payload, handedOver: bool): ShimResult =
    if (not(handedOver))
      state.toSendDoneOutput(Clean(payload), SendUnavailable)
    else
      { ...state, nextNonce: state.nextNonce + 1 }
        .toDivertedOutput(payload, Submit({ nonce: state.nextNonce, payload: payload }), SentOk)

  /// Route a `SendTransaction`. Nothing but a cleanly read pass-through
  /// transaction ever reaches the operator; everything else is diverted or
  /// fails closed.
  pure def sendTransaction(state: ShimState, input: SendInput, handedOver: bool): ShimResult =
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

  /// Every lookup goes to the hub, under a fresh nonce.
  pure def getTransaction(state: ShimState, query: TxId): ShimResult =
    val nonce = state.nextNonce
    { waiters: state.waiters.put(nonce, query), nextNonce: nonce + 1 }
      .toLookupSentOutput(Lookup({ nonce: nonce, txid: query }))

  /// A lookup got no reply in time. It fails closed.
  pure def lookupTimeout(state: ShimState, nonce: Nonce): ShimResult =
    if (not(state.hasWaiter(nonce)))
      state.toShimErrorOutput("no request is waiting under this nonce")
    else
      { ...state, waiters: state.waiters.mapRemove(nonce) }
        .toLookupDoneOutput(state.waiters.get(nonce), Unavailable)

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
          val query = state.waiters.get(reply.nonce)
          { ...state, waiters: state.waiters.mapRemove(reply.nonce) }
            .toLookupDoneOutput(query, interpretReply(reply.reply, query))
      | Submit(_) => state.toShimErrorOutput("not a reply frame")
      | Lookup(_) => state.toShimErrorOutput("not a reply frame")
    }

  // ------------------------------------------------------------------------
  // The shim function
  // ------------------------------------------------------------------------

  pure def shim(state: ShimState, input: ShimInput): ShimResult =
    match input {
      | SendTxSInput(send) => sendTransaction(state, send.input, send.handedOver)
      | GetTxSInput(query) => getTransaction(state, query)
      | FrameSInput(msg) => receive(state, msg)
      | LookupTimeoutSInput(nonce) => lookupTimeout(state, nonce)
    }
}

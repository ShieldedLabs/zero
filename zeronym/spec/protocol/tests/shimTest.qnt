// -*- mode: Bluespec; -*-

/// The shim function.
module shimTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import shim.* from "../shim"

  pure def payload(id: str, class: Class): Payload =
    { id: id, txid: Some(id), created: 1, expiry: Some(9), class: class, oversize: false }

  pure val pOrchard = payload("orchard", OrchardTouching)
  pure val pPlain = payload("plain", PassThrough)
  pure val pJunk = { ...payload("junk", Unparseable), txid: None, expiry: None }
  pure val pBig = { ...payload("big", OrchardTouching), oversize: true }
  pure val pBigPlain = { ...payload("big-plain", PassThrough), oversize: true }

  pure val PAYLOADS = Set(pOrchard, pPlain, pJunk, pBig, pBigPlain)
  pure val SEND_INPUTS = PAYLOADS.map(p => Clean(p)).union(Set(Unreadable, EmptyBody))

  pure def after(state: ShimState, input: ShimInput): ShimState = shim(state, input).state
  pure def outputOf(state: ShimState, input: ShimInput): ShimOutput = shim(state, input).out

  pure def isError(output: ShimOutput): bool =
    match output {
      | ShimErrorOutput(_) => true
      | _ => false
    }

  pure def send(input: SendInput, handedOver: bool): ShimInput =
    SendTxSInput({ input: input, handedOver: handedOver })

  pure def ack(nonce: Nonce, given: WireAck): ShimInput =
    FrameSInput(Ack({ nonce: nonce, ack: given }))

  pure def reply(nonce: Nonce, given: WireReply): ShimInput =
    FrameSInput(LookupReply({ nonce: nonce, reply: given }))

  /// Nonce 0 went out with a submission; nonce 1 is a lookup.
  pure val busy = initialShim.after(send(Clean(pOrchard), true)).after(GetTxSInput("orchard"))

  pure val STATES = Set(initialShim, busy, initialShim.after(GetTxSInput("orchard")))

  pure val INPUTS: Set[ShimInput] =
    tuples(SEND_INPUTS, Set(true, false)).map(((input, handedOver)) => send(input, handedOver))
      .union(Set("orchard", "zz").map(query => GetTxSInput(query)))
      .union(0.to(2).map(nonce => ack(nonce, WAccepted)))
      .union(0.to(2).map(nonce => ack(nonce, WRefused(WQueueFull))))
      .union(0.to(2).map(nonce => reply(nonce, WNotFound)))
      .union(0.to(2).map(nonce => reply(nonce, WFound({ body: Some(pOrchard), height: 4 }))))
      .union(0.to(2).map(nonce => LookupTimeoutSInput(nonce)))
      .union(Set(
        FrameSInput(Submit({ nonce: 0, payload: pOrchard })),
        FrameSInput(Lookup({ nonce: 0, txid: "orchard" })),
      ))

  // ------------------------------------------------------------------------
  // SendTransaction
  // ------------------------------------------------------------------------

  /// F5. Only a cleanly read pass-through transaction is forwarded. Every
  /// other input is diverted or fails closed, in every state.
  run onlyPassThroughIsForwardedTest = all {
    assert(tuples(STATES, SEND_INPUTS, Set(true, false)).forall(((state, input, handedOver)) =>
      match outputOf(state, send(input, handedOver)) {
        | ForwardOutput(forwarded) => input == Clean(forwarded) and forwarded.class == PassThrough
        | _ => true
      })),
    assert(outputOf(initialShim, send(Clean(pPlain), true)) == ForwardOutput(pPlain)),
    // Forwarding does not depend on the hub, or on the hub frame's size.
    assert(outputOf(initialShim, send(Clean(pPlain), false)) == ForwardOutput(pPlain)),
    assert(outputOf(initialShim, send(Clean(pBigPlain), true)) == ForwardOutput(pBigPlain)),
    assert(initialShim.after(send(Clean(pPlain), true)) == initialShim),
    // A body the shim cannot parse is diverted, never forwarded.
    assert(outputOf(initialShim, send(Clean(pJunk), true))
      == DivertedOutput({ payload: pJunk, frame: Submit({ nonce: 0, payload: pJunk }), told: SentOk })),
  }

  run failClosedTest = all {
    assert(outputOf(initialShim, send(Unreadable, true)) == SendDoneOutput({ input: Unreadable, obs: SendUnavailable })),
    assert(outputOf(initialShim, send(EmptyBody, true)) == SendDoneOutput({ input: EmptyBody, obs: SendInvalid })),
    assert(outputOf(initialShim, send(Clean(pBig), true)) == SendDoneOutput({ input: Clean(pBig), obs: SendTooLarge })),
    // The frame not handed over: the hub is unreachable.
    assert(outputOf(initialShim, send(Clean(pOrchard), false))
      == SendDoneOutput({ input: Clean(pOrchard), obs: SendUnavailable })),
    // None of these leaves anything behind.
    assert(Set(send(Unreadable, true), send(EmptyBody, true), send(Clean(pBig), true), send(Clean(pOrchard), false))
      .forall(input => initialShim.after(input) == initialShim)),
  }

  run dispatchOnlyTest = all {
    // One frame, under a fresh nonce, and the wallet is told ok at once.
    assert(outputOf(initialShim, send(Clean(pOrchard), true))
      == DivertedOutput({ payload: pOrchard, frame: Submit({ nonce: 0, payload: pOrchard }), told: SentOk })),
    assert(initialShim.after(send(Clean(pOrchard), true)).nextNonce == 1),
    // A submission leaves nothing to wait on, and an ack tells nobody.
    assert(initialShim.after(send(Clean(pOrchard), true)).waiters == Map()),
    assert(outputOf(busy, ack(0, WRefused(WQueueFull))) == NoShimOutput),
    assert(busy.after(ack(0, WRefused(WQueueFull))) == busy),
  }

  // ------------------------------------------------------------------------
  // GetTransaction
  // ------------------------------------------------------------------------

  run lookupTest = all {
    // The lookup goes to the hub under a fresh nonce.
    assert(outputOf(initialShim, GetTxSInput("orchard")) == LookupSentOutput(Lookup({ nonce: 0, txid: "orchard" }))),
    assert(busy.waiters == Map(1 -> "orchard")),
    // Each reply arm.
    assert(outputOf(busy, reply(1, WFound({ body: None, height: MEMPOOL_HEIGHT })))
      == LookupDoneOutput({ query: "orchard", result: Pending })),
    assert(outputOf(busy, reply(1, WFound({ body: Some(pOrchard), height: 4 })))
      == LookupDoneOutput({ query: "orchard", result: Tx({ payload: pOrchard, height: 4 }) })),
    assert(outputOf(busy, reply(1, WFound({ body: Some(pPlain), height: 4 })))
      == LookupDoneOutput({ query: "orchard", result: NotFound })),
    assert(outputOf(busy, reply(1, WNotFound)) == LookupDoneOutput({ query: "orchard", result: NotFound })),
    assert(outputOf(busy, reply(1, WError)) == LookupDoneOutput({ query: "orchard", result: Unavailable })),
    // Any reply is final: the waiter is gone.
    assert(Set(WNotFound, WError).forall(given => not(busy.after(reply(1, given)).hasWaiter(1)))),
  }

  run lookupTimeoutTest = all {
    // A timeout fails the lookup closed.
    assert(outputOf(busy, LookupTimeoutSInput(1)) == LookupDoneOutput({ query: "orchard", result: Unavailable })),
    assert(not(busy.after(LookupTimeoutSInput(1)).hasWaiter(1))),
    // A late reply to the abandoned lookup is dropped.
    assert(outputOf(busy.after(LookupTimeoutSInput(1)), reply(1, WNotFound)) == NoShimOutput),
    assert(isError(outputOf(busy, LookupTimeoutSInput(0)))),
    assert(isError(outputOf(busy, LookupTimeoutSInput(9)))),
  }

  run correlationTest = all {
    // An unknown nonce is dropped.
    assert(outputOf(busy, ack(9, WAccepted)) == NoShimOutput and busy.after(ack(9, WAccepted)) == busy),
    assert(outputOf(busy, reply(9, WNotFound)) == NoShimOutput and busy.after(reply(9, WNotFound)) == busy),
    // An ack under a lookup's nonce is ignored, and the lookup keeps waiting.
    // A reply under a submission's nonce is dropped.
    assert(outputOf(busy, ack(1, WAccepted)) == NoShimOutput and busy.after(ack(1, WAccepted)) == busy),
    assert(outputOf(busy, reply(0, WNotFound)) == NoShimOutput and busy.after(reply(0, WNotFound)) == busy),
    // Request frames are not replies.
    assert(isError(outputOf(busy, FrameSInput(Submit({ nonce: 0, payload: pOrchard }))))),
    assert(isError(outputOf(busy, FrameSInput(Lookup({ nonce: 1, txid: "orchard" }))))),
  }

  // ------------------------------------------------------------------------
  // Totality
  // ------------------------------------------------------------------------

  /// F11. The function answers every input in every state, and an invalid
  /// input leaves the state unchanged.
  run totalityTest =
    assert(tuples(STATES, INPUTS).forall(((state, input)) =>
      val result = shim(state, input)
      and {
        result.state.nextNonce >= state.nextNonce,
        isError(result.out) implies result.state == state,
      }))
}

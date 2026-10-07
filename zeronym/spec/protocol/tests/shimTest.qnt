// -*- mode: Bluespec; -*-

/// The shim function, checked with one hub and with two.
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
  pure val HEIGHTS = Set(0, 4)
  pure val SEND_INPUTS = PAYLOADS.map(p => Clean(p)).union(Set(Unreadable, EmptyBody))

  pure val dispatching = initialShim(["h1", "h2"])
  pure val single = initialShim(["h1"])

  pure def after(state: ShimState, input: ShimInput): ShimState = shim(state, input).state
  pure def outputOf(state: ShimState, input: ShimInput): ShimOutput = shim(state, input).out

  pure def isError(output: ShimOutput): bool =
    match output {
      | ShimErrorOutput(_) => true
      | _ => false
    }

  pure def send(input: SendInput, handedOver: int): ShimInput =
    SendTxSInput({ input: input, handedOver: handedOver })

  pure def getTx(query: TxId, start: int): ShimInput =
    GetTxSInput({ query: query, start: start })

  pure def ack(nonce: Nonce, given: WireAck): ShimInput =
    FrameSInput(Ack({ nonce: nonce, ack: given }))

  pure def reply(nonce: Nonce, given: WireReply): ShimInput =
    FrameSInput(LookupReply({ nonce: nonce, reply: given }))

  /// Nonces 0 and 1 went out with a submission; nonce 2 is a lookup at `h2`.
  pure val busy = dispatching.after(send(Clean(pOrchard), 2)).after(getTx("orchard", 1))

  pure val STATES = Set(dispatching, single, busy, single.after(getTx("orchard", 0)))

  pure val INPUTS: Set[ShimInput] =
    tuples(SEND_INPUTS, 0.to(3)).map(((input, handedOver)) => send(input, handedOver))
      .union(tuples(Set("orchard", "zz"), 0.to(2)).map(((query, start)) => getTx(query, start)))
      .union(0.to(3).map(nonce => ack(nonce, WAccepted)))
      .union(0.to(3).map(nonce => ack(nonce, WRefused(WQueueFull))))
      .union(0.to(3).map(nonce => reply(nonce, WNotFound)))
      .union(0.to(3).map(nonce => reply(nonce, WFound({ body: Some(pOrchard), height: 4 }))))
      .union(0.to(3).map(nonce => LookupTimeoutSInput(nonce)))
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
    assert(tuples(STATES, SEND_INPUTS, 0.to(2)).forall(((state, input, handedOver)) =>
      match outputOf(state, send(input, handedOver)) {
        | ForwardOutput(forwarded) => input == Clean(forwarded) and forwarded.class == PassThrough
        | _ => true
      })),
    assert(outputOf(dispatching, send(Clean(pPlain), 2)) == ForwardOutput(pPlain)),
    // Forwarding does not depend on the hubs, or on the hub frame's size.
    assert(outputOf(dispatching, send(Clean(pPlain), 0)) == ForwardOutput(pPlain)),
    assert(outputOf(dispatching, send(Clean(pBigPlain), 2)) == ForwardOutput(pBigPlain)),
    assert(dispatching.after(send(Clean(pPlain), 2)) == dispatching),
    // A body the shim cannot parse is diverted, never forwarded.
    assert(outputOf(dispatching, send(Clean(pJunk), 1))
      == DivertedOutput({
        payload: pJunk,
        frames: Set({ hub: "h1", msg: Submit({ nonce: 0, payload: pJunk }) }),
        told: SentOk,
      })),
  }

  run failClosedTest = all {
    assert(outputOf(dispatching, send(Unreadable, 2)) == SendDoneOutput({ input: Unreadable, obs: SendUnavailable })),
    assert(outputOf(dispatching, send(EmptyBody, 2)) == SendDoneOutput({ input: EmptyBody, obs: SendInvalid })),
    assert(outputOf(dispatching, send(Clean(pBig), 2)) == SendDoneOutput({ input: Clean(pBig), obs: SendTooLarge })),
    // No frame handed over: the hub is unreachable.
    assert(Set(dispatching, single).forall(state =>
      outputOf(state, send(Clean(pOrchard), 0)) == SendDoneOutput({ input: Clean(pOrchard), obs: SendUnavailable }))),
    // None of these leaves anything behind.
    assert(Set(send(Unreadable, 2), send(EmptyBody, 2), send(Clean(pBig), 2), send(Clean(pOrchard), 0))
      .forall(input => dispatching.after(input) == dispatching)),
  }

  run dispatchOnlyTest = all {
    // One frame per hub, each under its own nonce, and the wallet is told ok
    // at once.
    assert(outputOf(dispatching, send(Clean(pOrchard), 2)) == DivertedOutput({
      payload: pOrchard,
      frames: Set(
        { hub: "h1", msg: Submit({ nonce: 0, payload: pOrchard }) },
        { hub: "h2", msg: Submit({ nonce: 1, payload: pOrchard }) },
      ),
      told: SentOk,
    })),
    // A sweep that stopped after the first address still tells the wallet ok.
    assert(outputOf(dispatching, send(Clean(pOrchard), 1)) == DivertedOutput({
      payload: pOrchard,
      frames: Set({ hub: "h1", msg: Submit({ nonce: 0, payload: pOrchard }) }),
      told: SentOk,
    })),
    assert(dispatching.after(send(Clean(pOrchard), 2)).nextNonce == 2),
    // A submission leaves nothing to wait on, and an ack tells nobody.
    assert(dispatching.after(send(Clean(pOrchard), 2)).waiters == Map()),
    assert(outputOf(busy, ack(0, WRefused(WQueueFull))) == NoShimOutput),
    assert(busy.after(ack(0, WRefused(WQueueFull))) == busy),
  }

  // ------------------------------------------------------------------------
  // GetTransaction
  // ------------------------------------------------------------------------

  run lookupTest = all {
    // The lookup goes to the hub the cursor points at.
    assert(outputOf(dispatching, getTx("orchard", 0))
      == LookupSentOutput({ frame: { hub: "h1", msg: Lookup({ nonce: 0, txid: "orchard" }) } })),
    assert(outputOf(dispatching, getTx("orchard", 1))
      == LookupSentOutput({ frame: { hub: "h2", msg: Lookup({ nonce: 0, txid: "orchard" }) } })),
    assert(isError(outputOf(dispatching, getTx("orchard", 2)))),
    // No hub configured to ask: unavailable.
    assert(outputOf(initialShim([]), getTx("orchard", 0))
      == LookupDoneOutput({ query: "orchard", result: Unavailable })),
    // Each reply arm.
    assert(outputOf(busy, reply(2, WFound({ body: None, height: MEMPOOL_HEIGHT })))
      == LookupDoneOutput({ query: "orchard", result: Pending })),
    assert(outputOf(busy, reply(2, WFound({ body: Some(pOrchard), height: 4 })))
      == LookupDoneOutput({ query: "orchard", result: Tx({ payload: pOrchard, height: 4 }) })),
    assert(outputOf(busy, reply(2, WFound({ body: Some(pPlain), height: 4 })))
      == LookupDoneOutput({ query: "orchard", result: NotFound })),
    assert(outputOf(busy, reply(2, WNotFound)) == LookupDoneOutput({ query: "orchard", result: NotFound })),
    assert(outputOf(busy, reply(2, WError)) == LookupDoneOutput({ query: "orchard", result: Unavailable })),
    // Any reply is final: the waiter is gone, and no other hub is asked.
    assert(Set(WNotFound, WError).forall(given => not(busy.after(reply(2, given)).hasWaiter(2)))),
  }

  run lookupFailoverTest = all {
    // Only a timeout moves on, to the next hub round the ring, under a fresh
    // nonce.
    assert(outputOf(busy, LookupTimeoutSInput(2))
      == LookupSentOutput({ frame: { hub: "h1", msg: Lookup({ nonce: 3, txid: "orchard" }) } })),
    assert(not(busy.after(LookupTimeoutSInput(2)).hasWaiter(2))),
    // The last hub timing out fails the lookup closed.
    assert(outputOf(busy.after(LookupTimeoutSInput(2)), LookupTimeoutSInput(3))
      == LookupDoneOutput({ query: "orchard", result: Unavailable })),
    // A late reply to the abandoned attempt is dropped.
    assert(outputOf(busy.after(LookupTimeoutSInput(2)), reply(2, WNotFound)) == NoShimOutput),
    assert(isError(outputOf(busy, LookupTimeoutSInput(0)))),
    assert(isError(outputOf(busy, LookupTimeoutSInput(9)))),
  }

  run correlationTest = all {
    // An unknown nonce is dropped.
    assert(outputOf(busy, ack(9, WAccepted)) == NoShimOutput and busy.after(ack(9, WAccepted)) == busy),
    assert(outputOf(busy, reply(9, WNotFound)) == NoShimOutput and busy.after(reply(9, WNotFound)) == busy),
    // An ack under a lookup's nonce is ignored, and the lookup keeps waiting.
    // A reply under a submission's nonce is dropped.
    assert(outputOf(busy, ack(2, WAccepted)) == NoShimOutput and busy.after(ack(2, WAccepted)) == busy),
    assert(outputOf(busy, reply(0, WNotFound)) == NoShimOutput and busy.after(reply(0, WNotFound)) == busy),
    // Request frames are not replies.
    assert(isError(outputOf(busy, FrameSInput(Submit({ nonce: 0, payload: pOrchard }))))),
    assert(isError(outputOf(busy, FrameSInput(Lookup({ nonce: 2, txid: "orchard" }))))),
  }

  // ------------------------------------------------------------------------
  // Totality and the Byzantine relation
  // ------------------------------------------------------------------------

  /// F11. The function answers every input in every state, and an invalid
  /// input leaves the state unchanged.
  run totalityTest =
    assert(tuples(STATES, INPUTS).forall(((state, input)) =>
      val result = shim(state, input)
      and {
        result.state.hubs == state.hubs,
        result.state.nextNonce >= state.nextNonce,
        isError(result.out) implies result.state == state,
      }))

  /// F12. The Byzantine relation contains the honest transition.
  run byzantineContainsHonestTest =
    assert(tuples(STATES, INPUTS).forall(((state, input)) =>
      byzShimResults(state, input, PAYLOADS, HEIGHTS).contains(shim(state, input))))

  run byzantineShimTest = all {
    // It can hand a migration to the operator.
    assert(byzShimResults(dispatching, send(Clean(pOrchard), 2), PAYLOADS, HEIGHTS)
      .contains(dispatching.toForwardOutput(pOrchard))),
    // It can tell the wallet ok and send nothing.
    assert(byzShimResults(single, send(Clean(pOrchard), 1), PAYLOADS, HEIGHTS)
      .contains(single.toSendDoneOutput(Clean(pOrchard), SentOk))),
    // It can answer a lookup with a transaction that has another txid.
    assert(byzShimResults(dispatching, getTx("orchard", 0), PAYLOADS, HEIGHTS)
      .contains(dispatching.toLookupDoneOutput("orchard", Tx({ payload: pPlain, height: 4 })))),
    // It can send a hub something other than what the wallet sent.
    assert(byzShimResults(dispatching, send(Clean(pOrchard), 2), PAYLOADS, HEIGHTS).exists(result =>
      result.out == DivertedOutput({
        payload: pOrchard,
        frames: Set({ hub: "h2", msg: Submit({ nonce: 0, payload: pJunk }) }),
        told: SentOk,
      }))),
  }
}

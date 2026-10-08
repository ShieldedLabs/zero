// -*- mode: Bluespec; -*-

/// Each realisation in `realisations.qnt`, replayed through the real hub
/// function on the hub specification's `timely` schedule.
///
/// The protocol spec's hub answers wherever the real one would refuse or
/// error, so a protocol run proves nothing about reachability on its own.
module realisedRunsTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import hub.* from "../hub"
  import abstractHub.* from "../abstractHub"
  import protocol.* from "../protocol"
  import realisations.* from "./realisations"

  /// The hub specification's `timely` schedule.
  pure val PARAMS: HubParams = {
    flushInterval: 3,
    miningMargin: 2,
    deliveryLag: 1,
    minWalletExpiry: 7,
    reorgAllowance: 1,
    maxAttempts: 2,
  }
  /// What a Byzantine hub builds its lies from.
  pure val LIES = universeOf(baseline)

  pure def replyOf(output: HubOutput): List[AReply] =
    match output {
      | AckOutput(ack) => [AAck(renderAck(ack.kind))]
      | LookupReplyOutput(reply) => [AWire(render(reply.outcome))]
      | _ => []
    }

  /// The run, from a starting hub: whether every step was taken, the replies
  /// in order, and the hub as the protocol sees it at the end.
  pure def replay(steps: List[Step]): { ok: bool, replies: List[AReply], last: AHub } =
    val end = steps.foldl({ state: startingHub(PARAMS), ok: true, replies: [] }, (acc, step) =>
      val result = match step {
        | Do(input) => hub(acc.state, input)
        | Lie(lie) => { state: acc.state, out: lie.out }
      }
      val taken = match step {
        | Do(_) => not(match result.out { | HubErrorOutput(_) => true | _ => false })
        | Lie(lie) => byzHubResults(acc.state, lie.input, LIES).contains(result)
      }
      { state: result.state, ok: acc.ok and taken, replies: acc.replies.concat(result.out.replyOf()) })
    { ok: end.ok, replies: end.replies, last: { queue: end.state.queued(), held: end.state.inFlight() } }

  pure def realises(r: Realisation): bool =
    replay(r.steps) == { ok: true, replies: r.replies, last: r.last }

  run realisedRunsTest = all {
    assert(realises(k1a)),
    assert(realises(k2a)),
    assert(realises(k2bc)),
    assert(realises(k2d)),
    assert(realises(k2e)),
    assert(realises(w8)),
    assert(realises(w9)),
  }

  run realisedByzantineRunsTest = all {
    assert(realises(hubLeaksBody)),
    assert(realises(hubDeniesQueued)),
    assert(realises(hubFalseHeight)),
    assert(realises(hubAcksUnqueued)),
    assert(realises(w16)),
    assert(realises(indexerLeaksBody)),
    assert(realises(indexerForgesPending)),
  }
}

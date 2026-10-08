// -*- mode: Bluespec; -*-

/// The hub's part of each pinned protocol run that shows a violation or a
/// reached state, replayed through the real hub function.
///
/// The protocol spec's hub answers wherever the real one would refuse or
/// error, so a protocol run proves nothing about reachability on its own.
/// Each realisation here is the sequence of inputs the hub sees in that run,
/// on the hub specification's `timely` schedule, with the replies the run
/// relies on. A lie is a
/// member of the Byzantine relation, with the state unchanged.
///
/// K1b has no hub step: the frame is never delivered.
module realisedRunsTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import hub.* from "../hub"
  import abstractHub.* from "../abstractHub"
  import configs.* from "../instances"

  /// The hub specification's `timely` schedule.
  pure val PARAMS: HubParams = {
    flushInterval: 3,
    miningMargin: 2,
    deliveryLag: 1,
    minWalletExpiry: 7,
    reorgAllowance: 1,
    maxAttempts: 2,
  }
  pure val UNIVERSE = baseline.payloads.union(baseline.twins).union(baseline.tpPayloads)
  pure val HEIGHTS = 0.to(baseline.maxHeight)

  type Step = Do(HubInput) | Lie({ input: HubInput, out: HubOutput })

  pure def sub(nonce: Nonce, payload: Payload): HubInput = SubmitHInput({ nonce: nonce, payload: payload })
  pure def look(nonce: Nonce, txid: TxId, answer: IndexerAnswer): HubInput =
    LookupHInput({ nonce: nonce, txid: txid, answer: answer })
  pure def judged(payload: Payload, verdict: Verdict): HubInput =
    VerdictHInput({ payload: payload, verdict: verdict })
  pure def found(body: Option[Payload], height: Height): IndexerAnswer = IFound({ body: body, height: height })
  pure def tips(heights: List[Height]): List[Step] = heights.foldl([], (acc, h) => acc.append(Do(TipHInput(h))))
  pure def does(inputs: List[HubInput]): List[Step] = inputs.foldl([], (acc, i) => acc.append(Do(i)))

  pure def flushed(payload: Payload, verdict: Verdict): List[Step] =
    does([FlushDueHInput, judged(payload, verdict), FlushDoneHInput])

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
        | Lie(lie) => byzHubResults(acc.state, lie.input, UNIVERSE, HEIGHTS).contains(result)
      }
      { state: result.state, ok: acc.ok and taken, replies: acc.replies.concat(result.out.replyOf()) })
    { ok: end.ok, replies: end.replies, last: { queue: end.state.queued(), held: end.state.inFlight() } }

  pure def realises(steps: List[Step], replies: List[AReply], last: AHub): bool =
    replay(steps) == { ok: true, replies: replies, last: last }

  pure val accepted = AAck(WAccepted)
  pure val pending = AWire(WFound({ body: None, height: MEMPOOL_HEIGHT }))
  pure val inMempool = AWire(WFound({ body: Some(early), height: MEMPOOL_HEIGHT }))
  pure val queuedEarly = { queue: Set(early), held: Set() }

  /// Running at height 2 with `early` queued, as most runs begin.
  pure val earlyQueued = tips([1, 2]).append(Do(sub(0, early)))
  /// The same, flushed and accepted at height 3.
  pure val earlyPublished = earlyQueued.concat(tips([3])).concat(flushed(early, Accepted))

  run realisedRunsTest = all {
    // K1a: refused after the flush at 3.
    assert(realises(tips([1, 2, 3]).concat(does([FlushDueHInput, sub(0, tight)])),
      [AAck(WRefused(WExpiryTooTight))], emptyAHub)),
    // K2a: pending, then in the mempool.
    assert(realises(earlyQueued.append(Do(look(1, "early", INotFound))).concat(tips([3])).concat(flushed(early, Accepted))
      .append(Do(look(2, "early", found(Some(early), MEMPOOL_HEIGHT)))),
      [accepted, pending, inMempool], emptyAHub)),
    // K2b and K2c (the resubmission comes from the wallet or the third party).
    assert(realises(earlyPublished.append(Do(look(1, "early", found(Some(early), MEMPOOL_HEIGHT)))).append(Do(sub(2, early)))
      .append(Do(look(3, "early", found(Some(early), MEMPOOL_HEIGHT)))),
      [accepted, inMempool, accepted, pending], queuedEarly)),
    // K2d: the flush window.
    assert(realises(earlyQueued.append(Do(look(1, "early", INotFound))).concat(tips([3])).append(Do(FlushDueHInput))
      .append(Do(look(2, "early", INotFound))),
      [accepted, pending, AWire(WNotFound)], { queue: Set(), held: Set(early) })),
    // K2e: rejected at flush.
    assert(realises(earlyQueued.append(Do(look(1, "early", INotFound))).concat(tips([3])).concat(flushed(early, Rejected))
      .append(Do(look(2, "early", INotFound))),
      [accepted, pending, AWire(WNotFound)], emptyAHub)),
    // W8: a third party is told `early` is queued.
    assert(realises(earlyQueued.append(Do(look(0, "early", INotFound))), [accepted, pending], queuedEarly)),
    // W9, the unknown upgrade: queued and missed.
    assert(realises(tips([1]).concat(does([sub(0, junk), look(1, "junk", INotFound)])),
      [accepted, AWire(WNotFound)], { queue: Set(junk), held: Set() })),
  }

  /// The "required" runs of the trust matrix and W16, each one lie.
  pure def lieAbout(nonce: Nonce, answer: IndexerAnswer, outcome: HubOutcome): Step =
    Lie({ input: look(nonce, "early", answer), out: LookupReplyOutput({ nonce: nonce, outcome: outcome }) })

  run realisedByzantineRunsTest = all {
    // G2, the hub: the queued bytes, to a third party.
    assert(realises(earlyQueued.append(lieAbout(0, INotFound, FromIndexer(found(Some(early), MEMPOOL_HEIGHT)))),
      [accepted, inMempool], queuedEarly)),
    // G4, the hub: not found for a queued transaction.
    assert(realises(earlyQueued.append(lieAbout(1, INotFound, FromIndexer(INotFound))),
      [accepted, AWire(WNotFound)], queuedEarly)),
    // G4, the hub: a false height.
    assert(realises(earlyPublished.append(lieAbout(1, found(Some(early), MEMPOOL_HEIGHT), FromIndexer(found(Some(early), 9)))),
      [accepted, AWire(WFound({ body: Some(early), height: 9 }))], emptyAHub)),
    // G8, the hub: accepted and not queued.
    assert(realises(tips([1, 2]).append(Lie({ input: sub(0, early), out: AckOutput({ nonce: 0, kind: Admitted }) })),
      [accepted], emptyAHub)),
    // W16: a twin at a false height.
    assert(realises(earlyQueued.append(lieAbout(1, INotFound, FromIndexer(found(Some(earlyTwin), 9)))),
      [accepted, AWire(WFound({ body: Some(earlyTwin), height: 9 }))], queuedEarly)),
    // G2, the indexer: the honest hub forwards the unpublished bytes.
    assert(realises(earlyQueued.concat(tips([3])).concat(does([FlushDueHInput, judged(early, Retryable),
      look(0, "early", found(Some(early), MEMPOOL_HEIGHT))])),
      [accepted, inMempool], { queue: Set(), held: Set(early) })),
    // G4, the indexer: a forged pending for a transaction nobody queued.
    assert(realises(tips([1, 2]).append(Do(look(1, "early", found(None, MEMPOOL_HEIGHT)))), [pending], emptyAHub)),
  }
}

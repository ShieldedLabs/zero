// -*- mode: Bluespec; -*-

/// The indexer relations, checked over a small universe.
module indexerTest {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import indexer.* from "../indexer"

  pure def orchard(id: str, txid: TxId, expiry: Option[Height]): Payload =
    { id: id, txid: Some(txid), created: 1, expiry: expiry, class: OrchardTouching, oversize: false }

  pure val pA = orchard("a", "ta", Some(9))
  pure val pATwin = orchard("a-twin", "ta", Some(9))
  pure val pB = orchard("b", "tb", None)
  pure val pOld = orchard("old", "told", Some(4))
  pure val pJunk = { id: "junk", txid: None, created: 1, expiry: None, class: Unparseable, oversize: false }

  pure val UNIVERSE = Set(pA, pATwin, pB, pOld, pJunk)
  pure val HEIGHTS = Set(0, 3, 7)

  pure def verdictOn(given: Verdict): IndexerOutput = VerdictOutput(given)

  pure val empty = initialIndexer(4)
  /// `pA` accepted at height 4.
  pure val withA = indexerApply(empty, BroadcastIInput(pA), verdictOn(Accepted))
  /// `pA` mined at height 5.
  pure val minedA = indexerApply(indexerApply(withA, AdvanceIInput, NoIndexerOutput), MineIInput("ta"), NoIndexerOutput)
  /// `pB` offered and not taken.
  pure val offeredB = indexerApply(empty, BroadcastIInput(pB), verdictOn(Retryable))

  pure val STATES = Set(empty, withA, minedA, offeredB)
  pure val INPUTS: Set[IndexerInput] =
    UNIVERSE.map(payload => BroadcastIInput(payload))
      .union(Set(LookupIInput("ta"), LookupIInput("tb"), AdvanceIInput, MineIInput("ta"), MineIInput("tb")))

  run broadcastVerdictsTest = all {
    // A valid, unexpired, unknown transaction may be accepted.
    assert(honestIndexerOutputs(empty, BroadcastIInput(pA))
      == Set(Accepted, Rejected, Retryable).map(verdictOn)),
    // One that can no longer be mined may not.
    assert(honestIndexerOutputs(empty, BroadcastIInput(pOld))
      == Set(Rejected, Retryable).map(verdictOn)),
    // Nor one that does not parse.
    assert(honestIndexerOutputs(empty, BroadcastIInput(pJunk))
      == Set(Rejected, Retryable).map(verdictOn)),
    // The chain has it: already known, under these bytes or a twin's.
    assert(honestIndexerOutputs(withA, BroadcastIInput(pA))
      == Set(AlreadyKnown, Rejected, Retryable).map(verdictOn)),
    assert(honestIndexerOutputs(withA, BroadcastIInput(pATwin))
      == Set(AlreadyKnown, Rejected, Retryable).map(verdictOn)),
  }

  run broadcastEffectTest = all {
    assert(withA.inclusion("ta") == InMempool and withA.published() == Set(pA)),
    assert(withA.offered == Set(pA)),
    // Offered is remembered whatever the verdict; the chain is untouched.
    assert(offeredB.offered == Set(pB) and offeredB.txs == Map()),
    assert(Set(AlreadyKnown, Rejected, Retryable).forall(given =>
      indexerApply(empty, BroadcastIInput(pA), verdictOn(given)).txs == Map())),
    // The chain keeps the bytes it accepted first.
    assert(indexerApply(withA, BroadcastIInput(pATwin), verdictOn(AlreadyKnown)).published() == Set(pA)),
  }

  run lookupTest = all {
    assert(honestIndexerOutputs(empty, LookupIInput("ta"))
      == Set(AnswerOutput(INotFound), AnswerOutput(IUnavailable))),
    // A mempool transaction is found at height 0, with its bytes.
    assert(honestIndexerOutputs(withA, LookupIInput("ta"))
      == Set(AnswerOutput(IFound({ body: Some(pA), height: MEMPOOL_HEIGHT })), AnswerOutput(IUnavailable))),
    assert(honestIndexerOutputs(minedA, LookupIInput("ta"))
      == Set(AnswerOutput(IFound({ body: Some(pA), height: 5 })), AnswerOutput(IUnavailable))),
    assert(chainAnswer(empty, "ta") == INotFound),
    assert(chainAnswer(minedA, "ta") == IFound({ body: Some(pA), height: 5 })),
    // A lookup changes nothing.
    assert(STATES.forall(state =>
      honestIndexerResults(state, LookupIInput("ta")).forall(result => result.state == state))),
    // An honest indexer never reports a transaction without its bytes.
    assert(tuples(STATES, Set("ta", "tb")).forall(((state, txid)) =>
      honestIndexerOutputs(state, LookupIInput(txid)).forall(output =>
        match output {
          | AnswerOutput(answer) =>
            match answer {
              | IFound(found) => isSome(found.body)
              | _ => true
            }
          | _ => true
        }))),
  }

  run chainTest = all {
    assert(indexerApply(empty, AdvanceIInput, NoIndexerOutput).height == 5),
    assert(minedA.inclusion("ta") == MinedAt(5)),
    // Only a mempool transaction can be mined: not an absent one, not twice.
    assert(honestIndexerOutputs(withA, MineIInput("ta")) == Set(NoIndexerOutput)),
    assert(honestIndexerOutputs(empty, MineIInput("ta")) == Set()),
    assert(honestIndexerOutputs(minedA, MineIInput("ta")) == Set()),
    assert(indexerApply(minedA, MineIInput("ta"), NoIndexerOutput) == minedA),
  }

  run tipReportsTest = all {
    assert(honestTips(empty, 0) == Set(4)),
    assert(honestTips(empty, 1) == Set(3, 4)),
    assert(honestTips(initialIndexer(1), 3) == Set(0, 1)),
  }

  /// F12. The Byzantine relation contains every honest transition, and every
  /// honest tip report is one a Byzantine indexer could make.
  run byzantineContainsHonestTest = all {
    assert(tuples(STATES, INPUTS).forall(((state, input)) =>
      honestIndexerResults(state, input).subseteq(byzIndexerResults(state, input, UNIVERSE, HEIGHTS)))),
    assert(tuples(STATES, INPUTS).forall(((state, input)) =>
      honestIndexerOutputs(state, input).subseteq(byzIndexerOutputs(state, input, UNIVERSE, HEIGHTS)))),
    assert(tuples(STATES, 0.to(3)).forall(((state, slack)) =>
      honestTips(state, slack).subseteq(byzTips(7)))),
  }

  run byzantineIndexerTest = all {
    // It may serve what it was offered and the chain has not published.
    assert(offeredB.servable(UNIVERSE) == Set(pB)),
    assert(byzIndexerOutputs(offeredB, LookupIInput("tb"), UNIVERSE, HEIGHTS)
      .contains(AnswerOutput(IFound({ body: Some(pB), height: 7 })))),
    // A twin of what it knows, too. Nothing else.
    assert(withA.servable(UNIVERSE) == Set(pA, pATwin)),
    assert(empty.servable(UNIVERSE) == Set()),
    // "Found, height 0, no body" for a transaction that does not exist.
    assert(byzIndexerOutputs(empty, LookupIInput("ta"), UNIVERSE, HEIGHTS)
      .contains(AnswerOutput(IFound({ body: None, height: MEMPOOL_HEIGHT })))),
    // The verdict and the effect are independent.
    assert(byzIndexerResults(empty, BroadcastIInput(pA), UNIVERSE, HEIGHTS)
      .contains({ state: offeredB.with("offered", Set(pA)), out: verdictOn(Accepted) })),
    assert(byzIndexerResults(empty, BroadcastIInput(pA), UNIVERSE, HEIGHTS)
      .contains({ state: withA, out: verdictOn(Rejected) })),
    // It cannot put an expired transaction on the chain.
    assert(byzIndexerResults(empty, BroadcastIInput(pOld), UNIVERSE, HEIGHTS)
      .forall(result => result.state.txs == Map())),
    // The chain's own steps are not the indexer's to change.
    assert(byzIndexerResults(withA, AdvanceIInput, UNIVERSE, HEIGHTS) == honestIndexerResults(withA, AdvanceIInput)),
    assert(byzIndexerResults(withA, MineIInput("ta"), UNIVERSE, HEIGHTS) == honestIndexerResults(withA, MineIInput("ta"))),
  }
}

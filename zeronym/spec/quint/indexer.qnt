// -*- mode: Bluespec; -*-

/// The chain as a hub sees it: one abstract indexer standing for all of a
/// hub's indexer endpoints and the network behind them.
///
/// It is the environment, written in the same shape as the components: an
/// input, a set of possible outputs, and the effect of each on the state. The
/// honest relation is already nondeterministic, because a hub cannot tell in
/// advance whether a broadcast will be taken. The Byzantine relation is a
/// superset of it.
///
/// A hub folds several endpoints into one answer, and the folds are not
/// symmetric. The tip is the maximum over the endpoints that answer; a lookup
/// returns the first `found`; a broadcast takes the best verdict. So a single
/// misbehaving endpoint is enough to raise the tip, inject a lookup answer or
/// change a verdict, while lowering or freezing the tip takes every endpoint.
/// "Byzantine indexer" in this specification covers both cases.
module indexer {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"

  /// A transaction the chain has, with the bytes it was accepted as.
  type ChainTx = { payload: Payload, at: Inclusion }

  /// - `height`: the true chain height.
  /// - `txs`: the transactions in the mempool or in a block, by txid.
  /// - `offered`: every payload a hub has broadcast, whatever came of it. It
  ///   is what an indexer has seen, and so what a Byzantine one could reveal.
  type IndexerState = {
    height: Height,
    txs: TxId -> ChainTx,
    offered: Set[Payload],
  }

  type IndexerInput =
    | BroadcastIInput(Payload)   // a hub publishes one entry of a batch
    | LookupIInput(TxId)         // a hub's lookup missed its queue
    | AdvanceIInput              // the chain grows by one block
    | MineIInput(TxId)           // a mempool transaction is included

  type IndexerOutput =
    | VerdictOutput(Verdict)
    | AnswerOutput(IndexerAnswer)
    | NoIndexerOutput

  type IndexerResult = Result[IndexerState, IndexerOutput]

  pure def initialIndexer(height: Height): IndexerState =
    { height: height, txs: Map(), offered: Set() }

  // ------------------------------------------------------------------------
  // Views
  // ------------------------------------------------------------------------

  /// Where `txid` stands on the chain.
  pure def inclusion(state: IndexerState, txid: TxId): Inclusion =
    if (state.txs.keys().contains(txid)) state.txs.get(txid).at else Absent

  /// The payloads the chain has made public.
  pure def published(state: IndexerState): Set[Payload] =
    state.txs.keys().map(txid => state.txs.get(txid).payload)

  /// Whether the chain already has a transaction with `payload`'s txid.
  pure def isKnown(state: IndexerState, payload: Payload): bool =
    match payload.txid {
      | Some(txid) => state.txs.keys().contains(txid)
      | None => false
    }

  /// Whether a node would take `payload` into its mempool now: it parses, it
  /// can still be mined in the next block, and it is not there already.
  pure def isAcceptable(state: IndexerState, payload: Payload): bool =
    and {
      isSome(payload.txid),
      not(state.isKnown(payload)),
      match payload.expiry {
        | Some(expiry) => expiry > state.height
        | None => true
      },
    }

  // ------------------------------------------------------------------------
  // Effect
  // ------------------------------------------------------------------------

  /// The state after `input` was answered with `output`. A broadcast is always
  /// remembered as offered; it reaches the mempool only on `Accepted`.
  pure def indexerApply(state: IndexerState, input: IndexerInput, output: IndexerOutput): IndexerState =
    match input {
      | BroadcastIInput(payload) =>
        val noted = { ...state, offered: state.offered.union(Set(payload)) }
        match payload.txid {
          | Some(txid) =>
            if (output == VerdictOutput(Accepted))
              { ...noted, txs: noted.txs.put(txid, { payload: payload, at: InMempool }) }
            else noted
          | None => noted
        }
      | LookupIInput(_) => state
      | AdvanceIInput => { ...state, height: state.height + 1 }
      | MineIInput(txid) =>
        if (state.inclusion(txid) == InMempool)
          { ...state, txs: state.txs.setBy(txid, tx => { ...tx, at: Mined }) }
        else state
    }

  // ------------------------------------------------------------------------
  // Honest relation
  // ------------------------------------------------------------------------

  /// The outputs an honest indexer may give. An empty set means the input is
  /// not enabled.
  ///
  /// - Broadcast: `Rejected` and `Retryable` are always possible (the node has
  ///   reasons this model does not see, and the indexer may be unreachable);
  ///   `Accepted` only for a transaction a node would take; `AlreadyKnown`
  ///   only for one the chain has.
  /// - Lookup: the chain's answer, or unavailable.
  pure def honestIndexerOutputs(state: IndexerState, input: IndexerInput): Set[IndexerOutput] =
    match input {
      | BroadcastIInput(payload) =>
        Set(Rejected, Retryable)
          .union(if (state.isAcceptable(payload)) Set(Accepted) else Set())
          .union(if (state.isKnown(payload)) Set(AlreadyKnown) else Set())
          .map(verdict => VerdictOutput(verdict))
      | LookupIInput(txid) =>
        if (state.txs.keys().contains(txid))
          val tx = state.txs.get(txid)
          Set(
            AnswerOutput(IFound({ body: Some(tx.payload), height: answerHeight(tx.at) })),
            AnswerOutput(IUnavailable),
          )
        else Set(AnswerOutput(INotFound), AnswerOutput(IUnavailable))
      | AdvanceIInput => Set(NoIndexerOutput)
      | MineIInput(txid) =>
        if (state.inclusion(txid) == InMempool) Set(NoIndexerOutput) else Set()
    }

  pure def honestIndexerResults(state: IndexerState, input: IndexerInput): Set[IndexerResult] =
    honestIndexerOutputs(state, input).map(output =>
      { state: indexerApply(state, input, output), out: output })

  /// The honest relation as the protocol specification uses it, which has no
  /// chain height: a broadcast that parses and is new may be accepted whatever
  /// its expiry. It contains the honest relation (`heightlessCoversTest`).
  pure def heightlessIndexerResults(state: IndexerState, input: IndexerInput): Set[IndexerResult] =
    val accepted = match input {
      | BroadcastIInput(payload) =>
        if (isSome(payload.txid) and not(state.isKnown(payload)))
          Set({ state: indexerApply(state, input, VerdictOutput(Accepted)), out: VerdictOutput(Accepted) })
        else Set()
      | _ => Set()
    }
    honestIndexerResults(state, input).union(accepted)

  /// The truthful, available answer to a lookup: the one member of the honest
  /// relation that is not `IUnavailable`.
  pure def chainAnswer(state: IndexerState, txid: TxId): IndexerAnswer =
    if (state.txs.keys().contains(txid))
      val tx = state.txs.get(txid)
      IFound({ body: Some(tx.payload), height: answerHeight(tx.at) })
    else INotFound

  /// The tips an honest indexer may report when reports can trail the true
  /// height by up to `slack` blocks. `heights` is every height there is: the
  /// answer is a part of a fixed range, never a range with a moving bound.
  pure def honestTips(state: IndexerState, slack: int, heights: Set[Height]): Set[Height] =
    heights.filter(height => state.height - slack <= height and height <= state.height)

  // ------------------------------------------------------------------------
  // Byzantine relation
  // ------------------------------------------------------------------------

  /// The payloads a Byzantine indexer can put in a lookup answer: those it was
  /// offered, those the chain made public, and any twin of either that exists
  /// in `universe`.
  pure def servable(state: IndexerState, universe: Set[Payload]): Set[Payload] =
    val known = state.offered.union(state.published())
    universe.filter(candidate =>
      known.exists(payload => candidate == payload or areTwins(candidate, payload)))

  /// The outputs a Byzantine indexer may give: any verdict, and any lookup
  /// answer built from a payload it can serve, at any height, with or without
  /// a body.
  pure def byzIndexerOutputs(state: IndexerState, input: IndexerInput, universe: Set[Payload]): Set[IndexerOutput] =
    match input {
      | BroadcastIInput(_) =>
        Set(Accepted, AlreadyKnown, Rejected, Retryable).map(verdict => VerdictOutput(verdict))
      | LookupIInput(_) =>
        val bodies = Set(None).union(state.servable(universe).map(payload => Some(payload)))
        tuples(bodies, WIRE_HEIGHTS)
          .map(((body, claimed)) => AnswerOutput(IFound({ body: body, height: claimed })))
          .union(Set(AnswerOutput(INotFound), AnswerOutput(IUnavailable)))
          .union(honestIndexerOutputs(state, input))
      | AdvanceIInput => honestIndexerOutputs(state, input)
      | MineIInput(_) => honestIndexerOutputs(state, input)
    }

  /// The transitions of a Byzantine indexer. On a broadcast the verdict it
  /// reports and what it does with the transaction are independent: it may
  /// relay a transaction it claims to have rejected, or sit on one it claims to
  /// have accepted. It cannot make the network take a transaction a node would
  /// refuse: the chain itself stays honest.
  pure def byzIndexerResults(state: IndexerState, input: IndexerInput, universe: Set[Payload]): Set[IndexerResult] =
    val outputs = byzIndexerOutputs(state, input, universe)
    match input {
      | BroadcastIInput(payload) =>
        val withheld = indexerApply(state, input, VerdictOutput(Retryable))
        val relayed = indexerApply(state, input, VerdictOutput(Accepted))
        val effects = if (state.isAcceptable(payload)) Set(withheld, relayed) else Set(withheld)
        tuples(effects, outputs).map(((effect, output)) => { state: effect, out: output })
      | _ => outputs.map(output => { state: indexerApply(state, input, output), out: output })
    }

  /// The tips a Byzantine indexer may report: any height there is.
  pure def byzTips(heights: Set[Height]): Set[Height] =
    heights
}

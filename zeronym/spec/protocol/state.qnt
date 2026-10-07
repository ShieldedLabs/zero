// -*- mode: Bluespec; -*-

/// The whole system as one value: the components' states, the network between
/// them, and what each outside party has seen.
///
/// Nothing here decides anything. The functions in this module put a
/// component's output where it goes (a frame into the soup, an answer into the
/// wallet's log, a forwarded transaction in front of the operator) and derive
/// the views the properties are stated over.
module state {
  import basicSpells.* from "./spells/basicSpells"
  import soup.* from "./spells/soup"
  import types.* from "./types"
  import wire.* from "./wire"
  import indexer.* from "./indexer"
  import hub.* from "./hub"
  import shim.* from "./shim"

  // ------------------------------------------------------------------------
  // The system
  // ------------------------------------------------------------------------

  type Mail = Envelope[Addr, Msg]
  type Net = Soup[Addr, Msg]

  /// The wallet: the answers it has been given, in the order it got them, and
  /// how many requests of each kind it has made.
  type Wallet = { log: List[WalletEvent], sends: int, gets: int }

  /// The third party: a client of the hubs' public address that is not the
  /// shim. `txids` are the transaction ids it has learned out of band; `own`
  /// are payloads of its own making.
  type ThirdParty = { txids: Set[TxId], own: Set[Payload], nextNonce: Nonce, requests: int }

  /// - `polled`: the hubs whose cadence loop has asked for the tip since the
  ///   last block arrived.
  /// - `flightBlocks`: for each hub, the blocks that have arrived since its
  ///   flush in flight began; zero while no flush is in flight. Both are the
  ///   environment's bookkeeping of time; no component reads them.
  /// - `operator`: every transaction the shim has handed the operator's
  ///   indexer. The operator is assumed to publish nothing itself.
  /// - `disclosed`: payloads a Byzantine component has revealed. It is written
  ///   by the disclosure step and by nothing else.
  type System = {
    indexer: IndexerState,
    hubs: HubId -> HubState,
    polled: Set[HubId],
    flightBlocks: HubId -> int,
    shim: ShimState,
    net: Net,
    wallet: Wallet,
    operator: Set[Payload],
    thirdParty: ThirdParty,
    disclosed: Set[Payload],
  }

  /// The system at rest: the chain at `height`, every hub started and waiting
  /// for its first tip, nothing sent.
  pure def initialSystem(config: Config, params: HubParams, height: Height): System = {
    indexer: initialIndexer(height),
    hubs: config.hubs.indices().map(i => config.hubs[i]).mapBy(_ => startingHub(params)),
    polled: Set(),
    flightBlocks: config.hubs.indices().map(i => config.hubs[i]).mapBy(_ => 0),
    shim: initialShim(config.hubs, config.submitMode),
    net: Set(),
    wallet: { log: [], sends: 0, gets: 0 },
    operator: Set(),
    thirdParty: { txids: Set(), own: config.tpPayloads, nextNonce: 0, requests: 0 },
    disclosed: Set(),
  }

  /// The step that produced a state, with the choices that identify it. It
  /// names the input a component was given, never the transition the
  /// component then took.
  type Label =
    | Init
    | WalletSend({ input: SendInput, handedOver: int })
    | WalletGet({ query: TxId, start: int })
    | ShimReceive(Mail)
    | ShimLookupTimeout(Nonce)
    | ShimAckTimeout(Nonce)
    | HubReceive({ hub: HubId, mail: Mail })
    | HubObserveTip({ hub: HubId, tip: Height })
    | HubTipStale({ hub: HubId, estimate: Height })
    | HubFlushBegin(HubId)
    | IndexerVerdict({ hub: HubId, payload: Payload })
    | HubFlushEnd(HubId)
    | HubBeginDrain(HubId)
    | HubCrash(HubId)
    | HubRestart(HubId)
    | ChainAdvance
    | ChainMine(TxId)
    | ThirdPartyLearnsTxid(TxId)
    | ThirdPartyLookup({ txid: TxId, hub: HubId })
    | ThirdPartySubmit({ payload: Payload, hub: HubId })
    | ByzDisclose(Payload)

  /// What an observer of the run has recorded. It is not protocol state: no
  /// component reads it, and it is derived at every step from the states
  /// before and after, never from what a component reports about itself.
  ///
  /// - `everQueued`: every payload that has been in a hub's queue.
  /// - `admitted`: for a payload's first entry into a hub's queue, the true
  ///   chain height at that moment and the tip the hub believed in.
  /// - `offers`: every time a flush put a payload in flight: the true chain
  ///   height, the requeues the entry had had (`attempt`), and how many times
  ///   this hub had offered the payload before (`nth`).
  /// - `verdicts`: every time an entry in flight got its answer from the
  ///   indexer: the true chain height at that moment, which offer of the
  ///   payload it answers (`nth`), and whether a node judged the transaction
  ///   (`final`) or nothing did and it was set aside for requeue.
  /// - `windows`: for each lookup the shim has sent, the answers that were
  ///   true at the hub it asked at some point while it waited.
  /// - `refusals`: the admission refusals that have been sent.
  /// - `dropped`: entries a requeue gave up on, with the requeues they had had.
  type Offer = { hub: HubId, payload: Payload, height: Height, attempt: int, nth: int }
  type Judgement = { hub: HubId, payload: Payload, height: Height, nth: int, final: bool }
  type Audit = {
    everQueued: HubId -> Set[Payload],
    admitted: (HubId, Payload) -> { height: Height, tip: Height },
    offers: Set[Offer],
    verdicts: Set[Judgement],
    windows: Nonce -> Set[LookupObs],
    refusals: Set[Refusal],
    dropped: Set[{ hub: HubId, payload: Payload, attempts: int }],
  }

  // ------------------------------------------------------------------------
  // Views
  // ------------------------------------------------------------------------

  pure def hubIds(s: System): Set[HubId] =
    s.hubs.keys()

  pure def height(s: System): Height =
    s.indexer.height

  /// The payloads in `hub`'s queue.
  pure def queuedAt(s: System, hub: HubId): Set[Payload] =
    s.hubs.get(hub).queued()

  /// The payloads `hub` has out with a flush.
  pure def inFlightAt(s: System, hub: HubId): Set[Payload] =
    s.hubs.get(hub).inFlight()

  /// Where `txid` stands on the chain.
  pure def onChain(s: System, txid: TxId): Inclusion =
    s.indexer.inclusion(txid)

  /// The answers the wallet has been given, without their order.
  pure def events(s: System): Set[WalletEvent] =
    s.wallet.log.indices().map(i => s.wallet.log[i])

  /// The payloads the wallet has been told were diverted.
  pure def toldOk(s: System): Set[Payload] =
    s.events().fold(Set(), (acc, event) =>
      match event {
        | Sent(sent) =>
          match sent.input {
            | Clean(payload) => if (sent.obs == SentOk) acc.union(Set(payload)) else acc
            | _ => acc
          }
        | _ => acc
      })

  /// The submissions `client` has addressed to `hub`, as (nonce, payload).
  pure def submissions(s: System, client: Addr, hub: HubId): Set[(Nonce, Payload)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | Submit(submit) =>
          if (mail.src == client and mail.dst == HubAddr(hub))
            acc.union(Set((submit.nonce, submit.payload)))
          else acc
        | _ => acc
      })

  /// The acks `hub` has sent `client`, as (nonce, ack).
  pure def acks(s: System, hub: HubId, client: Addr): Set[(Nonce, WireAck)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | Ack(ack) =>
          if (mail.src == HubAddr(hub) and mail.dst == client) acc.union(Set((ack.nonce, ack.ack)))
          else acc
        | _ => acc
      })

  /// The payloads `hub` has acknowledged as accepted, to any client.
  pure def ackedAt(s: System, hub: HubId): Set[Payload] =
    Set(ShimAddr, ThirdPartyAddr).map(client =>
      tuples(s.submissions(client, hub), s.acks(hub, client))
        .filter(((submit, ack)) => submit._1 == ack._1 and ack._2 == WAccepted)
        .map(((submit, _)) => submit._2)
    ).flatten()

  /// The lookups `client` has addressed to hubs, as (nonce, hub, txid).
  pure def lookups(s: System, client: Addr): Set[(Nonce, HubId, TxId)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | Lookup(lookup) =>
          match mail.dst {
            | HubAddr(hub) =>
              if (mail.src == client) acc.union(Set((lookup.nonce, hub, lookup.txid))) else acc
            | _ => acc
          }
        | _ => acc
      })

  /// The lookup replies addressed to `client`, as (nonce, hub, reply).
  pure def replies(s: System, client: Addr): Set[(Nonce, HubId, WireReply)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | LookupReply(reply) =>
          match mail.src {
            | HubAddr(hub) =>
              if (mail.dst == client) acc.union(Set((reply.nonce, hub, reply.reply))) else acc
            | _ => acc
          }
        | _ => acc
      })

  /// The transaction bodies in lookup replies addressed to `client`.
  pure def repliedBodies(s: System, client: Addr): Set[Payload] =
    s.replies(client).fold(Set(), (acc, reply) =>
      match reply._3 {
        | WFound(found) =>
          match found.body {
            | Some(payload) => acc.union(Set(payload))
            | None => acc
          }
        | _ => acc
      })

  // ------------------------------------------------------------------------
  // What each party knows
  // ------------------------------------------------------------------------

  /// The transaction ids the third party knows: those it learned out of band
  /// and everything the chain has made public.
  pure def tpTxids(s: System): Set[TxId] =
    s.thirdParty.txids.union(s.indexer.txs.keys())

  /// The payloads the third party did not make and yet has: the bodies of
  /// replies sent to it, whatever reached the operator, and whatever a
  /// Byzantine component disclosed. The chain is left out on purpose: what is
  /// published is public.
  ///
  /// This is derived from what the third party can observe. Nothing updates it
  /// when a transaction is published, so a property over it constrains what
  /// the hubs put in their replies.
  pure def tpLearned(s: System): Set[Payload] =
    s.repliedBodies(ThirdPartyAddr).union(s.operator).union(s.disclosed).exclude(s.thirdParty.own)

  /// Every payload the third party has: what it learned, what the chain
  /// published, and its own.
  pure def tpPayloads(s: System): Set[Payload] =
    s.tpLearned().union(s.indexer.published()).union(s.thirdParty.own)

  /// The payloads the wallet has handed the shim: every send that was
  /// answered, and every send still waiting for a hub's decision.
  pure def sentByWallet(s: System): Set[Payload] =
    val answered = s.events().fold(Set(), (acc, event) =>
      match event {
        | Sent(done) =>
          match done.input {
            | Clean(payload) => acc.union(Set(payload))
            | _ => acc
          }
        | _ => acc
      })
    val waiting = s.shim.waiters.keys().fold(Set(), (acc, nonce) =>
      match s.shim.waiters.get(nonce) {
        | AckWaiter(waiter) => acc.union(Set(waiter.payload))
        | _ => acc
      })
    answered.union(waiting)

  /// The payloads the shim has seen in the clear: every transaction the wallet
  /// handed it, and every body a hub returned to it.
  pure def seenByShim(s: System): Set[Payload] =
    s.sentByWallet().union(s.repliedBodies(ShimAddr))

  /// The payloads addressed to `hub` by anyone: what it has received or may
  /// yet receive.
  pure def seenByHub(s: System, hub: HubId): Set[Payload] =
    Set(ShimAddr, ThirdPartyAddr).map(client =>
      s.submissions(client, hub).map(submit => submit._2)).flatten()

  /// What the Byzantine components, taken together, are able to reveal.
  pure def disclosable(s: System, roles: Roles): Set[Payload] =
    (if (roles.shim == Byzantine) s.seenByShim() else Set())
      .union(if (roles.indexer == Byzantine) s.indexer.offered else Set())
      .union(s.hubIds().filter(hub => roles.hubs.get(hub) == Byzantine)
        .map(hub => s.seenByHub(hub)).flatten())

  // ------------------------------------------------------------------------
  // Putting outputs where they go
  // ------------------------------------------------------------------------

  pure def logged(s: System, event: WalletEvent): System =
    { ...s, wallet: { ...s.wallet, log: s.wallet.log.append(event) } }

  pure def posted(s: System, mails: Set[Mail]): System =
    { ...s, net: s.net.sendAll(mails) }

  /// The system after the shim took `result`. `via` is the nonce of the frame
  /// that was delivered to it, if the step was a delivery: it is recorded with
  /// a lookup answer so the answer can be traced to the hub that gave it.
  pure def shimStepped(s: System, result: ShimResult, via: Option[Nonce]): System =
    val stepped = { ...s, shim: result.state }
    match result.out {
      | ForwardOutput(payload) =>
        { ...stepped, operator: stepped.operator.union(Set(payload)) }
          .logged(Sent({ input: Clean(payload), obs: SentToOperator }))
      | DivertedOutput(diverted) =>
        val dispatched = stepped.posted(diverted.frames.map(frame =>
          { src: ShimAddr, dst: HubAddr(frame.hub), msg: frame.msg }))
        match diverted.told {
          | Some(obs) => dispatched.logged(Sent({ input: Clean(diverted.payload), obs: obs }))
          | None => dispatched
        }
      | SendDoneOutput(done) => stepped.logged(Sent(done))
      | LookupSentOutput(sent) =>
        stepped.posted(Set({ src: ShimAddr, dst: HubAddr(sent.frame.hub), msg: sent.frame.msg }))
      | LookupDoneOutput(done) =>
        stepped.logged(Got({ query: done.query, obs: done.result, via: via }))
      | NoShimOutput => stepped
      | ShimErrorOutput(_) => stepped
    }

  /// The system after `hub` moved to `state` on its own schedule. A hub with
  /// no flush in flight has no flight time.
  pure def hubMoved(s: System, hub: HubId, state: HubState): System =
    { ...s,
      hubs: s.hubs.set(hub, state),
      flightBlocks: if (state.flush == Idle) s.flightBlocks.set(hub, 0) else s.flightBlocks,
    }

  /// The system after one more block: the chain is a block higher, no hub has
  /// asked for the new tip yet, and every flush in flight has been out one
  /// block longer.
  pure def blockArrived(s: System): System =
    { ...s,
      indexer: indexerApply(s.indexer, AdvanceIInput, NoIndexerOutput),
      polled: Set(),
      flightBlocks: s.hubs.keys().mapBy(hub =>
        if (s.hubs.get(hub).flush == Idle) 0 else s.flightBlocks.get(hub) + 1),
    }

  /// The system after `hub` took `result` on a frame from `client`. Its ack or
  /// lookup reply is put on the wire and sent back; nothing else a hub outputs
  /// leaves it.
  pure def hubReplied(s: System, hub: HubId, client: Addr, result: HubResult): System =
    val stepped = { ...s, hubs: s.hubs.set(hub, result.state) }
    match result.out {
      | AckOutput(ack) =>
        stepped.posted(Set({
          src: HubAddr(hub), dst: client, msg: Ack({ nonce: ack.nonce, ack: renderAck(ack.kind) }),
        }))
      | LookupReplyOutput(reply) =>
        stepped.posted(Set({
          src: HubAddr(hub), dst: client,
          msg: LookupReply({ nonce: reply.nonce, reply: render(reply.outcome) }),
        }))
      | _ => stepped
    }

  /// The hub input a request frame becomes. `answer` is what the hub's indexer
  /// would say to a lookup. Reply frames are not requests and give nothing.
  pure def requestInput(msg: Msg, answer: IndexerAnswer): Set[HubInput] =
    match msg {
      | Submit(submit) => Set(SubmitHInput(submit))
      | Lookup(lookup) => Set(LookupHInput({ nonce: lookup.nonce, txid: lookup.txid, answer: answer }))
      | _ => Set()
    }

  /// The wallet makes one more request of a kind.
  pure def countSend(s: System): System =
    { ...s, wallet: { ...s.wallet, sends: s.wallet.sends + 1 } }

  pure def countGet(s: System): System =
    { ...s, wallet: { ...s.wallet, gets: s.wallet.gets + 1 } }

  /// The third party sends `msg` to `hub`, under a nonce of its own.
  pure def thirdPartySent(s: System, hub: HubId, msg: Msg): System =
    { ...s,
      thirdParty: { ...s.thirdParty,
        nextNonce: s.thirdParty.nextNonce + 1,
        requests: s.thirdParty.requests + 1,
      },
    }.posted(Set({ src: ThirdPartyAddr, dst: HubAddr(hub), msg: msg }))
}

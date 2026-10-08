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
  import abstractHub.* from "./abstractHub"
  import shim.* from "./shim"

  // ------------------------------------------------------------------------
  // The system
  // ------------------------------------------------------------------------

  type Mail = Envelope[Addr, Msg]
  type Net = Soup[Addr, Msg]

  /// The wallet: the answers it has been given, in the order it got them, and
  /// how many requests of each kind it has made.
  type Wallet = { log: List[WalletEvent], sends: int, gets: int }

  /// The third party: a client of the hub's public address that is not the
  /// shim. `txids` are the transaction ids it has learned out of band; `own`
  /// are payloads of its own making.
  type ThirdParty = { txids: Set[TxId], own: Set[Payload], nextNonce: Nonce, requests: int }

  /// - `hub`: the hub as the protocol sees it (`abstractHub.qnt`).
  /// - `operator`: every transaction the shim has handed the operator's
  ///   indexer. The operator is assumed to publish nothing itself.
  /// - `disclosed`: payloads a Byzantine component has revealed. It is written
  ///   by the disclosure step and by nothing else.
  type System = {
    indexer: IndexerState,
    hub: AHub,
    shim: ShimState,
    net: Net,
    wallet: Wallet,
    operator: Set[Payload],
    thirdParty: ThirdParty,
    disclosed: Set[Payload],
  }

  /// The system at rest: the chain at `height`, the hub empty, nothing sent.
  pure def initialSystem(config: Config, height: Height): System = {
    indexer: initialIndexer(height),
    hub: emptyAHub,
    shim: initialShim,
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
    | WalletSend({ input: SendInput, handedOver: bool })
    | WalletGet(TxId)
    | ShimReceive(Mail)
    | ShimLookupTimeout(Nonce)
    | HubReceive(Mail)
    | HubTake
    | IndexerVerdict(Payload)
    | HubReturn(Set[Payload])
    | HubLose
    | ChainAdvance
    | ChainMine(TxId)
    | ThirdPartyLearnsTxid(TxId)
    | ThirdPartyLookup(TxId)
    | ThirdPartySubmit(Payload)
    | ByzDisclose(Payload)

  /// What an observer of the run has recorded. It is not protocol state: no
  /// component reads it, and it is derived at every step from the states
  /// before and after, never from what a component reports about itself.
  ///
  /// - `everQueued`: every payload that has been in the hub's queue.
  /// - `windows`: for each lookup the shim has sent, the answers that were
  ///   true at the hub at some point while it waited.
  type Audit = {
    everQueued: Set[Payload],
    windows: Nonce -> Set[LookupObs],
  }

  // ------------------------------------------------------------------------
  // Views
  // ------------------------------------------------------------------------

  pure def height(s: System): Height =
    s.indexer.height

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

  /// The submissions `client` has addressed to the hub, as (nonce, payload).
  pure def submissions(s: System, client: Addr): Set[(Nonce, Payload)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | Submit(submit) =>
          if (mail.src == client and mail.dst == HubAddr) acc.union(Set((submit.nonce, submit.payload)))
          else acc
        | _ => acc
      })

  /// The acks the hub has sent `client`, as (nonce, ack).
  pure def acks(s: System, client: Addr): Set[(Nonce, WireAck)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | Ack(ack) =>
          if (mail.src == HubAddr and mail.dst == client) acc.union(Set((ack.nonce, ack.ack)))
          else acc
        | _ => acc
      })

  /// The payloads the hub has acknowledged as accepted, to any client.
  pure def acked(s: System): Set[Payload] =
    Set(ShimAddr, ThirdPartyAddr).map(client =>
      tuples(s.submissions(client), s.acks(client))
        .filter(((submit, ack)) => submit._1 == ack._1 and ack._2 == WAccepted)
        .map(((submit, _)) => submit._2)
    ).flatten()

  /// The lookups `client` has addressed to the hub, as (nonce, txid).
  pure def lookups(s: System, client: Addr): Set[(Nonce, TxId)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | Lookup(lookup) =>
          if (mail.src == client and mail.dst == HubAddr) acc.union(Set((lookup.nonce, lookup.txid)))
          else acc
        | _ => acc
      })

  /// The lookup replies the hub has addressed to `client`, as (nonce, reply).
  pure def replies(s: System, client: Addr): Set[(Nonce, WireReply)] =
    s.net.fold(Set(), (acc, mail) =>
      match mail.msg {
        | LookupReply(reply) =>
          if (mail.src == HubAddr and mail.dst == client) acc.union(Set((reply.nonce, reply.reply)))
          else acc
        | _ => acc
      })

  /// The transaction bodies in lookup replies addressed to `client`.
  pure def repliedBodies(s: System, client: Addr): Set[Payload] =
    s.replies(client).fold(Set(), (acc, reply) =>
      match reply._2 {
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
  /// the hub puts in its replies.
  pure def tpLearned(s: System): Set[Payload] =
    s.repliedBodies(ThirdPartyAddr).union(s.operator).union(s.disclosed).exclude(s.thirdParty.own)

  /// Every payload the third party has: what it learned, what the chain
  /// published, and its own.
  pure def tpPayloads(s: System): Set[Payload] =
    s.tpLearned().union(s.indexer.published()).union(s.thirdParty.own)

  /// The payloads the wallet has handed the shim: every send is answered at
  /// once.
  pure def sentByWallet(s: System): Set[Payload] =
    s.events().fold(Set(), (acc, event) =>
      match event {
        | Sent(done) =>
          match done.input {
            | Clean(payload) => acc.union(Set(payload))
            | _ => acc
          }
        | _ => acc
      })

  /// The payloads addressed to the hub by anyone: what it has received or may
  /// yet receive.
  pure def seenByHub(s: System): Set[Payload] =
    Set(ShimAddr, ThirdPartyAddr).map(client => s.submissions(client).map(submit => submit._2)).flatten()

  /// What the Byzantine components, taken together, are able to reveal.
  pure def disclosable(s: System, roles: Roles): Set[Payload] =
    (if (roles.indexer == Byzantine) s.indexer.offered else Set())
      .union(if (roles.hub == Byzantine) s.seenByHub() else Set())

  // ------------------------------------------------------------------------
  // Putting outputs where they go
  // ------------------------------------------------------------------------

  pure def logged(s: System, event: WalletEvent): System =
    { ...s, wallet: { ...s.wallet, log: s.wallet.log.append(event) } }

  pure def posted(s: System, mails: Set[Mail]): System =
    { ...s, net: s.net.sendAll(mails) }

  /// The system after the shim took `result`. `via` is the nonce of the frame
  /// that was delivered to it, if the step was a delivery: it is recorded with
  /// a lookup answer so the answer can be traced to the reply that gave it.
  pure def shimStepped(s: System, result: ShimResult, via: Option[Nonce]): System =
    val stepped = { ...s, shim: result.state }
    match result.out {
      | ForwardOutput(payload) =>
        { ...stepped, operator: stepped.operator.union(Set(payload)) }
          .logged(Sent({ input: Clean(payload), obs: SentToOperator }))
      | DivertedOutput(diverted) =>
        stepped.posted(Set({ src: ShimAddr, dst: HubAddr, msg: diverted.frame }))
          .logged(Sent({ input: Clean(diverted.payload), obs: diverted.told }))
      | SendDoneOutput(done) => stepped.logged(Sent(done))
      | LookupSentOutput(frame) => stepped.posted(Set({ src: ShimAddr, dst: HubAddr, msg: frame }))
      | LookupDoneOutput(done) =>
        stepped.logged(Got({ query: done.query, obs: done.result, via: via }))
      | NoShimOutput => stepped
      | ShimErrorOutput(_) => stepped
    }

  /// The system after one more block.
  pure def blockArrived(s: System): System =
    { ...s, indexer: indexerApply(s.indexer, AdvanceIInput, NoIndexerOutput) }

  /// The system after the hub answered `answer` to the request `mail`. The
  /// reply goes back to its sender under its nonce.
  pure def hubReplied(s: System, mail: Mail, answer: AAnswer): System =
    val nonce = match mail.msg {
      | Submit(submit) => submit.nonce
      | Lookup(lookup) => lookup.nonce
      | _ => 0
    }
    val msg = match answer.reply {
      | AAck(ack) => Ack({ nonce: nonce, ack: ack })
      | AWire(reply) => LookupReply({ nonce: nonce, reply: reply })
    }
    { ...s, hub: answer.hub }.posted(Set({ src: HubAddr, dst: mail.src, msg: msg }))

  /// The request a frame carries. `answer` is what the hub's indexer would say
  /// to a lookup. Reply frames are not requests and give nothing.
  pure def requestOf(msg: Msg, answer: IndexerAnswer): Set[ARequest] =
    match msg {
      | Submit(submit) => Set(ASubmit(submit.payload))
      | Lookup(lookup) => Set(ALookup({ txid: lookup.txid, answer: answer }))
      | _ => Set()
    }

  /// The wallet makes one more request of a kind.
  pure def countSend(s: System): System =
    { ...s, wallet: { ...s.wallet, sends: s.wallet.sends + 1 } }

  pure def countGet(s: System): System =
    { ...s, wallet: { ...s.wallet, gets: s.wallet.gets + 1 } }

  /// The third party sends `msg` to the hub, under a nonce of its own.
  pure def thirdPartySent(s: System, msg: Msg): System =
    { ...s,
      thirdParty: { ...s.thirdParty,
        nextNonce: s.thirdParty.nextNonce + 1,
        requests: s.thirdParty.requests + 1,
      },
    }.posted(Set({ src: ThirdPartyAddr, dst: HubAddr, msg: msg }))
}

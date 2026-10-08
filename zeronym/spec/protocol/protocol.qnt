// -*- mode: Bluespec; -*-

/// The zeronym protocol as a state machine: a wallet, a shim, one hub, the
/// chain behind the hub's indexer, an unreliable network between them, and a
/// third party that can reach the hub.
///
/// The spec checks one hub; production runs one or more, replicated: every
/// shim sends every submission to every hub, and each hub that receives a
/// migration queues and broadcasts it.
///
/// What is assumed:
///
/// - Roles. The shim and the hub run in enclaves and are honest in the
///   baseline. The shim is honest in every configuration; the hub and its
///   indexer can each be made Byzantine through `cfg.roles`. A Byzantine
///   component draws its transitions from a wider relation and is not marked
///   in any other way.
/// - Network. Frames may be lost, duplicated, delayed and reordered. They
///   cannot be forged or read in transit.
/// - Third party. A client of the hub's public address. It looks up txids it
///   knows and submits payloads it has learned or the chain has published. It cannot read or
///   forge frames, so it does not know a nonce and cannot answer the shim.
/// - Nonces are unique. A counter stands for an unguessable random value.
/// - Chain. A transaction's status only moves forward: no reorg of an
///   included transaction, no mempool eviction.
/// - Hub. The hub is abstract (`abstractHub.qnt`): a queue, the entries out
///   with a flush, and its replies, with no schedule. `hubTest` checks that
///   the real hub function refines it. Its timing is the hub specification's.
/// - Time. There is no clock. A timeout is an event that may happen at any
///   moment.
///
/// How it is built. This module holds no protocol logic. Every step picks an
/// input, hands it to one component (`shim`, the abstract hub, or the indexer
/// relation) and puts the output where it goes. Each step exists in two
/// forms: `xWith(..)`, which takes every choice as a parameter and is what a
/// scripted run is made of, and `x`, which picks the choices and is what
/// `step` is made of. All state is written by `commit`, so no action has a
/// frame condition.
module protocol {
  import basicSpells.* from "./spells/basicSpells"
  import soup.* from "./spells/soup"
  import types.* from "./types"
  import wire.* from "./wire"
  import indexer.* from "./indexer"
  import abstractHub.* from "./abstractHub"
  import shim.* from "./shim"

  // ========================================================================
  // Transactions and configurations
  // ========================================================================

  // ------------------------------------------------------------------------
  // Transactions
  // ------------------------------------------------------------------------
  //
  // The expiries are set against the hub specification's schedule; the
  // protocol does not read them.

  pure def orchard(id: str, created: Height, expiry: Height): Payload =
    { id: id, txid: Some(id), created: created, expiry: Some(expiry), class: OrchardTouching, oversize: false }

  /// Two migrations from supported wallets, built at heights 2 and 4.
  pure val early = orchard("early", 2, 9)
  pure val late = orchard("late", 4, 11)
  /// A migration whose wallet set its expiry tighter than the supported floor.
  pure val tight = orchard("tight", 2, 5)
  /// Bytes the shim cannot parse and neither can a hub: no txid, no expiry.
  pure val junk: Payload =
    { id: "junk", txid: None, created: 1, expiry: None, class: Unparseable, oversize: false }
  /// A transaction that is not a migration.
  pure val plain: Payload =
    { id: "plain", txid: Some("plain"), created: 1, expiry: Some(9), class: PassThrough, oversize: false }
  /// Other bytes with the txid of `early`.
  pure val earlyTwin = { ...early, id: "early-twin" }

  // ------------------------------------------------------------------------
  // Configurations
  // ------------------------------------------------------------------------

  pure val allHonest: Roles = { hub: Honest, indexer: Honest }

  /// One hub, the mixnet transport, every component honest.
  pure val baseline: Config = {
    payloads: Set(early, late, tight, junk, plain),
    twins: Set(earlyTwin),
    maxRequests: 3,
    roles: allHonest,
  }

  // One Byzantine component at a time.
  pure val byzHub = { ...baseline, roles: { ...allHonest, hub: Byzantine } }
  pure val byzIndexer = { ...baseline, roles: { ...allHonest, indexer: Byzantine } }

  // ========================================================================
  // The system
  //
  // The whole system as one value: the components' states, the network
  // between them, and what each outside party has seen. Nothing here decides
  // anything: these functions put a component's output where it goes and
  // derive the views the properties are stated over.
  // ========================================================================

  // ------------------------------------------------------------------------
  // The system
  // ------------------------------------------------------------------------

  type Mail = Envelope[Addr, Msg]
  type Net = Soup[Addr, Msg]

  /// The wallet: the answers it has been given, in the order it got them, and
  /// how many requests of each kind it has made.
  type Wallet = { log: List[WalletEvent], sends: int, gets: int }

  /// The third party: a client of the hub's public address that is not the
  /// shim. `txids` are the transaction ids it has learned out of band.
  type ThirdParty = { txids: Set[TxId], nextNonce: Nonce, requests: int }

  /// - `hub`: the hub as the protocol sees it (`abstractHub.qnt`).
  /// - `operator`: every transaction the shim has handed the operator's
  ///   indexer. The operator is assumed to publish nothing itself.
  type System = {
    indexer: IndexerState,
    hub: AHub,
    shim: ShimState,
    net: Net,
    wallet: Wallet,
    operator: Set[Payload],
    thirdParty: ThirdParty,
  }

  /// The system at rest: the chain and the hub empty, nothing sent. The
  /// protocol has no chain height, and the indexer's stays at 0.
  pure def initialSystem(config: Config): System = {
    indexer: initialIndexer(0),
    hub: emptyAHub,
    shim: initialShim,
    net: Set(),
    wallet: { log: [], sends: 0, gets: 0 },
    operator: Set(),
    thirdParty: { txids: Set(), nextNonce: 0, requests: 0 },
  }

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

  /// Where `txid` stands on the chain.
  pure def onChain(s: System, txid: TxId): Inclusion =
    s.indexer.inclusion(txid)

  /// The answers the wallet has been given, without their order.
  pure def events(s: System): Set[WalletEvent] =
    s.wallet.log.indices().map(i => s.wallet.log[i])

  /// The payloads the wallet has been told were diverted.
  pure def toldOk(s: System): Set[Payload] =
    s.events().filterMap(event =>
      match event {
        | Sent(sent) =>
          match sent.input {
            | Clean(payload) => if (sent.obs == SentOk) Some(payload) else None
            | _ => None
          }
        | _ => None
      })

  /// The frames `client` has addressed to the hub.
  pure def toHub(s: System, client: Addr): Set[Msg] =
    s.net.filter(mail => mail.src == client and mail.dst == HubAddr).map(mail => mail.msg)

  /// The frames the hub has addressed to `client`.
  pure def fromHub(s: System, client: Addr): Set[Msg] =
    s.net.filter(mail => mail.src == HubAddr and mail.dst == client).map(mail => mail.msg)

  /// The submissions `client` has addressed to the hub.
  pure def submissions(s: System, client: Addr): Set[{ nonce: Nonce, payload: Payload }] =
    s.toHub(client).filterMap(msg => match msg { | Submit(submit) => Some(submit) | _ => None })

  /// The acks the hub has sent `client`.
  pure def acks(s: System, client: Addr): Set[{ nonce: Nonce, ack: WireAck }] =
    s.fromHub(client).filterMap(msg => match msg { | Ack(ack) => Some(ack) | _ => None })

  /// The payloads the hub has acknowledged as accepted, to any client.
  pure def acked(s: System): Set[Payload] =
    Set(ShimAddr, ThirdPartyAddr).map(client =>
      tuples(s.submissions(client), s.acks(client))
        .filter(((submit, ack)) => submit.nonce == ack.nonce and ack.ack == WAccepted)
        .map(((submit, _)) => submit.payload)
    ).flatten()

  /// The lookups `client` has addressed to the hub.
  pure def lookups(s: System, client: Addr): Set[{ nonce: Nonce, txid: TxId }] =
    s.toHub(client).filterMap(msg => match msg { | Lookup(lookup) => Some(lookup) | _ => None })

  /// The lookup replies the hub has addressed to `client`.
  pure def replies(s: System, client: Addr): Set[{ nonce: Nonce, reply: WireReply }] =
    s.fromHub(client).filterMap(msg => match msg { | LookupReply(reply) => Some(reply) | _ => None })

  /// The transaction bodies in lookup replies addressed to `client`.
  pure def repliedBodies(s: System, client: Addr): Set[Payload] =
    s.replies(client).filterMap(reply =>
      match reply.reply {
        | WFound(found) => found.body
        | _ => None
      })

  // ------------------------------------------------------------------------
  // What each party knows
  // ------------------------------------------------------------------------

  /// The transaction ids the third party knows: those it learned out of band
  /// and everything the chain has made public.
  pure def tpTxids(s: System): Set[TxId] =
    s.thirdParty.txids.union(s.indexer.txs.keys())

  /// The payloads the third party did not make and yet has: the bodies of
  /// replies sent to it, and whatever reached the operator. The chain is left
  /// out on purpose: what is published is public. A Byzantine hub or indexer
  /// leaks through a lookup reply; there is no other disclosure.
  ///
  /// This is derived from what the third party can observe. Nothing updates it
  /// when a transaction is published, so a property over it constrains what
  /// the hub puts in its replies.
  pure def tpLearned(s: System): Set[Payload] =
    s.repliedBodies(ThirdPartyAddr).union(s.operator)

  /// Every payload the third party has: what it learned and what the chain
  /// published.
  pure def tpPayloads(s: System): Set[Payload] =
    s.tpLearned().union(s.indexer.published())

  /// The payloads the wallet has handed the shim: every send is answered at
  /// once.
  pure def sentByWallet(s: System): Set[Payload] =
    s.events().filterMap(event =>
      match event {
        | Sent(done) =>
          match done.input {
            | Clean(payload) => Some(payload)
            | _ => None
          }
        | _ => None
      })

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

  // ========================================================================
  // Properties
  //
  // What a wallet can rely on, what it cannot, and what must be reachable.
  // Every definition is a pure predicate over the system and the audit
  // record. Three classes, kept apart: guarantees, invariants claimed under
  // stated trust assumptions; known gaps, invariants a wallet might hope for
  // that fail even when every component is honest; witnesses, states that
  // must be reachable. A predicate reads the system and the audit record, and
  // the audit record is derived from consecutive states by `advance`, never
  // from what a component says about itself.
  // ========================================================================

  // ------------------------------------------------------------------------
  // The audit record
  // ------------------------------------------------------------------------

  /// The true answer to a lookup for `query` at the hub, read off its
  /// queue and the chain. The queue comes first, as it does in the hub.
  ///
  /// `NotFound` is the true answer for a transaction that is with a flush and
  /// not yet on the chain. That window is part of the lookup contract. The
  /// implementation says so (`zeronym/hub/src/server.rs`, on `Hub::lookup`):
  ///
  /// > Note the flush-in-flight gap: `flush()` drains the queue before
  /// > `broadcast_batch` has reached the indexer, so a lookup in that window
  /// > gets a queue miss then an indexer NOT_FOUND for a transaction it was
  /// > told height-0 about seconds earlier. Wallets poll on multi-second
  /// > intervals and tolerate a transient NOT_FOUND; a resubmit is harmless
  /// > (deduped pre-flush, already-known post-flush). Holding entries until
  /// > broadcast returns would extend how long the hub remembers a txid, which
  /// > is the wrong trade.
  ///
  /// It is also the true answer for a queued payload that does not parse.
  pure def truth(s: System, query: TxId): LookupObs =
    if (s.hub.queue.exists(payload => payload.txid == Some(query)))
      Pending
    else if (s.indexer.txs.keys().contains(query))
      val tx = s.indexer.txs.get(query)
      Tx({ payload: tx.payload, height: answerHeight(tx.at) })
    else
      NotFound

  pure val initialAudit: Audit = { everQueued: Set(), windows: Map() }

  /// The audit record after one step, from the states before and after it.
  pure def advance(audit: Audit, pre: System, post: System): Audit =
    // The lookups the shim is waiting on, or was until this step, each widen
    // their window by what is true at the hub.
    val waiting = pre.shim.waiters.keys().union(post.shim.waiters.keys())
    val windows = post.lookups(ShimAddr)
      .filter(lookup => waiting.contains(lookup.nonce))
      .fold(audit.windows, (acc, lookup) =>
        val seen = if (acc.keys().contains(lookup.nonce)) acc.get(lookup.nonce) else Set()
        acc.put(lookup.nonce, seen.union(Set(truth(post, lookup.txid)))))
    { everQueued: audit.everQueued.union(post.hub.queue), windows: windows }

  // ------------------------------------------------------------------------
  // Guarantees
  // ------------------------------------------------------------------------

  /// G1. The operator sees no migration: everything the shim hands it is a
  /// pass-through transaction.
  pure def operatorBlindIn(s: System): bool =
    s.operator.forall(payload => payload.class == PassThrough)

  /// G2. A transaction's bytes do not reach a third party before the chain has
  /// published them. Everything the third party has learned is on the chain,
  /// or was a pass-through transaction the operator was given.
  pure def queuedBytesConfidentialIn(s: System): bool =
    s.tpLearned().forall(payload =>
      or {
        s.indexer.published().contains(payload),
        s.operator.contains(payload) and payload.class == PassThrough,
      })

  /// G3. A transaction served to the wallet has the txid the wallet asked for.
  /// That is all: it need not be the bytes the wallet sent (a twin passes),
  /// and its height is whatever the hub said.
  pure def txidAuthenticityIn(s: System): bool =
    s.events().forall(event =>
      match event {
        | Got(got) =>
          match got.obs {
            | Tx(tx) => tx.payload.txid == Some(got.query)
            | _ => true
          }
        | _ => true
      })

  /// G4. Every lookup answer other than `Unavailable` was true, at the hub
  /// that gave it, at some point between the request and the answer.
  ///
  /// It is a statement about one request. It does not say that successive
  /// answers agree; see `statusNeverRegresses`.
  pure def lookupValidityPerHubIn(s: System, audit: Audit): bool =
    s.events().forall(event =>
      match event {
        | Got(got) =>
          or {
            got.obs == Unavailable,
            match got.via {
              | Some(nonce) =>
                audit.windows.keys().contains(nonce) and audit.windows.get(nonce).contains(got.obs)
              | None => false
            },
          }
        | _ => true
      })

  /// G8. An accepted ack from the hub is for a payload it had queued by the
  /// time it acked. It holds whether or not anyone waits for the ack.
  pure def ackImpliesQueuedIn(s: System, audit: Audit): bool =
    s.acked().subseteq(audit.everQueued)

  // ------------------------------------------------------------------------
  // Known gaps
  // ------------------------------------------------------------------------

  /// K2. What a wallet sees of one transaction never goes backwards: once
  /// served, it is not later pending or missing; once pending, it is not later
  /// missing. This does not hold. Replies are reordered; a published
  /// transaction can be queued again; a flush empties the queue before the
  /// chain has the batch; and the node can reject at flush.
  pure def statusNeverRegressesIn(s: System): bool =
    val log = s.wallet.log
    tuples(log.indices(), log.indices()).forall(((i, j)) =>
      i < j implies
        match log[i] {
          | Got(earlier) =>
            match log[j] {
              | Got(later) =>
                earlier.query != later.query or
                  match earlier.obs {
                    | Tx(_) =>
                      match later.obs {
                        | Tx(_) => true
                        | Unavailable => true
                        | _ => false
                      }
                    | Pending => later.obs != NotFound
                    | _ => true
                  }
              | _ => true
            }
          | _ => true
        })

  // K1. "Told ok" promises nothing about the hub. It is stated as two
  // reachable states, not as a violated invariant, because the invariant is
  // false on the ordinary success path too: the wallet is told before the hub
  // has the frame.

  /// The nonces of the shim's submissions of `payload`.
  pure def shimSubmissionsOf(s: System, payload: Payload): Set[Nonce] =
    s.submissions(ShimAddr).filter(submit => submit.payload == payload).map(submit => submit.nonce)

  /// K1a. The wallet was told ok; every submission that reached the hub was
  /// refused; the hub never queued the payload.
  pure def wToldRefusedEverywhereIn(s: System, audit: Audit): bool =
    s.toldOk().exists(payload =>
      val submitted = s.shimSubmissionsOf(payload)
      and {
        not(audit.everQueued.contains(payload)),
        submitted != Set(),
        submitted.forall(nonce => s.acks(ShimAddr).exists(ack => ack.nonce == nonce and ack.ack != WAccepted)),
      })

  /// K1b. The wallet was told ok; the hub has answered none of the frames and
  /// never queued the payload. The network may leave it so forever.
  pure def wToldNeverDeliveredIn(s: System, audit: Audit): bool =
    s.toldOk().exists(payload =>
      and {
        not(audit.everQueued.contains(payload)),
        s.shimSubmissionsOf(payload).forall(nonce => not(s.acks(ShimAddr).exists(ack => ack.nonce == nonce))),
      })

  // ------------------------------------------------------------------------
  // Witnesses
  // ------------------------------------------------------------------------

  pure def wasGiven(s: System, isIt: LookupObs => bool): bool =
    s.events().exists(event =>
      match event {
        | Got(got) => isIt(got.obs)
        | _ => false
      })

  /// W1. The wallet is told its transaction is pending.
  pure def wPendingIn(s: System): bool =
    s.wasGiven(obs => obs == Pending)

  /// W2. The wallet is served its transaction from the mempool.
  pure def wTxInMempoolIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(tx) => tx.height == AtZero
        | _ => false
      })

  /// W3. The wallet is served its transaction from a block.
  pure def wTxMinedIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(tx) => tx.height == AtMined
        | _ => false
      })

  /// W8. The accepted disclosure: a third party that knows a txid learns that
  /// it is queued at the hub. The hub withholds the bytes; it does not withhold
  /// the fact. The implementation leaves this open on purpose
  /// (`zeronym/hub/src/server.rs`, in `Hub::lookup`):
  ///
  /// > What this does NOT close: the 200-versus-NotFound distinction still
  /// > discloses that a given txid is queued here. Closing that too means
  /// > answering NotFound, which costs a wallet the ability to tell "pending"
  /// > from "never seen". That is a product decision, not a code one, and it
  /// > is left open deliberately.
  pure def wQueuedDisclosedIn(s: System): bool =
    tuples(s.lookups(ThirdPartyAddr), s.replies(ThirdPartyAddr)).exists(((lookup, reply)) =>
      lookup.nonce == reply.nonce and reply.reply == WFound({ body: None, height: AtZero }))

  /// W9. The hub holds a payload it cannot parse; the wallet that sent it asks
  /// for it and is told not found. An entry without a txid can never
  /// be hit.
  pure def wUnparseableMissedIn(s: System): bool =
    tuples(s.events(), s.lookups(ShimAddr)).exists(((event, lookup)) =>
      match event {
        | Got(got) =>
          and {
            got.obs == NotFound,
            got.via == Some(lookup.nonce),
            s.hub.queue.exists(payload => payload.txid == None and walletTxid(payload) == got.query),
          }
        | _ => false
      })

  /// W16a. The wallet is served a twin of what it sent: other bytes, same txid.
  pure def wTwinServedIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(tx) => s.toldOk().exists(sent => areTwins(sent, tx.payload))
        | _ => false
      })

  /// W16b. The wallet is served a transaction at a height that cannot be
  /// true: the chain does not have it, or has it in the mempool and the
  /// height says mined, or the height is not where it was mined.
  pure def wFalseHeightServedIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(tx) =>
          val at = match tx.payload.txid {
            | Some(txid) => s.onChain(txid)
            | None => Absent
          }
          or { at == Absent, tx.height == AtOther, tx.height == AtMined and at != Mined }
        | _ => false
      })

  // ------------------------------------------------------------------------
  // Non-vacuity: the antecedent of each guarantee is reachable
  // ------------------------------------------------------------------------

  pure def vOperatorBlindIn(s: System): bool =
    s.operator != Set()

  pure def vQueuedBytesConfidentialIn(s: System): bool =
    s.tpLearned() != Set()

  pure def vTxidAuthenticityIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(_) => true
        | _ => false
      })

  pure def vLookupValidityPerHubIn(s: System): bool =
    and {
      s.wPendingIn(),
      s.vTxidAuthenticityIn(),
      s.wasGiven(obs => obs == NotFound),
    }

  pure def vAckImpliesQueuedIn(s: System): bool =
    s.acked() != Set()

  // ========================================================================
  // The machine
  // ========================================================================


  // ------------------------------------------------------------------------
  // Configuration
  // ------------------------------------------------------------------------

  /// The configuration. Written by `initWith` and by nothing else. The names
  /// below are its fields, and are what the rest of the module uses.
  var cfg: Config

  /// The transactions a wallet may send.
  def PAYLOADS = cfg.payloads
  /// Twins of wallet transactions: other bytes with the same txid. No honest
  /// party sends one.
  def TWINS = cfg.twins

  /// The bound of the model: the wallet makes at most `MAX_REQUESTS` sends
  /// and as many lookups, as does the third party.
  def MAX_REQUESTS = cfg.maxRequests

  def ROLES = cfg.roles

  /// Every payload that exists. A Byzantine component builds its lies from it.
  pure def universeOf(c: Config): Set[Payload] = c.payloads.union(c.twins)
  def UNIVERSE = universeOf(cfg)
  /// What a wallet may send.
  def SEND_INPUTS = PAYLOADS.map(payload => Clean(payload)).union(Set(Unreadable, EmptyBody))
  /// Every transaction id there is.
  def TXIDS = txidsOf(UNIVERSE)

  /// The standing assumption, checked by every named init: payloads are told
  /// apart by their bytes, and a twin is a twin of something a wallet sends.
  pure def payloadsWellFormed(c: Config): bool =
    val universe = universeOf(c)
    and {
      universe.size() == universe.map(payload => payload.id).size(),
      c.twins.forall(twin => c.payloads.exists(payload => areTwins(twin, payload))),
    }

  /// Checked by the Byzantine inits: the universe holds a wallet payload, its
  /// twin, and a payload with another txid, so a lie can be the twin or a
  /// foreign transaction.
  pure def universeCoversLies(c: Config): bool =
    c.payloads.exists(payload => and {
      c.twins.exists(twin => areTwins(twin, payload)),
      c.payloads.exists(other => other.txid != None and other.txid != payload.txid),
    })

  // ------------------------------------------------------------------------
  // State
  // ------------------------------------------------------------------------

  /// The protocol state.
  var s: System
  /// The observer's record of the run. No step reads it.
  var audit: Audit

  /// The only writer of the variables after `initWith`.
  action commit(post: System): bool = all {
    cfg' = cfg,
    s' = post,
    audit' = advance(audit, s, post),
  }

  action initWith(c: Config): bool = all {
    cfg' = c,
    s' = initialSystem(c),
    audit' = initialAudit,
  }

  action initBaseline = all { payloadsWellFormed(baseline), initWith(baseline) }
  action initByzHub = all { payloadsWellFormed(byzHub), universeCoversLies(byzHub), initWith(byzHub) }
  action initByzIndexer = all { payloadsWellFormed(byzIndexer), universeCoversLies(byzIndexer), initWith(byzIndexer) }

  // ------------------------------------------------------------------------
  // Roles
  // ------------------------------------------------------------------------

  def hubAnswers(h: AHub, request: ARequest): Set[AAnswer] =
    match ROLES.hub {
      | Honest => honestAnswers(h, request)
      | Byzantine => byzantineAnswers(h, request, UNIVERSE)
    }

  def indexerResults(state: IndexerState, input: IndexerInput): Set[IndexerResult] =
    match ROLES.indexer {
      | Honest => heightlessIndexerResults(state, input)
      | Byzantine => byzIndexerResults(state, input, UNIVERSE)
    }

  /// What the indexer may answer a hub's lookup for the transaction `msg`
  /// asks about. A frame that is not a lookup needs no answer.
  def lookupAnswers(state: IndexerState, msg: Msg): Set[IndexerAnswer] =
    match msg {
      | Lookup(lookup) =>
        indexerResults(state, LookupIInput(lookup.txid)).filterMap(result =>
          match result.out {
            | AnswerOutput(answer) => Some(answer)
            | _ => None
          })
      | _ => Set(INotFound)
    }

  pure def isShimError(output: ShimOutput): bool =
    match output {
      | ShimErrorOutput(_) => true
      | _ => false
    }

  // ------------------------------------------------------------------------
  // Wallet
  // ------------------------------------------------------------------------

  /// The wallet sends a transaction, and the shim routes it and answers.
  /// `handedOver` is whether the transport takes the frame for the hub.
  action walletSendWith(input: SendInput, handedOver: bool): bool = all {
    s.wallet.sends < MAX_REQUESTS,
    SEND_INPUTS.contains(input),
    val result = shim(s.shim, SendTxSInput({ input: input, handedOver: handedOver }))
    all {
      not(isShimError(result.out)),
      commit(s.countSend().shimStepped(result, None)),
    },
  }

  action walletSend = all {
    s.wallet.sends < MAX_REQUESTS,
    {
      nondet input = oneOf(SEND_INPUTS)
      nondet handedOver = oneOf(Set(true, false))
      walletSendWith(input, handedOver)
    },
  }

  /// The txids of the transactions the wallet has handed the shim. A wallet
  /// asks only about its own transactions.
  def walletTxids: Set[TxId] =
    s.sentByWallet().map(payload => walletTxid(payload))

  /// The wallet asks for a transaction it has sent. The shim asks the hub.
  action walletGetWith(query: TxId): bool = all {
    s.wallet.gets < MAX_REQUESTS,
    walletTxids.contains(query),
    val result = shim(s.shim, GetTxSInput(query))
    all {
      not(isShimError(result.out)),
      commit(s.countGet().shimStepped(result, None)),
    },
  }

  action walletGet = all {
    s.wallet.gets < MAX_REQUESTS,
    walletTxids != Set(),
    {
      nondet query = oneOf(walletTxids)
      walletGetWith(query)
    },
  }

  // ------------------------------------------------------------------------
  // Shim
  // ------------------------------------------------------------------------

  /// The frames whose delivery to the shim would do something: replies under
  /// a nonce it is waiting on. Any other frame is dropped without effect.
  def shimDeliverable: Set[Mail] =
    s.net.inbox(ShimAddr).filter(mail =>
      match nonceOf(FrameSInput(mail.msg)) {
        | Some(nonce) => s.shim.hasWaiter(nonce)
        | None => false
      })

  /// The network delivers a frame to the shim.
  action shimReceiveWith(mail: Mail): bool = all {
    shimDeliverable.contains(mail),
    val result = shim(s.shim, FrameSInput(mail.msg))
    all {
      not(isShimError(result.out)),
      commit(s.shimStepped(result, nonceOf(FrameSInput(mail.msg)))),
    },
  }

  action shimReceive = all {
    shimDeliverable != Set(),
    {
      nondet mail = oneOf(shimDeliverable)
      shimReceiveWith(mail)
    },
  }

  /// The shim gives up waiting for a lookup reply.
  action shimLookupTimeoutWith(nonce: Nonce): bool = all {
    s.shim.hasWaiter(nonce),
    val result = shim(s.shim, LookupTimeoutSInput(nonce))
    all {
      not(isShimError(result.out)),
      commit(s.shimStepped(result, None)),
    },
  }

  action shimLookupTimeout = all {
    s.shim.waiters.keys() != Set(),
    {
      nondet nonce = oneOf(s.shim.waiters.keys())
      shimLookupTimeoutWith(nonce)
    },
  }

  // ------------------------------------------------------------------------
  // Hub
  // ------------------------------------------------------------------------

  /// The request frames that may be delivered to the hub: all of them, as
  /// often as the network likes.
  def hubDeliverable: Set[Mail] =
    s.net.inbox(HubAddr).filter(mail => requestOf(mail.msg, INotFound) != Set())

  /// What the hub may answer when `mail` is delivered and its indexer would
  /// answer a lookup with `answer`.
  def hubReceipts(mail: Mail, answer: IndexerAnswer): Set[AAnswer] =
    requestOf(mail.msg, answer).map(request => hubAnswers(s.hub, request)).flatten()

  /// The network delivers a request to the hub, which answers its sender. For
  /// a lookup, `answer` is what the hub's indexer says if the queue misses.
  action hubReceiveWith(mail: Mail, answer: IndexerAnswer, result: AAnswer): bool = all {
    hubDeliverable.contains(mail),
    lookupAnswers(s.indexer, mail.msg).contains(answer),
    hubReceipts(mail, answer).contains(result),
    commit(s.hubReplied(mail, result)),
  }

  action hubReceive = all {
    hubDeliverable != Set(),
    {
      nondet mail = oneOf(hubDeliverable)
      nondet answer = oneOf(lookupAnswers(s.indexer, mail.msg))
      nondet result = oneOf(hubReceipts(mail, answer))
      hubReceiveWith(mail, answer, result)
    },
  }

  /// A flush begins: the whole queue goes out at once. A Byzantine hub keeps
  /// the schedule, so these steps are the same whatever its role.
  action hubTake = all {
    s.hub.queue != Set(),
    commit({ ...s, hub: s.hub.take() }),
  }

  /// The verdict an indexer output carries. Read only where it carries one.
  pure def verdictIn(output: IndexerOutput): Verdict =
    match output {
      | VerdictOutput(verdict) => verdict
      | _ => Retryable
    }

  /// The indexer returns its verdict on one entry out with a flush. `result`
  /// is the indexer's transition: the verdict, and what became of the
  /// transaction. A node's verdict settles the entry; a retryable one leaves it
  /// out with the flush.
  action indexerVerdictWith(payload: Payload, result: IndexerResult): bool = all {
    s.hub.held.contains(payload),
    indexerResults(s.indexer, BroadcastIInput(payload)).contains(result),
    result.out == VerdictOutput(verdictIn(result.out)),
    val hub = if (verdictIn(result.out) == Retryable) s.hub else s.hub.settle(payload)
    commit({ ...s, indexer: result.state, hub: hub }),
  }

  action indexerVerdict = all {
    s.hub.held != Set(),
    {
      nondet payload = oneOf(s.hub.held)
      nondet result = oneOf(indexerResults(s.indexer, BroadcastIInput(payload)))
      indexerVerdictWith(payload, result)
    },
  }

  /// A flush ends: of what nothing judged, `kept` goes back into the queue and
  /// the rest is dropped.
  action hubReturnWith(kept: Set[Payload]): bool = all {
    s.hub.held != Set(),
    kept.subseteq(s.hub.held),
    commit({ ...s, hub: s.hub.giveBack(kept) }),
  }

  action hubReturn = all {
    s.hub.held != Set(),
    {
      nondet kept = oneOf(s.hub.held.powerset())
      hubReturnWith(kept)
    },
  }

  /// The hub loses everything: its process dies, or exits after its final
  /// flush.
  action hubLose = all {
    s.hub != emptyAHub,
    commit({ ...s, hub: emptyAHub }),
  }

  // ------------------------------------------------------------------------
  // Chain
  // ------------------------------------------------------------------------

  /// The transactions waiting in the mempool.
  def mempool: Set[TxId] =
    s.indexer.txs.keys().filter(txid => s.onChain(txid) == InMempool)

  /// A mempool transaction is included in the current block.
  action chainMineWith(txid: TxId): bool = all {
    mempool.contains(txid),
    commit({ ...s, indexer: indexerApply(s.indexer, MineIInput(txid), NoIndexerOutput) }),
  }

  action chainMine = all {
    mempool != Set(),
    {
      nondet txid = oneOf(mempool)
      chainMineWith(txid)
    },
  }

  // ------------------------------------------------------------------------
  // Third party
  // ------------------------------------------------------------------------

  /// The txids of wallet transactions the third party has yet to learn.
  def unknownTxids: Set[TxId] =
    txidsOf(PAYLOADS).exclude(s.tpTxids())

  /// The third party learns a txid out of band, before the transaction is
  /// published. An operator can recover one from a wallet's transparent-pool
  /// queries.
  action thirdPartyLearnsTxidWith(txid: TxId): bool = all {
    unknownTxids.contains(txid),
    commit({ ...s, thirdParty: { ...s.thirdParty, txids: s.thirdParty.txids.union(Set(txid)) } }),
  }

  action thirdPartyLearnsTxid = all {
    unknownTxids != Set(),
    {
      nondet txid = oneOf(unknownTxids)
      thirdPartyLearnsTxidWith(txid)
    },
  }

  /// The third party asks the hub about a txid it knows. The lookup is not
  /// authenticated.
  action thirdPartyLookupWith(txid: TxId): bool = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpTxids().contains(txid),
    commit(s.thirdPartySent(Lookup({ nonce: s.thirdParty.nextNonce, txid: txid }))),
  }

  action thirdPartyLookup = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpTxids() != Set(),
    {
      nondet txid = oneOf(s.tpTxids())
      thirdPartyLookupWith(txid)
    },
  }

  /// The third party submits a payload it has learned or the chain has
  /// published. Submission is not authenticated either.
  action thirdPartySubmitWith(payload: Payload): bool = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpPayloads().contains(payload),
    commit(s.thirdPartySent(Submit({ nonce: s.thirdParty.nextNonce, payload: payload }))),
  }

  action thirdPartySubmit = all {
    s.thirdParty.requests < MAX_REQUESTS,
    s.tpPayloads() != Set(),
    {
      nondet payload = oneOf(s.tpPayloads())
      thirdPartySubmitWith(payload)
    },
  }

  // ------------------------------------------------------------------------
  // Step
  // ------------------------------------------------------------------------

  /// A fault: a timeout, or the hub losing what it holds.
  action faultStep = any {
    shimLookupTimeout, hubLose,
  }

  /// A step by someone outside the protocol: the third party.
  action outsiderStep = any {
    thirdPartyLearnsTxid, thirdPartyLookup, thirdPartySubmit,
  }

  /// One step of the system.
  ///
  /// Faults and outsiders are grouped so that the simulator, which chooses
  /// uniformly among the alternatives it is given, takes one of each about as
  /// often as it takes any single step of the protocol.
  action step = any {
    walletSend, walletGet,
    shimReceive,
    hubReceive, hubTake, indexerVerdict, hubReturn,
    chainMine,
    faultStep,
    outsiderStep,
  }

  // The relations below are parts of `step`. Everything one of them reaches,
  // `step` reaches, so they are sound for showing that a state is reachable
  // and for nothing else. They keep a simulation on one part of the behaviour
  // long enough to get deep into it.

  /// The protocol with no faults and no outsiders.
  action quietStep = any {
    walletSend, walletGet,
    shimReceive,
    hubReceive, hubTake, indexerVerdict, hubReturn,
    chainMine,
  }

  // ------------------------------------------------------------------------
  // Guarantees
  // ------------------------------------------------------------------------
  //
  // The predicates are defined, and documented, above, each as
  // `<name>In(..)`: a predicate over a system and an audit record. These are
  // their values in the current state, under the names the gate checks.

  val operatorBlind = operatorBlindIn(s)
  val queuedBytesConfidential = queuedBytesConfidentialIn(s)
  val txidAuthenticity = txidAuthenticityIn(s)
  val lookupValidityPerHub = lookupValidityPerHubIn(s, audit)
  val ackImpliesQueued = ackImpliesQueuedIn(s, audit)

  // ------------------------------------------------------------------------
  // Known gaps: invariants that do not hold
  // ------------------------------------------------------------------------

  val statusNeverRegresses = statusNeverRegressesIn(s)

  // ------------------------------------------------------------------------
  // Witnesses
  // ------------------------------------------------------------------------

  val wToldRefusedEverywhere = wToldRefusedEverywhereIn(s, audit)
  val wToldNeverDelivered = wToldNeverDeliveredIn(s, audit)

  val wPending = wPendingIn(s)
  val wTxInMempool = wTxInMempoolIn(s)
  val wTxMined = wTxMinedIn(s)
  val wQueuedDisclosed = wQueuedDisclosedIn(s)
  val wUnparseableMissed = wUnparseableMissedIn(s)
  val wTwinServed = wTwinServedIn(s)
  val wFalseHeightServed = wFalseHeightServedIn(s)

  // Non-vacuity: the antecedent of each guarantee is reachable.
  val vOperatorBlind = vOperatorBlindIn(s)
  val vQueuedBytesConfidential = vQueuedBytesConfidentialIn(s)
  val vTxidAuthenticity = vTxidAuthenticityIn(s)
  val vLookupValidityPerHub = vLookupValidityPerHubIn(s)
  val vAckImpliesQueued = vAckImpliesQueuedIn(s)

  // ------------------------------------------------------------------------
  // Run vocabulary
  // ------------------------------------------------------------------------
  //
  // Shorthands for scripted runs. Each is a `...With` step with the choices an
  // honest component and a truthful, reachable indexer would make, or a short
  // sequence of such steps. A run that needs another choice (a Byzantine
  // transition, an indexer that is down) uses the `...With` step itself.

  /// A submission of `payload` from the shim to the hub under `nonce`.
  pure def submitMail(nonce: Nonce, payload: Payload): Mail =
    { src: ShimAddr, dst: HubAddr, msg: Submit({ nonce: nonce, payload: payload }) }

  /// A lookup of `txid` from the shim to the hub under `nonce`.
  pure def lookupMail(nonce: Nonce, txid: TxId): Mail =
    { src: ShimAddr, dst: HubAddr, msg: Lookup({ nonce: nonce, txid: txid }) }

  /// The same frame, sent by the third party instead.
  pure def fromThirdParty(mail: Mail): Mail =
    { ...mail, src: ThirdPartyAddr }

  /// The hub's ack to the shim under `nonce`.
  pure def ackMail(nonce: Nonce, ack: WireAck): Mail =
    { src: HubAddr, dst: ShimAddr, msg: Ack({ nonce: nonce, ack: ack }) }

  /// The hub's lookup reply to the shim under `nonce`.
  pure def replyMail(nonce: Nonce, reply: WireReply): Mail =
    { src: HubAddr, dst: ShimAddr, msg: LookupReply({ nonce: nonce, reply: reply }) }

  /// The wallet sends, and the shim routes the transaction as it should.
  action sends(input: SendInput, handedOver: bool): bool =
    walletSendWith(input, handedOver)

  /// The wallet sends a transaction and the transport takes the frame.
  action sendToHub(payload: Payload): bool =
    sends(Clean(payload), true)

  /// The wallet asks, and the shim asks the hub.
  action ask(query: TxId): bool =
    walletGetWith(query)

  /// `client`'s submission of `payload` under `nonce` reaches the hub, which
  /// answers `ack`, and queues the payload if it accepts it.
  action answerSubmitFrom(client: Addr, nonce: Nonce, payload: Payload, ack: WireAck): bool =
    hubReceiveWith(
      { src: client, dst: HubAddr, msg: Submit({ nonce: nonce, payload: payload }) },
      INotFound,
      { hub: if (ack == WAccepted) { ...s.hub, queue: s.hub.queue.union(Set(payload)) } else s.hub, reply: AAck(ack) },
    )

  /// The same, accepted.
  action deliverSubmitFrom(client: Addr, nonce: Nonce, payload: Payload): bool =
    answerSubmitFrom(client, nonce, payload, WAccepted)

  /// The shim's submission of `payload` under `nonce` reaches the hub.
  action deliverSubmit(nonce: Nonce, payload: Payload): bool =
    deliverSubmitFrom(ShimAddr, nonce, payload)

  /// `client`'s lookup of `txid` under `nonce` reaches the hub, which acts as
  /// it should on the answer `answer` from its indexer.
  action deliverLookupFrom(client: Addr, nonce: Nonce, txid: TxId, answer: IndexerAnswer): bool =
    hubReceiveWith(
      { src: client, dst: HubAddr, msg: Lookup({ nonce: nonce, txid: txid }) },
      answer,
      { hub: s.hub, reply: AWire(honestReplyOn(txid, answer)) },
    )

  /// The shim's lookup of `txid` under `nonce` reaches the hub, whose indexer
  /// is reachable and truthful.
  action deliverLookup(nonce: Nonce, txid: TxId): bool =
    deliverLookupFrom(ShimAddr, nonce, txid, chainAnswer(s.indexer, txid))

  /// `mail` reaches the shim, which acts as it should.
  action deliverToShim(mail: Mail): bool =
    shimReceiveWith(mail)

  /// The reply an honest hub gives, in the current state, to a lookup of
  /// `txid` on which its indexer would answer `answer`.
  def honestReplyOn(txid: TxId, answer: IndexerAnswer): WireReply =
    render(if (s.hub.queue.exists(payload => payload.txid == Some(txid))) QueueHit else FromIndexer(answer))

  /// The same, with a truthful indexer.
  def honestReply(txid: TxId): WireReply =
    honestReplyOn(txid, chainAnswer(s.indexer, txid))

  /// The shim's wait under `nonce` times out.
  action timeOutLookup(nonce: Nonce): bool =
    shimLookupTimeoutWith(nonce)

  /// An honest indexer gives `verdict` on `payload`, with the effect that
  /// verdict has.
  action judge(payload: Payload, verdict: Verdict): bool =
    indexerVerdictWith(payload, {
      state: indexerApply(s.indexer, BroadcastIInput(payload), VerdictOutput(verdict)),
      out: VerdictOutput(verdict),
    })

  /// The wallet sends `payload`, and the frame under `nonce` reaches the hub.
  run submitTo(nonce: Nonce, payload: Payload): bool =
    sendToHub(payload).then(deliverSubmit(nonce, payload))

  /// The wallet asks for `query`; the lookup under `nonce` reaches the hub;
  /// the reply reaches the shim. The hub's state does not change in between.
  run lookUp(nonce: Nonce, query: TxId): bool =
    ask(query)
      .then(deliverLookup(nonce, query))
      .then(deliverToShim(replyMail(nonce, honestReply(query))))

  /// The hub flushes `batch`, and the indexer gives every entry `verdict`.
  run flush(batch: List[Payload], verdict: Verdict): bool =
    hubTake.then(batch.length().reps(i => judge(batch[i], verdict)))

  /// The wallet's most recent answer.
  def lastEvent: WalletEvent =
    s.wallet.log[s.wallet.log.length() - 1]
}

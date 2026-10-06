// Shim <-> hub divert protocol, over the Nym transport.
//
// Scope: a wallet diverts migrations through the stateless shim to the hub,
// then polls GetTransaction. The hub queues, flushes on epochs, and answers
// lookups from its queue or its indexer. The mixnet may lose, duplicate and
// reorder any message. A transaction's bytes are modelled as its id, and ""
// is an empty body. An unparseable body has no txid: `parseable` is per tx.
//
// Code this mirrors:
//   shim  intercept.rs get_transaction reply arms, nym.rs nonce correlation
//   hub   server.rs lookup, queue.rs admit, batcher.rs flush
//
// Run all of these with spec/check.sh, or one at a time (npm i -g @informalsystems/quint):
//   quint run --main=honest --invariant=safety spec/divert.qnt           holds
//   quint run --main=honest --invariant=pendingVisible ...               holds (honest hub)
//   quint run --main=current --invariant=noQueuedBytes ...               fails: known gap (lying hub)
//   quint run --main=current --invariant=noEarlyBytes ...                fails: known gap (lying hub)
//   quint run --main=before_ecb4641 --invariant=pendingVisible ...       fails: the sentinel bug
//   quint run --main=before_45e408f --invariant=noQueuedBytes ...        fails: the byte leak
//   quint run --main=current --invariant=noSilentRefusal ...             fails: known gap
//   quint run --main=current --invariant=pendingIsTrue ...               fails: known gap (lying hub)
//   quint run --main=current --invariant=queuedNotSuppressed ...         fails: hostile not-found
//   quint run --main=badIndexer --invariant=pendingIsTrue ...            fails: zero RawTransaction
//   quint run --main=honest --invariant=pendingMonotone ...              fails: known gap (flush window)
// Simulation samples random traces; `quint verify` (Apalache) checks every
// trace up to a bound.
module divert {
  // Toggles that reproduce fixed bugs.
  const SENTINEL_ARM: bool     // ecb4641f7e: shim relays found/height 0/empty ahead of the L4 guard
  const HUB_HIDES_QUEUED: bool // 45e408f0ff: hub answers a queue hit with no bytes
  const HONEST_HUB: bool       // false lets the hub answer found, notfound, or error
  const CONFORMING_INDEXER: bool // false lets a missed lookup return a zero RawTransaction
  const TXS: Set[str]
  const MAX_NONCE: int

  pure val EMPTY = ""

  // One record shape for every message; `kind` picks the fields that matter.
  // `hubSaw` and `honest` are ghost fields: the hub's state for the tx when it
  // replied, and whether the reply followed the hub's code.
  type Msg = {
    kind: str, // "submit" | "lookup" | "reply"
    tx: str,
    nonce: int,
    disp: str, // "found" | "notfound" | "error"
    height: int,
    data: str,
    hubSaw: str,
    honest: bool,
  }

  // What the wallet saw for one lookup.
  // `disp` is the reply's disposition, before the shim rewrites a mismatched
  // found into not-found. A timeout has no reply, so its disp is "".
  type Obs = { tx: str, status: str, data: str, hubSaw: str, honest: bool, disp: str }

  // hub[tx]: "absent" | "queued" | "flushing" | "mempool" | "mined" | "dropped"
  // chain[tx]: what a broadcast left on the indexer. "none" until the first
  // acceptance. Admit and a dropped queue copy do not clear it.
  var hub: str -> str
  var chain: str -> str
  var net: Set[Msg]
  var waiters: int -> str // shim: lookup nonce -> queried tx
  var nextNonce: int
  var told: Set[str]      // wallet was told its submit succeeded
  var refused: Set[str]   // hub refused a submit (the ack never reaches the wallet)
  var obs: List[Obs]
  var parseable: str -> bool // false: that body has no txid; find_by_txid skips it

  pure def submitMsg(t: str): Msg =
    { kind: "submit", tx: t, nonce: -1, disp: "", height: 0, data: EMPTY, hubSaw: "", honest: true }

  pure def lookupMsg(t: str, n: int): Msg =
    { kind: "lookup", tx: t, nonce: n, disp: "", height: 0, data: EMPTY, hubSaw: "", honest: true }

  pure def replyMsg(t: str, n: int, disp: str, h: int, d: str, saw: str): Msg =
    { kind: "reply", tx: t, nonce: n, disp: disp, height: h, data: d, hubSaw: saw, honest: true }

  // find_by_txid: the queue hits only when some entry's txid is the query.
  // A parseable body's txid is its id. An unparseable body has none.
  def queueHit(st: str, t: str): bool =
    st == "queued" and TXS.exists(e => parseable.get(e) and e == t)

  // server.rs lookup: queue first, then the indexer. The indexer image survives
  // a later queue copy being admitted or dropped.
  def hubReply(t: str, n: int, st: str, hides: bool): Msg =
    if (queueHit(st, t)) replyMsg(t, n, "found", 0, if (hides) EMPTY else t, st)
    else if (chain.get(t) == "mempool") replyMsg(t, n, "found", 0, t, "mempool")
    else if (chain.get(t) == "mined") replyMsg(t, n, "found", 1, t, "mined")
    else replyMsg(t, n, "notfound", 0, EMPTY, st) // absent, flushing, dropped, or queued with no txid

  // intercept.rs get_transaction: the reply arms, in order.
  pure def shimHandle(q: str, r: Msg, sentinelArm: bool): Obs =
    if (r.disp == "found" and sentinelArm and r.data == EMPTY and r.height == 0)
      { tx: q, status: "pending", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest, disp: r.disp }
    else if (r.disp == "found")
      // L4 guard: the bytes must be the queried transaction. Status becomes
      // not-found; disp stays found, so a mismatch is not a not-found reply.
      if (r.data == q) { tx: q, status: "tx", data: r.data, hubSaw: r.hubSaw, honest: r.honest, disp: r.disp }
      else { tx: q, status: "notfound", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest, disp: r.disp }
    else if (r.disp == "notfound") { tx: q, status: "notfound", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest, disp: r.disp }
    else { tx: q, status: "unavailable", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest, disp: r.disp }

  action init = all {
    hub' = TXS.mapBy(_ => "absent"),
    net' = Set(),
    waiters' = Map(),
    nextNonce' = 0,
    told' = Set(),
    refused' = Set(),
    obs' = List(),
    parseable' = TXS.mapBy(_ => true),
    chain' = TXS.mapBy(_ => "none"),
  }

  action unchangedExcept_net_hub = all {
    waiters' = waiters, nextNonce' = nextNonce, told' = told, refused' = refused, obs' = obs,
    parseable' = parseable, chain' = chain,
  }

  // Nym submit is dispatch-only: the wallet hears success once the frame is
  // handed to the transport (nym.rs send_submit).
  action walletSubmit(t: str): bool = all {
    net' = net.union(Set(submitMsg(t))),
    told' = told.union(Set(t)),
    hub' = hub, waiters' = waiters, nextNonce' = nextNonce, refused' = refused, obs' = obs,
    parseable' = parseable, chain' = chain,
  }

  // The body does not deserialize. It is still submitted; admit holds the bytes
  // with no txid. Kept out of `step`; the run is the witness.
  action submitUnparseable(t: str): bool = all {
    net' = net.union(Set(submitMsg(t))),
    told' = told.union(Set(t)),
    parseable' = parseable.set(t, false),
    hub' = hub, waiters' = waiters, nextNonce' = nextNonce, refused' = refused, obs' = obs,
    chain' = chain,
  }

  // A resend of the same bytes is admitted again while the tx is published,
  // flushing, or returning from a retry. Lookup prefers the queue, so the next
  // answer is the empty sentinel. A resend that arrives while the bytes are
  // already queued stays a no-op (sha256 dedup).
  action hubAdmit(t: str): bool = all {
    net.contains(submitMsg(t)),
    hub.get(t).in(Set("absent", "dropped", "flushing", "mempool", "mined")),
    hub' = hub.set(t, "queued"),
    net' = net,
    unchangedExcept_net_hub,
  }

  // TipStale, Draining, ExpiryTooTight, Full: the ack says refused and the
  // shim has already dropped the waiter.
  action hubRefuse(t: str): bool = all {
    net.contains(submitMsg(t)),
    hub.get(t).in(Set("absent", "dropped")),
    refused' = refused.union(Set(t)),
    hub' = hub, net' = net, parseable' = parseable, chain' = chain,
    waiters' = waiters, nextNonce' = nextNonce, told' = told, obs' = obs,
  }

  // batcher.rs: an epoch boundary drains the whole queue at once.
  action flushStart: bool = all {
    TXS.exists(t => hub.get(t) == "queued"),
    hub' = hub.keys().mapBy(t => if (hub.get(t) == "queued") "flushing" else hub.get(t)),
    net' = net,
    unchangedExcept_net_hub,
  }

  // The run's stand-in for one broadcast_batch verdict: the tx is in the mempool.
  action markMempool(t: str): bool = all {
    hub.get(t) == "flushing",
    hub' = hub.set(t, "mempool"),
    net' = net,
    chain' = chain.set(t, "mempool"),
    waiters' = waiters, nextNonce' = nextNonce, told' = told, refused' = refused, obs' = obs,
    parseable' = parseable,
  }

  // A published tx stays on the indexer. A later mempool acceptance must not
  // downgrade a mined one.
  pure def chainAfter(prev: str, outcome: str): str =
    if (outcome == "mempool" and prev != "mined") "mempool" else prev

  // broadcast_batch verdicts: published, rejected, or retryable (requeued).
  // Only an acceptance updates the indexer. A rejection or a requeue leaves
  // whatever was already broadcast.
  action flushOutcome(t: str): bool = all {
    hub.get(t) == "flushing",
    nondet outcome = Set("mempool", "dropped", "queued").oneOf()
    all {
      hub' = hub.set(t, outcome),
      net' = net,
      chain' = chain.set(t, chainAfter(chain.get(t), outcome)),
      waiters' = waiters, nextNonce' = nextNonce, told' = told, refused' = refused, obs' = obs,
      parseable' = parseable,
    },
  }

  action mine(t: str): bool = all {
    hub.get(t) == "mempool",
    hub' = hub.set(t, "mined"),
    net' = net,
    chain' = chain.set(t, "mined"),
    waiters' = waiters, nextNonce' = nextNonce, told' = told, refused' = refused, obs' = obs,
    parseable' = parseable,
  }

  action walletLookup(t: str): bool = all {
    nextNonce < MAX_NONCE,
    net' = net.union(Set(lookupMsg(t, nextNonce))),
    waiters' = waiters.put(nextNonce, t),
    nextNonce' = nextNonce + 1,
    hub' = hub, told' = told, refused' = refused, obs' = obs, parseable' = parseable, chain' = chain,
  }

  action hubAnswer(n: int, t: str): bool = all {
    net.contains(lookupMsg(t, n)),
    net' = net.union(Set(hubReply(t, n, hub.get(t), HUB_HIDES_QUEUED))),
    hub' = hub,
    unchangedExcept_net_hub,
  }

  // Indexer unreachable (chain.rs: no endpoint answered). A queue hit answers
  // before the indexer is asked.
  action hubError(n: int, t: str): bool = all {
    net.contains(lookupMsg(t, n)),
    hub.get(t) != "queued",
    net' = net.union(Set(replyMsg(t, n, "error", 0, EMPTY, hub.get(t)))),
    hub' = hub,
    unchangedExcept_net_hub,
  }

  // Queue miss forwards the indexer image. A non-conforming indexer answers
  // that path with a zero RawTransaction: found, empty body, height 0. The
  // shim renders pending. A queue hit returns before the indexer is asked.
  action indexerZero(n: int, t: str): bool = all {
    not(CONFORMING_INDEXER),
    net.contains(lookupMsg(t, n)),
    hub.get(t) != "queued",
    net' = net.union(Set(replyMsg(t, n, "found", 0, EMPTY, hub.get(t)))),
    hub' = hub,
    unchangedExcept_net_hub,
  }

  // A hostile hub may answer a lookup with found, notfound, or error.
  // Height is 0 or 1. The body is empty or one of the modelled tx ids.
  action hubLie(n: int, t: str): bool = all {
    not(HONEST_HUB),
    net.contains(lookupMsg(t, n)),
    nondet disp = Set("found", "notfound", "error").oneOf()
    nondet d = TXS.union(Set(EMPTY)).oneOf()
    nondet h = Set(0, 1).oneOf()
    net' = net.union(Set(replyMsg(t, n, disp, h, d, hub.get(t)).with("honest", false))),
    hub' = hub,
    unchangedExcept_net_hub,
  }

  // nym.rs: a reply for an unknown nonce is dropped; a known one resolves the
  // waiter once.
  action shimDeliver(r: Msg): bool = all {
    r.kind == "reply",
    waiters.keys().contains(r.nonce),
    obs' = obs.append(shimHandle(waiters.get(r.nonce), r, SENTINEL_ARM)),
    waiters' = waiters.keys().exclude(Set(r.nonce)).mapBy(k => waiters.get(k)),
    net' = net, hub' = hub, nextNonce' = nextNonce, told' = told, refused' = refused, parseable' = parseable, chain' = chain,
  }

  // 90 s lookup deadline.
  action shimTimeout(n: int): bool = all {
    waiters.keys().contains(n),
    obs' = obs.append({ tx: waiters.get(n), status: "unavailable", data: EMPTY, hubSaw: "timeout", honest: true, disp: "" }),
    waiters' = waiters.keys().exclude(Set(n)).mapBy(k => waiters.get(k)),
    net' = net, hub' = hub, nextNonce' = nextNonce, told' = told, refused' = refused, parseable' = parseable, chain' = chain,
  }

  action lose(m: Msg): bool = all {
    net' = net.exclude(Set(m)),
    hub' = hub,
    unchangedExcept_net_hub,
  }

  action step = {
    nondet t = TXS.oneOf()
    nondet n = 0.to(MAX_NONCE - 1).oneOf()
    any {
      walletSubmit(t),
      hubAdmit(t),
      hubRefuse(t),
      flushStart,
      flushOutcome(t),
      mine(t),
      walletLookup(t),
      hubAnswer(n, t),
      hubError(n, t),
      indexerZero(n, t),
      hubLie(n, t),
      shimTimeout(n),
      all { net.size() > 0, nondet m = net.oneOf() any { shimDeliver(m), lose(m) } },
    }
  }

  // ---- Shim claims: hold for an honest hub's replies ----

  // An honest queue hit on a parseable tx reaches the wallet as pending.
  // `not(o.honest)` drops lying replies. An unparseable body has no txid, so
  // a lookup misses it even while the hub holds the bytes.
  val pendingVisible: bool = obs.foldl(true, (ok, o) =>
    ok and (not(o.honest) or o.hubSaw != "queued" or not(parseable.get(o.tx)) or o.status == "pending"))

  // ---- Hub claims: hold for an honest hub; attestation is what makes it one ----

  // A queued migration's bytes never leave the hub before publication.
  val noQueuedBytes: bool = net.forall(m =>
    not(m.kind == "reply" and m.hubSaw == "queued" and m.data != EMPTY))

  // The wallet sees a transaction's bytes only once it is published.
  val noEarlyBytes: bool = obs.foldl(true, (ok, o) =>
    ok and (o.status != "tx" or o.hubSaw.in(Set("mempool", "mined"))))

  val safety: bool = pendingVisible and noQueuedBytes and noEarlyBytes

  // ---- Known gaps: expected to fail; each counterexample documents one ----

  // Over Nym the wallet is told success even when the hub refuses.
  val noSilentRefusal: bool = told.intersect(refused) == Set()

  // noQueuedBytes and noEarlyBytes also fail under a lying hub: a hub holding
  // a queued migration can release its bytes early.

  // Pending only ever comes from a real queue hit. A lying hub can fake it.
  val pendingIsTrue: bool = obs.foldl(true, (ok, o) =>
    ok and (o.status != "pending" or o.hubSaw == "queued"))

  // A hostile hub can answer not-found for a migration it has queued.
  // This is the reply's disposition. L4 rewrites a mismatched found into
  // status not-found and must not count.
  val queuedNotSuppressed: bool = obs.foldl(true, (ok, o) =>
    ok and not(o.hubSaw == "queued" and o.disp == "notfound" and not(o.honest)))

  // Once pending, a tx reads as not-found only if the hub dropped it. The
  // flush window (server.rs lookup note) breaks this.
  val pendingMonotone: bool =
    0.to(obs.length() - 1).forall(i => 0.to(obs.length() - 1).forall(j =>
      not(i < j and obs[i].tx == obs[j].tx and obs[i].status == "pending"
          and obs[j].status == "notfound" and obs[j].hubSaw != "dropped")))

  // Once the wallet has been shown a transaction's bytes, a later observation
  // of that tx is those bytes again or unavailable. A reordered reply and a
  // resend after publication both break this.
  val statusNeverRegresses: bool =
    0.to(obs.length() - 1).forall(i => 0.to(obs.length() - 1).forall(j =>
      not(i < j and obs[i].tx == obs[j].tx and obs[i].status == "tx"
          and not(obs[j].status.in(Set("tx", "unavailable"))))))
}

module current {
  import divert(
    SENTINEL_ARM = true, HUB_HIDES_QUEUED = true, HONEST_HUB = false,
    CONFORMING_INDEXER = true,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*

  run hostileHubHidesQueueHitTest =
    init
      .then(walletSubmit("a"))
      .then(hubAdmit("a"))
      .then(walletLookup("a"))
      .then(shimDeliver(replyMsg("a", 0, "notfound", 0, EMPTY, "queued").with("honest", false)))
      .expect(obs[0].disp == "notfound" and obs[0].status == "notfound" and not(queuedNotSuppressed))
}

module honest {
  import divert(
    SENTINEL_ARM = true, HUB_HIDES_QUEUED = true, HONEST_HUB = true,
    CONFORMING_INDEXER = true,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*

  // L4: a found body that is not the queried tx is not served.
  run mismatchedLookupRefusedTest =
    init
      .then(walletLookup("a"))
      .then(shimDeliver(replyMsg("a", 0, "found", 1, "b", "mined")))
      .expect(obs[0].status == "notfound")

  // The queue-hit image is pending, ahead of L4.
  run sentinelLookupPendingTest =
    init
      .then(walletLookup("a"))
      .then(shimDeliver(replyMsg("a", 0, "found", 0, EMPTY, "queued")))
      .expect(obs[0].status == "pending")

  // Unparseable bytes stay queued. Lookup misses them: no entry has that txid.
  run unparseableLookupMissesTest =
    init
      .then(submitUnparseable("a"))
      .then(hubAdmit("a"))
      .then(walletLookup("a"))
      .then(hubAnswer(0, "a"))
      .then(shimDeliver(hubReply("a", 0, "queued", true)))
      .expect(not(parseable.get("a")) and hub.get("a") == "queued" and obs[0].status == "notfound" and obs[0].hubSaw == "queued")

  run reorderedReplyRegressesTest =
    init
      .then(walletSubmit("a"))
      .then(hubAdmit("a"))
      .then(walletLookup("a"))
      .then(hubAnswer(0, "a"))
      .then(flushStart)
      .then(markMempool("a"))
      .then(walletLookup("a"))
      .then(hubAnswer(1, "a"))
      .then(shimDeliver(hubReply("a", 1, "mempool", true)))
      .then(shimDeliver(hubReply("a", 0, "queued", true)))
      .expect(not(statusNeverRegresses))

  run resendAfterPublishRegressesTest =
    init
      .then(walletSubmit("a"))
      .then(hubAdmit("a"))
      .then(flushStart)
      .then(markMempool("a"))
      .then(walletLookup("a"))
      .then(hubAnswer(0, "a"))
      .then(shimDeliver(hubReply("a", 0, "mempool", true)))
      .then(hubAdmit("a"))
      .then(walletLookup("a"))
      .then(hubAnswer(1, "a"))
      .then(shimDeliver(hubReply("a", 1, "queued", true)))
      .expect(not(statusNeverRegresses))
}

module before_ecb4641 {
  import divert(
    SENTINEL_ARM = false, HUB_HIDES_QUEUED = true, HONEST_HUB = true,
    CONFORMING_INDEXER = true,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*
}

module before_45e408f {
  import divert(
    SENTINEL_ARM = false, HUB_HIDES_QUEUED = false, HONEST_HUB = true,
    CONFORMING_INDEXER = true,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*
}

// Honest hub, indexer answers the forward path with a zero RawTransaction.
module badIndexer {
  import divert(
    SENTINEL_ARM = true, HUB_HIDES_QUEUED = true, HONEST_HUB = true,
    CONFORMING_INDEXER = false,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*

  run indexerZeroForgesPendingTest =
    init
      .then(walletLookup("a"))
      .then(indexerZero(0, "a"))
      .then(shimDeliver(replyMsg("a", 0, "found", 0, EMPTY, "absent")))
      .expect(obs[0].status == "pending" and obs[0].hubSaw == "absent" and obs[0].honest)
}

// Shim <-> hub divert protocol, over the Nym transport.
//
// Scope: a wallet diverts migrations through the stateless shim to the hub,
// then polls GetTransaction. The hub queues, flushes on epochs, and answers
// lookups from its queue or its indexer. The mixnet may lose, duplicate and
// reorder any message. A transaction's bytes are modelled as its id, and ""
// is an empty body.
//
// Code this mirrors:
//   shim  intercept.rs get_transaction reply arms, nym.rs nonce correlation
//   hub   server.rs lookup, queue.rs admit, batcher.rs flush
//
// Run all of these with spec/check.sh, or one at a time (npm i -g @informalsystems/quint):
//   quint run --main=honest --invariant=safety spec/divert.qnt           holds
//   quint run --main=current --invariant=shimSafety ...                  holds
//   quint run --main=current --invariant=noEarlyBytes ...                fails: known gap (lying hub)
//   quint run --main=before_ecb4641 --invariant=pendingVisible ...       fails: the sentinel bug
//   quint run --main=before_45e408f --invariant=noQueuedBytes ...        fails: the byte leak
//   quint run --main=current --invariant=noSilentRefusal ...             fails: known gap
//   quint run --main=current --invariant=pendingIsTrue ...               fails: known gap (lying hub)
//   quint run --main=honest --invariant=pendingMonotone ...              fails: known gap (flush window)
// Simulation samples random traces; `quint verify` (Apalache) checks every
// trace up to a bound.
module divert {
  // Toggles that reproduce fixed bugs.
  const SENTINEL_ARM: bool     // ecb4641f7e: shim relays found/height 0/empty ahead of the L4 guard
  const HUB_HIDES_QUEUED: bool // 45e408f0ff: hub answers a queue hit with no bytes
  const HONEST_HUB: bool       // false lets the hub answer lookups with anything
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
  type Obs = { tx: str, status: str, data: str, hubSaw: str, honest: bool }

  // hub[tx]: "absent" | "queued" | "flushing" | "mempool" | "mined" | "dropped"
  var hub: str -> str
  var net: Set[Msg]
  var waiters: int -> str // shim: lookup nonce -> queried tx
  var nextNonce: int
  var told: Set[str]      // wallet was told its submit succeeded
  var refused: Set[str]   // hub refused a submit (the ack never reaches the wallet)
  var obs: List[Obs]

  pure def submitMsg(t: str): Msg =
    { kind: "submit", tx: t, nonce: -1, disp: "", height: 0, data: EMPTY, hubSaw: "", honest: true }

  pure def lookupMsg(t: str, n: int): Msg =
    { kind: "lookup", tx: t, nonce: n, disp: "", height: 0, data: EMPTY, hubSaw: "", honest: true }

  pure def replyMsg(t: str, n: int, disp: str, h: int, d: str, saw: str): Msg =
    { kind: "reply", tx: t, nonce: n, disp: disp, height: h, data: d, hubSaw: saw, honest: true }

  // server.rs lookup: queue first, then the indexer.
  pure def hubReply(t: str, n: int, st: str, hides: bool): Msg =
    if (st == "queued") replyMsg(t, n, "found", 0, if (hides) EMPTY else t, st)
    else if (st == "mempool") replyMsg(t, n, "found", 0, t, st)
    else if (st == "mined") replyMsg(t, n, "found", 1, t, st)
    else replyMsg(t, n, "notfound", 0, EMPTY, st) // absent, flushing, dropped

  // intercept.rs get_transaction: the reply arms, in order.
  pure def shimHandle(q: str, r: Msg, sentinelArm: bool): Obs =
    if (r.disp == "found" and sentinelArm and r.data == EMPTY and r.height == 0)
      { tx: q, status: "pending", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest }
    else if (r.disp == "found")
      // L4 guard: the bytes must be the queried transaction.
      if (r.data == q) { tx: q, status: "tx", data: r.data, hubSaw: r.hubSaw, honest: r.honest }
      else { tx: q, status: "notfound", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest }
    else if (r.disp == "notfound") { tx: q, status: "notfound", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest }
    else { tx: q, status: "unavailable", data: EMPTY, hubSaw: r.hubSaw, honest: r.honest }

  action init = all {
    hub' = TXS.mapBy(_ => "absent"),
    net' = Set(),
    waiters' = Map(),
    nextNonce' = 0,
    told' = Set(),
    refused' = Set(),
    obs' = List(),
  }

  action unchangedExcept_net_hub = all {
    waiters' = waiters, nextNonce' = nextNonce, told' = told, refused' = refused, obs' = obs,
  }

  // Nym submit is dispatch-only: the wallet hears success once the frame is
  // handed to the transport (nym.rs send_submit).
  action walletSubmit(t: str): bool = all {
    net' = net.union(Set(submitMsg(t))),
    told' = told.union(Set(t)),
    hub' = hub, waiters' = waiters, nextNonce' = nextNonce, refused' = refused, obs' = obs,
  }

  // queue.rs admit. Delivery leaves the message in `net`, so duplicates are
  // free; a duplicate of a queued tx is a no-op (sha256 dedup).
  action hubAdmit(t: str): bool = all {
    net.contains(submitMsg(t)),
    hub.get(t).in(Set("absent", "dropped")),
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
    hub' = hub, net' = net,
    waiters' = waiters, nextNonce' = nextNonce, told' = told, obs' = obs,
  }

  // batcher.rs: an epoch boundary drains the whole queue at once.
  action flushStart: bool = all {
    TXS.exists(t => hub.get(t) == "queued"),
    hub' = hub.keys().mapBy(t => if (hub.get(t) == "queued") "flushing" else hub.get(t)),
    net' = net,
    unchangedExcept_net_hub,
  }

  // broadcast_batch verdicts: published, rejected, or retryable (requeued).
  action flushOutcome(t: str): bool = all {
    hub.get(t) == "flushing",
    nondet outcome = Set("mempool", "dropped", "queued").oneOf()
    hub' = hub.set(t, outcome),
    net' = net,
    unchangedExcept_net_hub,
  }

  action mine(t: str): bool = all {
    hub.get(t) == "mempool",
    hub' = hub.set(t, "mined"),
    net' = net,
    unchangedExcept_net_hub,
  }

  action walletLookup(t: str): bool = all {
    nextNonce < MAX_NONCE,
    net' = net.union(Set(lookupMsg(t, nextNonce))),
    waiters' = waiters.put(nextNonce, t),
    nextNonce' = nextNonce + 1,
    hub' = hub, told' = told, refused' = refused, obs' = obs,
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

  // A buggy or hostile hub may answer any lookup with anything.
  action hubLie(n: int, t: str): bool = all {
    not(HONEST_HUB),
    net.contains(lookupMsg(t, n)),
    nondet d = TXS.union(Set(EMPTY)).oneOf()
    nondet h = Set(0, 1).oneOf()
    net' = net.union(Set(replyMsg(t, n, "found", h, d, hub.get(t)).with("honest", false))),
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
    net' = net, hub' = hub, nextNonce' = nextNonce, told' = told, refused' = refused,
  }

  // 90 s lookup deadline.
  action shimTimeout(n: int): bool = all {
    waiters.keys().contains(n),
    obs' = obs.append({ tx: waiters.get(n), status: "unavailable", data: EMPTY, hubSaw: "timeout", honest: true }),
    waiters' = waiters.keys().exclude(Set(n)).mapBy(k => waiters.get(k)),
    net' = net, hub' = hub, nextNonce' = nextNonce, told' = told, refused' = refused,
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
      hubLie(n, t),
      shimTimeout(n),
      all { net.size() > 0, nondet m = net.oneOf() any { shimDeliver(m), lose(m) } },
    }
  }

  // ---- Shim claims: hold against any hub ----

  // L4: bytes served to the wallet are always the queried transaction.
  val guardHolds: bool = obs.foldl(true, (ok, o) => ok and (o.status != "tx" or o.data == o.tx))

  // An honest queue hit reaches the wallet as pending (the ecb4641f7e bug).
  val pendingVisible: bool = obs.foldl(true, (ok, o) =>
    ok and (not(o.honest) or o.hubSaw != "queued" or o.status == "pending"))

  val shimSafety: bool = guardHolds and pendingVisible

  // ---- Hub claims: hold for an honest hub; attestation is what makes it one ----

  // A queued migration's bytes never leave the hub before publication.
  val noQueuedBytes: bool = net.forall(m =>
    not(m.kind == "reply" and m.hubSaw == "queued" and m.data != EMPTY))

  // The wallet sees a transaction's bytes only once it is published.
  val noEarlyBytes: bool = obs.foldl(true, (ok, o) =>
    ok and (o.status != "tx" or o.hubSaw.in(Set("mempool", "mined"))))

  val safety: bool = shimSafety and noQueuedBytes and noEarlyBytes

  // ---- Known gaps: expected to fail; each counterexample documents one ----

  // Over Nym the wallet is told success even when the hub refuses.
  val noSilentRefusal: bool = told.intersect(refused) == Set()

  // noQueuedBytes and noEarlyBytes also fail under a lying hub: a hub holding
  // a queued migration can release its bytes early.

  // Pending only ever comes from a real queue hit. A lying hub can fake it.
  val pendingIsTrue: bool = obs.foldl(true, (ok, o) =>
    ok and (o.status != "pending" or o.hubSaw == "queued"))

  // Once pending, a tx reads as not-found only if the hub dropped it. The
  // flush window (server.rs lookup note) breaks this.
  val pendingMonotone: bool =
    0.to(obs.length() - 1).forall(i => 0.to(obs.length() - 1).forall(j =>
      not(i < j and obs[i].tx == obs[j].tx and obs[i].status == "pending"
          and obs[j].status == "notfound" and obs[j].hubSaw != "dropped")))
}

module current {
  import divert(
    SENTINEL_ARM = true, HUB_HIDES_QUEUED = true, HONEST_HUB = false,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*
}

module honest {
  import divert(
    SENTINEL_ARM = true, HUB_HIDES_QUEUED = true, HONEST_HUB = true,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*
}

module before_ecb4641 {
  import divert(
    SENTINEL_ARM = false, HUB_HIDES_QUEUED = true, HONEST_HUB = true,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*
}

module before_45e408f {
  import divert(
    SENTINEL_ARM = false, HUB_HIDES_QUEUED = false, HONEST_HUB = true,
    TXS = Set("a", "b"), MAX_NONCE = 4,
  ).*
}

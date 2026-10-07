// -*- mode: Bluespec; -*-

/// The configurations the protocol is checked in.
///
/// `configs` names each configuration as a value. Below it there is one module
/// per configuration, which is what `quint run --main=<name>` simulates. A
/// module that scripts runs against a configuration instantiates `protocol`
/// with the same value.

module configs {
  import basicSpells.* from "./spells/basicSpells"
  import types.* from "./types"

  // ------------------------------------------------------------------------
  // Transactions
  // ------------------------------------------------------------------------
  //
  // The schedule below flushes every 3 blocks with a mining margin of 2 and a
  // delivery lag of 1, and supports wallets that set an expiry 7 blocks out.

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
  /// The third party's own: one payload a hub will queue, one too large to.
  pure val garbage: Payload =
    { id: "garbage", txid: Some("garbage"), created: 1, expiry: None, class: OrchardTouching, oversize: false }
  pure val bloat = { ...garbage, id: "bloat", txid: Some("bloat"), oversize: true }

  // ------------------------------------------------------------------------
  // Configurations
  // ------------------------------------------------------------------------

  pure val allHonest: Roles = { shim: Honest, hubs: Map("h1" -> Honest), indexer: Honest }

  /// One hub, the mixnet transport, every component honest, a timely tip.
  ///
  /// The schedule is the shipped one scaled down (shipped: interval 20,
  /// margin 4, lag 6, reorg allowance 10, staleness window 12 blocks, expiry
  /// floor 40), keeping the relations between the numbers that matter
  /// (`shippedRelationsKept` in `protocol.qnt`):
  ///
  /// - the slack `floor - (interval + margin + lag)` equals the reorg
  ///   allowance (10 there, 1 here);
  /// - the staleness window is larger than the margin and than the slack;
  /// - `interval + margin + lag + (window - 1)` exceeds the floor by one
  ///   (41 > 40 there, 8 > 7 here), while `interval + lag + (window - 1)` does
  ///   not (37 <= 40, 6 <= 7).
  ///
  /// The margin is 2, the smallest that leaves room for a block to arrive
  /// while a flush is in flight (`maxFlightBlocks`).
  pure val baseline: Config = {
    payloads: Set(early, late, tight, junk, plain),
    twins: Set(earlyTwin),
    tpPayloads: Set(garbage, bloat),
    hubs: ["h1"],
    flushInterval: 3,
    miningMargin: 2,
    deliveryLag: 1,
    reorgAllowance: 1,
    staleWindow: 3,
    freeRun: NotSlower,
    minWalletExpiry: 7,
    maxAttempts: 2,
    queueCap: 2,
    maxFlightBlocks: 1,
    maxHeight: 12,
    maxRequests: 3,
    submitMode: DispatchOnly,
    roles: allHonest,
    tip: TipTimely,
    reliesOnReorgSlack: true,
    reliesOnFlightWithinMargin: true,
  }

  // One Byzantine component at a time.
  pure val byzShim = { ...baseline, roles: { ...allHonest, shim: Byzantine } }
  pure val byzHub = { ...baseline, roles: { ...allHonest, hubs: Map("h1" -> Byzantine) } }
  pure val byzIndexer = { ...baseline, roles: { ...allHonest, indexer: Byzantine } }

  // The HTTP transport: the wallet's answer is the hub's decision.
  pure val awaitAck = { ...baseline, submitMode: AwaitVerdict }
  pure val awaitAckByzShim = { ...byzShim, submitMode: AwaitVerdict }
  pure val awaitAckByzHub = { ...byzHub, submitMode: AwaitVerdict }
  pure val awaitAckByzIndexer = { ...byzIndexer, submitMode: AwaitVerdict }

  // Two hubs sharing one chain.
  pure val replicated = {
    ...baseline,
    hubs: ["h1", "h2"],
    roles: { ...allHonest, hubs: Map("h1" -> Honest, "h2" -> Honest) },
  }
  pure val replicatedOneByz = {
    ...replicated,
    roles: { ...allHonest, hubs: Map("h1" -> Honest, "h2" -> Byzantine) },
  }

  // A tip that may be reported up to the reorg allowance behind the chain:
  // with the slack that covers it, and without. In the second the expiry
  // floor is the three-term budget exactly.
  pure val flakyTip = { ...baseline, tip: TipMayRegress }
  pure val flakyTipNoSlack = {
    ...flakyTip,
    minWalletExpiry: 6,
    reliesOnReorgSlack: false,
    payloads: Set(orchard("early", 2, 8), late, tight, junk, plain),
    twins: Set({ ...orchard("early", 2, 8), id: "early-twin" }),
  }

  // The same tip, and a flush that may stay in flight for as many blocks as
  // the mining margin reserves.
  pure val flakyTipSlowFlight = { ...flakyTip, maxFlightBlocks: 2, reliesOnFlightWithinMargin: false }

  // A hub that may go without a tip for a while: on the shipped relation
  // between the staleness window and the expiry floor, and on the relation
  // that would cover the silence.
  pure val staleLag = { ...baseline, tip: TipMayLag }
  pure val staleLagWithSlack = {
    ...staleLag,
    minWalletExpiry: 8,
    payloads: Set(orchard("early", 2, 10), orchard("late", 4, 12), tight, junk, plain),
    twins: Set({ ...orchard("early", 2, 10), id: "early-twin" }),
  }
}

module baseline {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = baseline).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module byzShim {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzShim).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module byzHub {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzHub).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module byzIndexer {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzIndexer).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module awaitAck {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAck).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module awaitAckByzShim {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAckByzShim).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module awaitAckByzHub {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAckByzHub).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module awaitAckByzIndexer {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAckByzIndexer).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module replicated {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = replicated).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module replicatedOneByz {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = replicatedOneByz).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module flakyTip {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = flakyTip).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

// The reorg slack is dropped on purpose: this configuration shows what it buys.
module flakyTipNoSlack {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = flakyTipNoSlack).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(flightWithinMargin),
    assert(not(reorgSlackFits)),
    assert(not(shippedRelationsKept)),
  }
}

// A flush may outlast the mining margin: this configuration shows what the
// bound on flight time buys.
module flakyTipSlowFlight {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = flakyTipSlowFlight).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(shippedRelationsKept),
    assert(not(flightWithinMargin)),
  }
}

// The stale slack does not hold here, as it does not for the shipped constants.
module staleLag {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = staleLag).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(shippedRelationsKept),
    assert(not(staleSlackFits)),
  }
}

module staleLagWithSlack {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = staleLagWithSlack).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
    assert(flightWithinMargin),
    assert(staleSlackFits),
    assert(not(shippedRelationsKept)),
  }
}

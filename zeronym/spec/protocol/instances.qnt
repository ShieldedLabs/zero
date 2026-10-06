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
  // The schedule below flushes every 3 blocks with a mining margin of 1 and a
  // delivery lag of 1, and supports wallets that set an expiry 6 blocks out.

  pure def orchard(id: str, created: Height, expiry: Height): Payload =
    { id: id, txid: Some(id), created: created, expiry: Some(expiry), class: OrchardTouching, oversize: false }

  /// Two migrations from supported wallets, built at heights 2 and 4.
  pure val early = orchard("early", 2, 8)
  pure val late = orchard("late", 4, 10)
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
  /// floor 40), keeping the relations between the numbers that matter:
  ///
  /// - the slack `floor - (interval + margin + lag)` equals the reorg
  ///   allowance (10 there, 1 here);
  /// - the staleness window is larger than the margin and than the slack;
  /// - `interval + margin + lag + (window - 1)` exceeds the floor by one
  ///   (41 > 40 there, 7 > 6 here), while `interval + lag + (window - 1)` does
  ///   not (37 <= 40, 6 <= 6).
  pure val baseline: Config = {
    payloads: Set(early, late, tight, junk, plain),
    twins: Set(earlyTwin),
    tpPayloads: Set(garbage, bloat),
    hubs: ["h1"],
    flushInterval: 3,
    miningMargin: 1,
    deliveryLag: 1,
    reorgAllowance: 1,
    staleWindow: 3,
    freeRun: NotSlower,
    minWalletExpiry: 6,
    maxAttempts: 2,
    queueCap: 2,
    maxHeight: 12,
    maxRequests: 3,
    submitMode: DispatchOnly,
    roles: allHonest,
    tip: TipTimely,
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
  // with the slack that covers it, and without.
  pure val flakyTip = { ...baseline, tip: TipMayRegress }
  pure val flakyTipNoSlack = {
    ...flakyTip,
    minWalletExpiry: 5,
    payloads: Set(orchard("early", 2, 7), late, tight, junk, plain),
    twins: Set({ ...orchard("early", 2, 7), id: "early-twin" }),
  }

  // A hub that may go without a tip for a while: on the shipped relation
  // between the staleness window and the expiry floor, and on the relation
  // that would cover the silence.
  pure val staleLag = { ...baseline, tip: TipMayLag }
  pure val staleLagWithSlack = {
    ...staleLag,
    minWalletExpiry: 7,
    payloads: Set(orchard("early", 2, 9), orchard("late", 4, 11), tight, junk, plain),
    twins: Set({ ...orchard("early", 2, 9), id: "early-twin" }),
  }
}

module baseline {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = baseline).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module byzShim {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzShim).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module byzHub {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzHub).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module byzIndexer {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzIndexer).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module awaitAck {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAck).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module awaitAckByzShim {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAckByzShim).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module awaitAckByzHub {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAckByzHub).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module awaitAckByzIndexer {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = awaitAckByzIndexer).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module replicated {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = replicated).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module replicatedOneByz {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = replicatedOneByz).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

module flakyTip {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = flakyTip).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(reorgSlackFits),
  }
}

// The reorg slack is dropped on purpose: this configuration shows what it buys.
module flakyTipNoSlack {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = flakyTipNoSlack).* from "./protocol"

  run assumptionsTest = all {
    assert(standingAssumptions),
    assert(not(reorgSlackFits)),
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
    assert(staleSlackFits),
  }
}

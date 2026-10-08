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
  /// The third party's own payload.
  pure val garbage: Payload =
    { id: "garbage", txid: Some("garbage"), created: 1, expiry: None, class: OrchardTouching, oversize: false }

  // ------------------------------------------------------------------------
  // Configurations
  // ------------------------------------------------------------------------

  pure val allHonest: Roles = { hub: Honest, indexer: Honest }

  /// One hub, the mixnet transport, every component honest.
  pure val baseline: Config = {
    payloads: Set(early, late, tight, junk, plain),
    twins: Set(earlyTwin),
    tpPayloads: Set(garbage),
    maxRequests: 3,
    roles: allHonest,
  }

  // One Byzantine component at a time.
  pure val byzHub = { ...baseline, roles: { ...allHonest, hub: Byzantine } }
  pure val byzIndexer = { ...baseline, roles: { ...allHonest, indexer: Byzantine } }

}

module baseline {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = baseline).* from "./protocol"

  run assumptionsTest = assert(payloadsWellFormed)
}

module byzHub {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzHub).* from "./protocol"

  run assumptionsTest = assert(payloadsWellFormed)
}

module byzIndexer {
  import types.* from "./types"
  import configs.*
  import protocol(CONFIG = byzIndexer).* from "./protocol"

  run assumptionsTest = assert(payloadsWellFormed)
}

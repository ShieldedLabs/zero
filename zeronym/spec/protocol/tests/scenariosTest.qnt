// -*- mode: Bluespec; -*-

/// Scripted runs against the honest configurations: one for each behaviour
/// that must be reachable, and one for each cause of each known gap.
///
/// A run is a sequence of the machine's own steps. Where it says a property
/// fails, it also says what is in the wallet's log, the soup or the audit
/// record that makes it fail.

module baselineScenarios {
  import basicSpells.* from "../spells/basicSpells"
  import types.* from "../types"
  import wire.* from "../wire"
  import indexer.* from "../indexer"
  import hub.* from "../hub"
  import shim.* from "../shim"
  import state.* from "../state"
  import configs.* from "../instances"
  import protocol(CONFIG = baseline).* from "../protocol"

  /// A migration from send to mined, with the wallet polling along the way.
  run happyPathTest =
    started
      .then(block)
      // Height 2. The wallet sends; the shim diverts and answers at once.
      .then(submitTo("h1", 0, early))
      .expect(lastEvent == Sent({ input: Clean(early), obs: SentOk }))
      .expect(s.queuedAt("h1") == Set(early) and s.operator == Set())
      .then(lookUp("h1", 1, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Pending, via: Some(1) }))
      // Height 3 is a flush boundary.
      .then(block)
      .then(flush("h1", [early], Accepted))
      .expect(s.queuedAt("h1") == Set() and s.onChain("early") == InMempool)
      .then(lookUp("h1", 2, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: MEMPOOL_HEIGHT }), via: Some(2) }))
      .then(block)
      .then(chainMineWith("early"))
      .then(lookUp("h1", 3, "early"))
      .expect(lastEvent == Got({ query: "early", obs: Tx({ payload: early, height: 4 }), via: Some(3) }))
      .expect(wPending and wTxInMempool and wTxMined)
      .expect(operatorBlind and queuedBytesConfidential and txidAuthenticity and lookupValidityPerHub)
      .expect(offeredBeforeExpiry and conformingFirstOfferBeforeExpiry and ackImpliesQueued and wellFormed)
      .expect(statusNeverRegresses and vLookupValidityPerHub == false)
}

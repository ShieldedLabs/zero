// -*- mode: Bluespec; -*-

/// What a wallet can rely on, what it cannot, and what must be reachable.
///
/// Every definition is a pure predicate over the system and the audit record.
/// There are three classes, kept apart:
///
/// - guarantees: invariants claimed under stated trust and timing assumptions;
/// - known gaps: invariants a wallet might hope for that do not hold even when
///   every component is honest, each with the reason;
/// - witnesses: states that must be reachable, so that the guarantees are not
///   vacuous and the behaviours the gaps describe are shown to happen.
///
/// Two rules keep the guarantees honest. A predicate reads the system and the
/// audit record, and the audit record is derived from consecutive states by
/// `advance` below, never from what a component says about itself. And a
/// guarantee is something a change to an honest component can break.
module properties {
  import basicSpells.* from "./spells/basicSpells"
  import soup.* from "./spells/soup"
  import types.* from "./types"
  import wire.* from "./wire"
  import indexer.* from "./indexer"
  import abstractHub.* from "./abstractHub"
  import shim.* from "./shim"
  import state.* from "./state"

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
      .filter(lookup => waiting.contains(lookup._1))
      .fold(audit.windows, (acc, lookup) =>
        val seen = if (acc.keys().contains(lookup._1)) acc.get(lookup._1) else Set()
        acc.put(lookup._1, seen.union(Set(truth(post, lookup._2)))))
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

  /// G7. Structural sanity: every nonce in use was minted.
  pure def wellFormedIn(s: System): bool =
    and {
      s.shim.waiters.keys().forall(nonce => nonce < s.shim.nextNonce),
      s.net.forall(mail =>
        match mail.msg {
          | Submit(submit) =>
            submit.nonce < (if (mail.src == ShimAddr) s.shim.nextNonce else s.thirdParty.nextNonce)
          | Lookup(lookup) =>
            lookup.nonce < (if (mail.src == ShimAddr) s.shim.nextNonce else s.thirdParty.nextNonce)
          | _ => true
        }),
    }

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
    s.submissions(ShimAddr).filter(submit => submit._2 == payload).map(submit => submit._1)

  /// K1a. The wallet was told ok; every submission that reached the hub was
  /// refused; the hub never queued the payload.
  pure def wToldRefusedEverywhereIn(s: System, audit: Audit): bool =
    s.toldOk().exists(payload =>
      val submitted = s.shimSubmissionsOf(payload)
      and {
        not(audit.everQueued.contains(payload)),
        submitted != Set(),
        submitted.forall(nonce => s.acks(ShimAddr).exists(ack => ack._1 == nonce and ack._2 != WAccepted)),
      })

  /// K1b. The wallet was told ok; the hub has answered none of the frames and
  /// never queued the payload. The network may leave it so forever.
  pure def wToldNeverDeliveredIn(s: System, audit: Audit): bool =
    s.toldOk().exists(payload =>
      and {
        not(audit.everQueued.contains(payload)),
        s.shimSubmissionsOf(payload).forall(nonce => not(s.acks(ShimAddr).exists(ack => ack._1 == nonce))),
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
        | Tx(tx) => tx.height == MEMPOOL_HEIGHT
        | _ => false
      })

  /// W3. The wallet is served its transaction from a block.
  pure def wTxMinedIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(tx) => tx.height != MEMPOOL_HEIGHT
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
      lookup._1 == reply._1 and reply._2 == WFound({ body: None, height: MEMPOOL_HEIGHT }))

  /// W9. The hub holds a payload it cannot parse; the wallet that sent it asks
  /// for it and is told not found. An entry without a txid can never
  /// be hit.
  pure def wUnparseableMissedIn(s: System): bool =
    tuples(s.events(), s.lookups(ShimAddr)).exists(((event, lookup)) =>
      match event {
        | Got(got) =>
          and {
            got.obs == NotFound,
            got.via == Some(lookup._1),
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
  /// true: the chain does not have it, or has not got that far.
  pure def wFalseHeightServedIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(tx) =>
          tx.height > s.height() or not(s.indexer.published().exists(payload => payload.txid == tx.payload.txid))
        | _ => false
      })

  /// W17. The hub has queued a payload of the third party's own making.
  pure def wThirdPartyPayloadQueuedIn(s: System): bool =
    s.hub.queue.intersect(s.thirdParty.own) != Set()

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
}

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
  import hub.* from "./hub"
  import shim.* from "./shim"
  import state.* from "./state"

  // ------------------------------------------------------------------------
  // The audit record
  // ------------------------------------------------------------------------

  /// The true answer to a lookup for `query` at `hub`, read off the hub's
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
  pure def truth(s: System, hub: HubId, query: TxId): LookupObs =
    if (s.queuedAt(hub).exists(payload => payload.txid == Some(query)))
      Pending
    else if (s.indexer.txs.keys().contains(query))
      val tx = s.indexer.txs.get(query)
      Tx({ payload: tx.payload, height: answerHeight(tx.at) })
    else
      NotFound

  pure def initialAudit(s: System): Audit = {
    everQueued: s.hubIds().mapBy(_ => Set()),
    admitted: Map(),
    offers: Set(),
    verdicts: Set(),
    windows: Map(),
    refusals: Set(),
    dropped: Set(),
  }

  /// The refusal a refused ack stands for. The wire has one code for a full
  /// hub and a draining one; the hub's phase tells them apart.
  pure def refusalBehind(code: WireRefusal, phase: Phase): Refusal =
    match code {
      | WTipStale => TipStale
      | WTooLarge => TooLarge
      | WExpiryTooTight => ExpiryTooTight
      | WQueueFull => if (phase == Draining) HubDraining else Full
    }

  /// The audit record after one step, from the states before and after it.
  pure def advance(audit: Audit, pre: System, post: System): Audit =
    val hubs = post.hubIds()
    val entered = hubs.map(hub =>
      post.queuedAt(hub).map(payload => (hub, payload))).flatten()
    // A flush that was idle and is now broadcasting has just put its batch in
    // flight.
    val offered = hubs.map(hub =>
      match post.hubs.get(hub).flush {
        | Broadcasting(flush) =>
          if (pre.hubs.get(hub).flush == Idle)
            flush.batch.keys().map(payload => {
              hub: hub,
              payload: payload,
              height: post.height(),
              attempt: flush.batch.get(payload),
              nth: audit.offers.filter(offer => offer.hub == hub and offer.payload == payload).size(),
            })
          else Set()
        | Idle => Set()
      }).flatten()
    // An entry that was awaiting a verdict and no longer is, in a flush still
    // in flight, has just had its answer. A node judged it unless it was set
    // aside for requeue.
    val judged = hubs.map(hub =>
      match pre.hubs.get(hub).flush {
        | Broadcasting(before) =>
          match post.hubs.get(hub).flush {
            | Broadcasting(after) =>
              before.batch.keys().exclude(after.batch.keys()).map(payload => {
                hub: hub,
                payload: payload,
                height: post.height(),
                nth: audit.offers.filter(offer => offer.hub == hub and offer.payload == payload).size() - 1,
                final: not(after.unplaced.keys().contains(payload)),
              })
            | Idle => Set()
          }
        | Idle => Set()
      }).flatten()
    // A flush that has ended, other than a final one, gave up on every entry
    // nothing judged that is not back in the queue.
    val dropped = hubs.map(hub =>
      match pre.hubs.get(hub).flush {
        | Broadcasting(flush) =>
          if (post.hubs.get(hub).flush == Idle and not(flush.final) and post.hubs.get(hub).phase != Down)
            flush.unplaced.keys().exclude(post.queuedAt(hub)).map(payload =>
              { hub: hub, payload: payload, attempts: flush.unplaced.get(payload) })
          else Set()
        | Idle => Set()
      }).flatten()
    // The lookups the shim is waiting on, or was until this step, each widen
    // their window by what is true at the hub they went to.
    val waiting = pre.shim.waiters.keys().union(post.shim.waiters.keys())
    val windows = post.lookups(ShimAddr)
      .filter(lookup => waiting.contains(lookup._1))
      .fold(audit.windows, (acc, lookup) =>
        val seen = if (acc.keys().contains(lookup._1)) acc.get(lookup._1) else Set()
        acc.put(lookup._1, seen.union(Set(truth(post, lookup._2, lookup._3)))))
    val refused = post.net.exclude(pre.net).fold(Set(), (acc, mail) =>
      match mail.msg {
        | Ack(ack) =>
          match ack.ack {
            | WRefused(code) =>
              match mail.src {
                | HubAddr(hub) => acc.union(Set(refusalBehind(code, pre.hubs.get(hub).phase)))
                | _ => acc
              }
            | WAccepted => acc
          }
        | _ => acc
      })
    {
      everQueued: hubs.mapBy(hub => audit.everQueued.get(hub).union(post.queuedAt(hub))),
      admitted: entered.fold(audit.admitted, (acc, entry) =>
        if (acc.keys().contains(entry)) acc
        else acc.put(entry, { height: post.height(), tip: post.hubs.get(entry._1).observedTip() })),
      offers: audit.offers.union(offered),
      verdicts: audit.verdicts.union(judged),
      windows: windows,
      refusals: audit.refusals.union(refused),
      dropped: audit.dropped.union(dropped),
    }

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
  /// It is a statement about one request and one hub. It does not say that
  /// successive answers agree, nor that hubs agree with each other; see
  /// `statusNeverRegresses`.
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

  /// Whether an offer left the mining margin: the transaction can still be
  /// mined `miningMargin` blocks after the true height it was published at.
  pure def offeredInTime(s: System, offer: Offer): bool =
    match offer.payload.expiry {
      | Some(expiry) => expiry >= offer.height + s.hubs.get(offer.hub).params.miningMargin
      | None => true
    }

  /// Whether `payload` reached `hub` as a supported wallet's would: it honours
  /// the expiry floor, and it first entered the queue within the delivery lag
  /// of the height it was built at.
  pure def isConformingAndTimely(s: System, audit: Audit, hub: HubId, payload: Payload): bool =
    val params = s.hubs.get(hub).params
    and {
      conforming(payload, params.minWalletExpiry),
      audit.admitted.keys().contains((hub, payload)),
      audit.admitted.get((hub, payload)).height <= payload.created + params.deliveryLag,
    }

  // G6 comes in three parts. G6a and G6b are about the moment a flush begins:
  // how much of the mining margin is left when the hub hands the batch over.
  // They do not say a node accepts the transaction, because the chain may move
  // while the batch is in flight. G6c is about the moment a node judges it.

  /// G6a. Every transaction one of `hubs` offers is offered with the mining
  /// margin to spare: whatever was admitted, on every attempt. A claim about
  /// the margin left at the offer, not about acceptance.
  pure def offeredBeforeExpiryIn(s: System, audit: Audit, hubs: Set[HubId]): bool =
    audit.offers.forall(offer => hubs.contains(offer.hub) implies offeredInTime(s, offer))

  /// G6b. The same, for supported wallets only and for the first time a hub
  /// offers the transaction. It says nothing about a later offer of an entry
  /// that was requeued; see `conformingEveryOfferBeforeExpiry`. Like G6a it is
  /// a claim about the margin left at the offer.
  pure def conformingFirstOfferBeforeExpiryIn(s: System, audit: Audit, hubs: Set[HubId]): bool =
    audit.offers.forall(offer =>
      and {
        hubs.contains(offer.hub),
        offer.nth == 0,
        isConformingAndTimely(s, audit, offer.hub, offer.payload),
      } implies offeredInTime(s, offer))

  /// G6c. The end-to-end claim: when a node judges the first offer of a
  /// supported wallet's transaction, the transaction has not expired. It can
  /// still be mined in the next block, so the node does not turn it away for
  /// its expiry.
  ///
  /// The margin is what pays for the blocks that arrive while the batch is in
  /// flight. G6c therefore needs G6b and one thing more: that fewer blocks
  /// than the mining margin arrive during a flush (`flightWithinMargin`).
  pure def conformingFirstOfferJudgedBeforeExpiryIn(s: System, audit: Audit, hubs: Set[HubId]): bool =
    audit.verdicts.forall(verdict =>
      and {
        hubs.contains(verdict.hub),
        verdict.nth == 0,
        verdict.final,
        isConformingAndTimely(s, audit, verdict.hub, verdict.payload),
      } implies
        match verdict.payload.expiry {
          | Some(expiry) => expiry > verdict.height
          | None => true
        })

  /// G7. Structural sanity: a queued entry is within its attempts; a hub that
  /// is down holds nothing and knows nothing; every nonce in use was minted.
  pure def wellFormedIn(s: System): bool =
    and {
      s.hubIds().forall(hub =>
        val state = s.hubs.get(hub)
        and {
          state.queued().forall(payload =>
            state.queue.get(payload) >= 0 and state.queue.get(payload) <= state.params.maxAttempts),
          state.phase == Down implies state == downHub(state.params),
        }),
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

  /// G8. An accepted ack from one of `hubs` is for a payload that hub had
  /// queued by the time it acked. It holds whether or not anyone waits for
  /// the ack.
  pure def ackImpliesQueuedIn(s: System, audit: Audit, hubs: Set[HubId]): bool =
    hubs.forall(hub => s.ackedAt(hub).subseteq(audit.everQueued.get(hub)))

  // ------------------------------------------------------------------------
  // Known gaps
  // ------------------------------------------------------------------------

  /// K2. What a wallet sees of one transaction never goes backwards: once
  /// served, it is not later pending or missing; once pending, it is not later
  /// missing. This does not hold. Replies are reordered; a published
  /// transaction can be queued again; a flush empties the queue before the
  /// chain has the batch; the node can reject at flush; and two hubs need not
  /// agree.
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

  /// K5. A payload a hub has acknowledged is accounted for: the hub still
  /// holds it, queued or in flight; or the chain has it; or a node judged it
  /// when this hub offered it, and said accepted, already known or rejected.
  /// Being offered is not enough: an offer nothing judged settles nothing.
  ///
  /// This does not hold. The queue lives in memory: a crash after the ack
  /// loses it, and so does a final flush that finds the indexer unreachable.
  pure def ackedIsHeldOrSettledIn(s: System, audit: Audit): bool =
    s.hubIds().forall(hub =>
      s.ackedAt(hub).forall(payload =>
        or {
          s.queuedAt(hub).contains(payload),
          s.inFlightAt(hub).contains(payload),
          s.indexer.published().contains(payload),
          audit.verdicts.exists(verdict =>
            verdict.hub == hub and verdict.payload == payload and verdict.final),
        }))

  /// K6. G6b without its restriction to the first offer. This does not hold
  /// once a hub is stale: requeue judges an entry at the observed tip, which
  /// has stopped, while the flush schedule runs on.
  pure def conformingEveryOfferBeforeExpiryIn(s: System, audit: Audit): bool =
    audit.offers.forall(offer => isConformingAndTimely(s, audit, offer.hub, offer.payload) implies offeredInTime(s, offer))

  // K1. "Told ok" promises nothing about any hub. It is
  // stated as three reachable states, not as a violated invariant, because the
  // invariant is false on the ordinary success path too: the wallet is told
  // before any hub has the frame.

  /// The shim's submissions of `payload`, as (hub, nonce).
  pure def shimSubmissionsOf(s: System, payload: Payload): Set[(HubId, Nonce)] =
    s.hubIds().map(hub =>
      s.submissions(ShimAddr, hub).filter(submit => submit._2 == payload).map(submit => (hub, submit._1))
    ).flatten()

  pure def neverQueued(s: System, audit: Audit, payload: Payload): bool =
    s.hubIds().forall(hub => not(audit.everQueued.get(hub).contains(payload)))

  /// K1a. The wallet was told ok; every submission that reached a hub was
  /// refused; no hub ever queued the payload.
  pure def wToldRefusedEverywhereIn(s: System, audit: Audit): bool =
    s.toldOk().exists(payload =>
      val submitted = s.shimSubmissionsOf(payload)
      and {
        neverQueued(s, audit, payload),
        submitted != Set(),
        submitted.forall(submit =>
          s.acks(submit._1, ShimAddr).exists(ack => ack._1 == submit._2 and ack._2 != WAccepted)),
      })

  /// K1b. The wallet was told ok; no hub has answered any of the frames; no
  /// hub ever queued the payload. The network may leave it so forever.
  pure def wToldNeverDeliveredIn(s: System, audit: Audit): bool =
    s.toldOk().exists(payload =>
      and {
        neverQueued(s, audit, payload),
        s.shimSubmissionsOf(payload).forall(submit =>
          not(s.acks(submit._1, ShimAddr).exists(ack => ack._1 == submit._2))),
      })

  /// K1c. The wallet was told ok though the sweep stopped early: some hub was
  /// never sent the payload.
  pure def wToldPrefixOnlyIn(s: System): bool =
    s.toldOk().exists(payload =>
      val reached = s.shimSubmissionsOf(payload).map(submit => submit._1)
      reached != Set() and reached != s.hubIds())

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

  /// W4. A hub has refused a submission for `refusal`.
  pure def wRefused(audit: Audit, refusal: Refusal): bool =
    audit.refusals.contains(refusal)

  /// W5. An entry nothing judged is back in a queue.
  pure def wRequeuedIn(s: System): bool =
    s.hubIds().exists(hub =>
      s.queuedAt(hub).exists(payload => s.hubs.get(hub).queue.get(payload) > 0))

  /// W6. A requeue dropped an entry that could no longer survive the next
  /// flush: it was given up on while it still had attempts left.
  pure def wDroppedExpiredIn(s: System, audit: Audit): bool =
    audit.dropped.exists(entry =>
      entry.attempts + 1 <= s.hubs.get(entry.hub).params.maxAttempts)

  /// W7. A requeue dropped an entry that was out of attempts: one with no
  /// expiry, which nothing else would ever have stopped.
  pure def wDroppedExhaustedIn(audit: Audit): bool =
    audit.dropped.exists(entry => entry.payload.expiry == None)

  /// W8. The accepted disclosure: a third party that knows a txid learns that
  /// it is queued at a hub. The hub withholds the bytes; it does not withhold
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
      and {
        lookup._1 == reply._1,
        lookup._2 == reply._2,
        reply._3 == WFound({ body: None, height: MEMPOOL_HEIGHT }),
      })

  /// W9. A hub holds a payload it cannot parse; the wallet that sent it asks
  /// that hub for it and is told not found. An entry without a txid can never
  /// be hit.
  pure def wUnparseableMissedIn(s: System): bool =
    tuples(s.events(), s.lookups(ShimAddr)).exists(((event, lookup)) =>
      match event {
        | Got(got) =>
          and {
            got.obs == NotFound,
            got.via == Some(lookup._1),
            s.queuedAt(lookup._2).exists(payload =>
              payload.txid == None and walletTxid(payload) == got.query),
          }
        | _ => false
      })

  /// W12. A queue holds more than its capacity: requeue honours the older
  /// promise over the newer limit.
  pure def wQueueOverCapacityIn(s: System): bool =
    s.hubIds().exists(hub => s.queuedAt(hub).size() > s.hubs.get(hub).params.queueCap)

  /// W13. A lookup has moved on from a hub that did not answer, and the next
  /// hub has answered it.
  pure def wFailoverAnsweredIn(s: System): bool =
    s.shim.waiters.keys().exists(nonce =>
      s.shim.waiters.get(nonce).attempt > 0 and
        s.replies(ShimAddr).exists(reply => reply._1 == nonce and reply._3 != WError))

  /// W14. Two hubs have each published the same payload, in their own flushes,
  /// and it is on the chain: replication, not failover.
  pure def wPublishedByTwoHubsIn(s: System, audit: Audit): bool =
    s.indexer.published().exists(payload =>
      and {
        audit.offers.filter(offer => offer.payload == payload).map(offer => offer.hub).size() > 1,
        s.hubIds().forall(hub =>
          not(s.queuedAt(hub).contains(payload)) and not(s.inFlightAt(hub).contains(payload))),
      })

  /// A hub is publishing a scheduled batch for an epoch the chain has not
  /// reached: its cadence clock is ahead of the true height. The batch is
  /// smaller than the schedule intended.
  pure def flushesAheadOfChain(s: System, hub: HubId): bool =
    val state = s.hubs.get(hub)
    match state.flush {
      | Broadcasting(flush) =>
        not(flush.final) and state.cadenceEpoch() > s.height() / state.params.flushInterval
      | Idle => false
    }

  /// W15. Premature flush: a hub that follows its tip flushes ahead of the
  /// chain, because the tip it was given is ahead of the chain.
  pure def wPrematureFlushIn(s: System): bool =
    s.hubIds().exists(hub => s.hubs.get(hub).cadence == Tracking and flushesAheadOfChain(s, hub))

  /// W18. Early flush: a stale hub flushes ahead of the chain, because its
  /// free-running clock is ahead of the chain.
  pure def wEarlyFreeRunFlushIn(s: System): bool =
    s.hubIds().exists(hub => s.hubs.get(hub).cadence != Tracking and flushesAheadOfChain(s, hub))

  /// W16a. The wallet is served a twin of what it sent: other bytes, same txid.
  pure def wTwinServedIn(s: System): bool =
    s.wasGiven(obs =>
      match obs {
        | Tx(tx) => s.seenByShim().exists(sent => s.toldOk().contains(sent) and areTwins(sent, tx.payload))
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

  /// W17. A hub has queued a payload of the third party's own making.
  pure def wThirdPartyPayloadQueuedIn(s: System): bool =
    s.hubIds().exists(hub => s.queuedAt(hub).intersect(s.thirdParty.own) != Set())

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

  pure def vOfferedBeforeExpiryIn(audit: Audit): bool =
    audit.offers.exists(offer => isSome(offer.payload.expiry))

  pure def vConformingFirstOfferBeforeExpiryIn(s: System, audit: Audit): bool =
    audit.offers.exists(offer =>
      and {
        isSome(offer.payload.expiry),
        offer.nth == 0,
        isConformingAndTimely(s, audit, offer.hub, offer.payload),
      })

  /// The same, for a payload admitted while the hub's tip was behind the chain.
  pure def vConformingOfferAdmittedBehindIn(s: System, audit: Audit): bool =
    audit.offers.exists(offer =>
      and {
        isSome(offer.payload.expiry),
        offer.nth == 0,
        isConformingAndTimely(s, audit, offer.hub, offer.payload),
        val entry = audit.admitted.get((offer.hub, offer.payload))
        entry.tip < entry.height,
      })

  pure def vConformingFirstOfferJudgedIn(s: System, audit: Audit): bool =
    audit.verdicts.exists(verdict =>
      and {
        isSome(verdict.payload.expiry),
        verdict.nth == 0,
        verdict.final,
        isConformingAndTimely(s, audit, verdict.hub, verdict.payload),
      })

  pure def vAckImpliesQueuedIn(s: System): bool =
    s.hubIds().exists(hub => s.ackedAt(hub) != Set())
}

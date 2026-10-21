# The zeronym protocol, specified in Quint

A [Quint](https://quint-lang.org) specification of zeronym: the protocol between a wallet, the shim in front of an operator's indexer, the hub that batches diverted transactions, and the chain.

> [!NOTE]
> Checked against: `main` at `<commit>`.

- [System](#system)
- [Assumptions](#assumptions)
- [Out of scope](#out-of-scope)
- [Threat model](#threat-model)
- [Guarantees](#guarantees)
- [Trust matrix](#trust-matrix)
- [Known gaps](#known-gaps)
- [Findings](#findings)
- [How it is checked](#how-it-is-checked)
- [Future work](#future-work)

## System

> Who the actors are, what each one does, and where each lives in the specification.

The architecture diagram and the prose description of the deployment are in [`zeronym/README.md`](../../README.md).

Only the mixnet transport is modelled; the HTTP transport is out of scope (see [Out of scope](#out-of-scope)).

There are two specifications, sharing one hub function:

- **The protocol specification** (`protocol.qnt`): wallet, shim, network, hub, indexer and an outside third party. Its hub is abstract: a queue and the entries out with a flush, with no tip, schedule or phases. It is checked by random simulation and scripted runs.
- **The hub specification** (`hubMachine.qnt`): one hub, the chain, and the two things the hub asks its indexer (the tip, and a verdict on each broadcast). It owns the schedule, expiry, requeue, crash and drain. TLC visits every reachable state of each of its configurations.

Each component is one total function from its state and one input to its next state and one output. An input that is invalid in the current state returns an error output and leaves the state alone. The state machines hold no protocol logic: a step picks an input, calls the function, and puts the output where it goes. `hub.qnt` and `shim.qnt` are the precise statement of what each does.

| Component | Inputs | Outputs | Seam in the implementation |
|---|---|---|---|
| `hub` | `SubmitHInput`, `LookupHInput` (with the indexer's answer), `TipHInput(height)`, `StaleHInput(estimate)`, `FlushDueHInput`, `VerdictHInput`, `FlushDoneHInput`, `DrainHInput`, `CrashHInput`, `RestartHInput` | `AckOutput`, `LookupReplyOutput`, `BroadcastOutput`, `RequeuedOutput`, `NoHubOutput`, `HubErrorOutput` | `Hub::admit`, `Hub::lookup` (`hub/src/server.rs`), `run_listener` (`hub/src/nym.rs`), `TipTracker::observe`, `cadence_height`, `flush` (`hub/src/batcher.rs`), `Queue::requeue`, `Queue::begin_draining` (`hub/src/queue.rs`) |
| `shim` | `SendTxSInput` (with whether the transport took the frame), `GetTxSInput`, `FrameSInput`, `LookupTimeoutSInput` | `ForwardOutput`, `DivertedOutput`, `SendDoneOutput`, `LookupSentOutput`, `LookupDoneOutput`, `NoShimOutput`, `ShimErrorOutput` | `send_transaction`, `divert`, `get_transaction` (`shim/src/intercept.rs`), `NymHandle::submit`, `get_transaction`, `deliver` (`shim/src/nym.rs`) |
| indexer | `BroadcastIInput`, `LookupIInput`, `AdvanceIInput`, `MineIInput` | `VerdictOutput`, `AnswerOutput`, `NoIndexerOutput` | the mock indexer in `hub/tests/common/mod.rs` |

<details>
<summary><b>Indexer, network and third party</b></summary>

- **Chain and indexer** (`indexer.qnt`). A transaction's status only moves forward: absent, in the mempool, mined. The indexer answers lookups and gives a verdict on each broadcast.
- **Network** (`spells/soup.qnt`). A message is added to the soup and never removed. It may be delivered any number of times, in any order, or never.
- **Third party.** A client of the hub's public, unauthenticated address. It may learn a txid out of band, look it up, and resubmit any payload the chain has published.

</details>

<details>
<summary><b>Wire</b>: the three lookup replies and what the wallet sees</summary>

A hub answers a lookup with one of three wire replies, and the shim turns that into what the wallet sees (`wire.qnt`).

| The hub's situation | Reply on the wire | What the wallet sees |
|---|---|---|
| The transaction is queued here | found, height 0, no body | Pending |
| Its indexer has the transaction | found, with the height and the body | The transaction, if the body's txid is the one asked for; otherwise not found |
| Its indexer says not found | not found | Not found |
| Its indexer cannot be reached | error | Unavailable |
| Its indexer says "found" with no body, at any height (a fault) | error | Unavailable |

</details>

"Pending" has no reply of its own on the wire: it is a found reply with nothing in it. So the hub's queue hit and an indexer's empty "found" are the same bytes, and neither the shim nor the wallet can tell "queued at the hub" from "an indexer said found and returned nothing". Before #85 one misbehaving indexer endpoint was enough to produce the second (finding 8); the hub's client now reads that answer as the endpoint failing.

## Assumptions

> What must be true of the world for the claims to apply.

- **Roles.** The shim and the hub both run attested. An honest shim or hub runs a binary built from the commit under *Checked against*, which reproduces to the hash in its `deploy/EXPECTED_SHA256` and passes `caution verify`. The shim is modelled as honest in every configuration: it sees every migration in plaintext and controls everything the wallet observes, so no wallet-facing guarantee could survive its compromise. The hub is modelled as honest or Byzantine, not because the design relies on it more, but to measure how much each guarantee depends on the hub's enclave. The indexer runs outside any enclave, so a Byzantine indexer is the realistic adversary. One component is Byzantine at a time.
- **Honest and Byzantine.** An honest component takes exactly the transition its function gives. A Byzantine one takes any member of a finite set that contains the honest transition (`byzantineContainsHonestTest`). No message or state field records which it took.
- **Byzantine hub** (protocol specification only). It admits or refuses a submission whatever the admission rules say, and may send any reply to a lookup. Its ack is modelled as truthful: a real one could ack anything, but nothing reads an ack. Every other move is the honest one: it cannot evict or withhold a queued entry, flush off schedule, or send a frame nobody asked for.
- **Byzantine indexer.** Any verdict, with the transaction relayed or not. Any lookup answer built from a payload it was offered or one the chain published. In the hub specification it also reports any tip.
- **Network.** May lose, duplicate, delay and reorder frames. Cannot forge or read them.
- **Third party.** Looks up txids it knows and submits payloads it has learned or the chain has published. It cannot read or forge frames, so it does not know a nonce and cannot answer the shim.
- **Nonces** are unique. A counter stands for an unguessable value.
- **Chain.** No reorg of an included transaction and no mempool eviction. The operator's indexer publishes nothing.
- **Wallets.** A supported ("conforming") wallet sets an expiry at least `MIN_WALLET_EXPIRY` after the height it builds at, and its frame reaches the hub within `DELIVERY_LAG` blocks. A wallet asks only about transactions it has sent.
- **Hub schedule** (hub specification only). How promptly the hub learns the tip depends on the configuration: at every block, up to the reorg allowance behind, or not at all for a while. Fewer blocks arrive while a flush is in flight than the mining margin reserves. The code enforces neither; see [Configurations](#configurations) and findings 1, 2 and 4.
- **Time.** There is no clock. A timeout may happen at any moment; the staleness window is counted in blocks.

Each configuration's assumptions are the guard of its named `init`. A guard that is false leaves no initial state, and the gate fails on that.

## Out of scope

> What the specification deliberately does not cover, and why.

### One hub

The specification checks one hub; production runs one or more, replicated: every shim sends every submission to every hub, and each hub that receives a migration queues and broadcasts it (`zeronym/shim/src/nym.rs:602-647`). The single hub is a scope choice, not a claim about production.

Not checked as a result: two hubs disagreeing about one transaction; duplicate publication, and a second enclave holding the plaintext, both accepted deliberately in production; told ok after a partial send, and its anonymity cost; a lookup moving to the next address on a timeout (finding 9); that one Byzantine replica is enough to break G2 and G4.

Argued, not checked, on the assumption that hubs share nothing but the chain and the indexer:

- **Compose per hub:** G1, G3, G4 (which is why its name says "per hub"), G6b, and gap K5.
- **Compose only if every hub is honest:** G2. One Byzantine replica holds the same bytes and can give them away.
- **Do not compose:** K1 and K2 each gain a cause with a second hub.

### Not modelled

| Item | Reason |
|---|---|
| Attestation, PCRs, TLS, STEVE, keymaker quorum | No in-protocol messages exist. Represented by the roles |
| Mixnet internals: SURBs, Sphinx, cover traffic, gateways, throttling; the shim's client rotation; both `nym_driver.rs` | Their protocol-visible effect is loss and delay |
| The hub's lookup concurrency bound, reply deadline and dropped acks | Refinements of "the network lost the message" |
| Wall-clock time | There is no clock: the staleness window is counted in blocks, and a timeout may happen at any moment |
| Multiple indexer endpoints | One abstract indexer stands for all of a hub's endpoints. The trust matrix says, for each Byzantine-indexer cell, whether one lying endpoint suffices |
| Wire codecs, byte layout, malformed frames | Pinned by the Rust tests and golden vectors in both crates. The specification works at the level of what a reply means, and does not bind the codec |
| Reorgs of included transactions, mempool eviction | Assumed away: a transaction's chain status only moves forward |
| Anonymity-set size, shuffle, simultaneity | Not properties of a single run |
| Covert channels: extra bytes on the real wire, sizes and timing | Messages are strictly typed. A message can carry only its type's fields, so one with extra data is unrepresentable in the model |
| The forward-only shim, transparent-pool RPCs, health, address and attestation endpoints, logging | Not part of the divert protocol |
| The HTTP transport, where the shim waits for the hub's verdict before answering the wallet (`HubTransport::Http`, `--hub`) | The production deployment is the mixnet (`HTTP_SUBMIT=0`). So there is no configuration here in which the shim's ok means the hub has the transaction |
| A Byzantine shim | The shim runs attested, sees every migration in plaintext and controls what the wallet observes. Every wallet-facing guarantee assumes it honest |
| What the hub's ack says | Nothing reads an ack: the shim tells the wallet ok without waiting for it. That an accepted ack is only for a queued payload is not a checked property, and a Byzantine hub's ack is modelled as truthful |
| The hub's capacity and size refusals, and denial of service generally | Not claimed properties. The shim's own too-large refusal is modelled |
| A third party submitting payloads of its own making | Possible, since the hub's address is public and unauthenticated. The model's third party submits only what it has learned or the chain has published |
| Wallets whose expiry is below the supported floor, and submissions that arrive later than the delivery lag | No schedule guarantee is made for them. Admission's own claim that every admitted entry "provably survives" its scheduled flush (`zeronym/hub/src/queue.rs`) is G8, for entries not requeued; before #85 it did not hold under a tip reported behind the chain (finding 6) |
| A different transaction with the same txid (a twin) | No configuration contains one. The pure wire test shows the shim would serve it (`servedOnlyOnMatchingTxidTest`): the shim compares the txid and nothing else |
| A Byzantine hub in the hub specification | It produced no finding, and an adversary's moves are not an implementation to check against the abstract hub. The protocol specification keeps its Byzantine hub, which lies in lookup replies and admits or refuses as it likes |
| More than one Byzantine component at once | The trust matrix is single-fault |
| Liveness: that anything eventually happens, such as a submitted migration being published | The network may lose everything, and nobody waits for an ack |

## Threat model

> What the specification is afraid of, who could cause it, and whether a property answers it.

The deployment targets the server-side and network-metadata adversaries of Taylor Hornby's [wallet app threat model](https://zcash.readthedocs.io/en/latest/rtd_pages/wallet_threat_model.html); the Security section of [`zeronym/README.md`](../../README.md) says what is and is not protected. This specification covers the part of that which is a property of protocol runs. Which guarantee answers a threat is the "Answers" column under [Guarantees](#guarantees), and which gap records one that is not prevented is the "Threat" column under [Known gaps](#known-gaps).

| # | Threat | Adversary | Status |
|---|---|---|---|
| T1 | The operator sees a migration's contents | Operator behind the shim | Answered |
| T2 | Someone obtains a queued migration's bytes and publishes it early, breaking the batch | Unauthenticated third party; Byzantine hub or indexer | Answered, if the hub and the indexer are honest |
| T3 | The wallet is served a different transaction than the one it asked for | Byzantine hub or indexer | Answered for the txid; not for the bytes or the height |
| T4 | The wallet is told something false about its transaction's status | Byzantine hub or indexer; network reordering | Answered for each answer, if the hub and the indexer are honest; not across answers (T8) |
| T5 | A supported wallet's migration expires while the hub holds it | Chain timing; a flaky tip; Byzantine hub or indexer | Answered in part: under a timely or regressing tip, not under a stale one |
| T6 | The hub silently drops or admits entries outside its rules | Hub implementation error | Answered |
| T7 | The wallet is told "sent" but the hub never admits it | Network; the hub's own refusals | Not prevented |
| T8 | The wallet sees its transaction's status go backwards | Network reordering; resubmission; the flush window (after a flush empties the queue, before the indexer has the batch) | Not prevented |
| T9 | An acknowledged migration is lost to a crash, a failed final flush, or a requeue drop | None needed | Not prevented |
| T10 | A third party who knows a txid learns it is queued | Unauthenticated third party | Accepted |
| T11 | A lying indexer makes the hub flush early, shrinking the batch, or stop admitting | Byzantine indexer; one endpoint suffices | Not prevented; recorded |
| T12 | Any admitted transaction, including a late arrival or one from an unsupported wallet, is offered too late | As T5 | Not checked |
| T13 | The hub acknowledges a migration it never queued | Byzantine hub | Not modelled: nothing reads an ack |
| T14 | Linking a wallet to its migration by source IP | Network observer; operator | Not modelled. Claimed protected in `zeronym/README.md` |
| T15 | Linking by submission size and arrival time | Operator | Not modelled. Listed as not protected in `zeronym/README.md` |
| T16 | The operator recovering txid and value through transparent-pool queries | Operator | Not modelled. Listed as not protected in `zeronym/README.md` |
| T17 | Batch-size and timing anonymity; partitioning the anonymity set across hubs | Network observer | Out of scope: timing and anonymity are not trace properties here, and there is one hub |
| T18 | A compromised shim or enclave host | Host; a malicious build | Assumed away. A compromised host is delegated to AWS in `zeronym/README.md`; a malicious shim build is excluded by attestation and is not discussed there |

## Guarantees

> Promises about whole runs of the system: this bad thing never happens.

| Id | Name | What it says | Answers | Checked by |
|---|---|---|---|---|
| G1 | `operatorBlind` | Every message the shim sends the operator's indexer is a pass-through transaction, by message type | T1 | Simulation |
| G2 | `queuedBytesConfidential` | Everything the third party has learned is on the chain, or was a pass-through transaction given to the operator's indexer. Its knowledge is derived from the replies sent to it and the operator's view | T2 | Simulation |
| G3 | `txidAuthenticity` | A transaction served to the wallet has the txid asked for. It need not be the bytes the wallet sent, and its height is whatever the hub said | T3 | Simulation; `servedOnlyOnMatchingTxidTest` exhaustively |
| G4 | `lookupValidityPerHub` | Every lookup answer other than "unavailable" was true at the hub that gave it at some point between request and answer. Not-found during the flush window counts as true, as the implementation intends (`Hub::lookup`, `zeronym/hub/src/server.rs`). It does not say that successive answers agree | T4 | Simulation |
| G5 | `pendingOnlyFromQueue` | Every "pending" the wallet is told was true at the hub that gave it: the transaction was queued there. G4 restricted to pending | T4 | Simulation |
| G6b | `conformingFirstOfferBeforeExpiry` | A supported wallet's transaction is offered with the mining margin to spare, the first time a hub offers it. About the margin left when the flush begins, not about acceptance; nothing about a later offer of a requeued entry | T5 | TLC, exhaustive |
| G8 | `admissionMatchesCadence` | Every entry admitted, and not requeued, survives the first flush the cadence has not spent, with the mining margin to spare | T5 | TLC, exhaustive |
| G7 | `wellFormedTest` | Structural sanity of the hub: a queued entry is within its attempts and a down hub holds nothing | T6 | Exhaustive test over every reachable hub state |
| A2 | `neverEvictTest` | An entry leaves the hub's queue only into a flush, or because the hub went down or exited after its final flush | T6 | Exhaustive test over every reachable hub state and input |
| A3 | `drainIsFinalTest` | A draining honest hub's queue gains only what a flush hands back | T6 | Exhaustive test over every reachable hub state and input |

An unchecked corollary of G6b: if fewer blocks arrive while a flush is in flight than the mining margin reserves, a node that judges a supported wallet's first offer finds it unexpired. Nothing in the code bounds a flight in blocks (finding 4).

A2 and A3 constrain a single hub step, not a state; the rest are state invariants. What each entry under "Checked by" is worth is under [How it is checked](#how-it-is-checked).

Each pure function also has exhaustive tests over small inputs, in `tests/*Test.qnt`.

## Trust matrix

> For each guarantee, whose honesty it depends on.

Single-fault. "holds" is a checked row on the named [configuration](#configurations). "required" is a scripted run in which the component is Byzantine and the guarantee fails; the run asserts the guarantee in the state just before the Byzantine step, so the lie is what breaks it.

| | All honest | Byzantine hub | Byzantine indexer |
|---|---|---|---|
| G1 | holds (`baseline`) | holds (`byzHub`) | holds (`byzIndexer`) |
| G2 | holds (`baseline`) | **required**: `hubServesQueuedBodyTest` | **required**: `indexerServesUnpublishedBodyTest`. One endpoint suffices |
| G3 | holds (`baseline`) | holds (`byzHub`) | holds (`byzIndexer`) |
| G4 | holds (`baseline`) | **required**: `hubDeniesQueuedTest`, `hubServesFalseHeightTest` | **required**: `indexerServesFalseHeightTest`. One endpoint suffices |
| G5 | holds (`baseline`) | **required**: `hubForgesPendingTest` | holds (`byzIndexer`); `indexerEmptyAnswerIsUnavailableTest` |
| G6b | holds (`timely`, `flakyTip`) | argued, not checked: a hub that admits while it has no tip breaks it | **required**: `indexerWithholdsTipFromConformingTest`. Needs every endpoint |
| G8 | holds (`timely`, `flakyTip`, `staleLagWithSlack`) | argued, not checked | argued, not checked: admission never judges below the recorded boundary, whatever the tip |
| A3 | holds | argued, not checked: draining is an admission rule | holds |

- A hub folds several indexer endpoints into one answer: the tip is the maximum, a lookup takes the first "found", a broadcast takes the best verdict. So one misbehaving endpoint can raise the tip, inject a lookup answer or change a verdict, while lowering or freezing the tip takes every endpoint. The model has one abstract indexer and does not enforce that difference; the indexer cells say which each needs.
- There is no shim column: every wallet-facing guarantee assumes an honest, attested shim.
- G3 is the only wallet-facing guarantee that survives a Byzantine hub or indexer, and it authenticates the txid only.
- G1 depends on the shim alone.
- The hub specification has no Byzantine hub, so the hub column of G6b and A3 is an argument. A Byzantine hub breaks G6b by admitting while it has no tip, when an honest hub refuses everything. Admitting past the expiry rule cannot break it, because that rule never refuses a supported wallet's timely transaction (`conformingTimelyPayloadIsAdmissibleTest`). The protocol specification's Byzantine hub backs the other rows.

## Known gaps

> Things you might expect to hold that don't, each with a concrete example run. Every component is honest in all of them.

"By design" means the implementation says so itself. "Found here" means this specification showed it and nobody has yet decided whether it is acceptable. "Where" names a [configuration](#configurations).

| Id | Threat | What is lost | Status | Where | Checked by | Scripted run |
|---|---|---|---|---|---|---|
| K1 | T7 | Told ok does not mean the hub ever admits it: it may refuse the frame, or never receive it | By design: the hub's verdict "is deliberately not waited for" (`zeronym/shim/src/hub.rs:289-295`) | `baseline` | scripted runs | `toldOkThenRefusedTest`, `toldOkAndNeverDeliveredTest` |
| K2 | T8 | `statusNeverRegresses`: what a wallet sees of one transaction never goes backwards | By design for the flush window, which the code accepts in a comment on `Hub::lookup` (`zeronym/hub/src/server.rs`); resubmission of a published transaction is finding 10, not triaged | `baseline` | simulation, violated | `repliesReorderedTest`, `walletResendsPublishedTest`, `thirdPartyResubmitsPublishedTest`, `flushWindowTest`, `rejectedAtFlushTest` |
| K3' | T5 | G6b when the expiry floor leaves no slack for the reorg allowance | Found here (finding 3); not triaged | `flakyTipNoSlack` | TLC, violated | `conformingMissesMarginWithoutSlackTest` |
| K4 | T5 | G6b on the shipped relation between the constants, across a tip silence shorter than the staleness window | Found here (finding 1); not triaged | `staleLag` | TLC, violated | `silenceAcrossBoundaryMissesMarginTest`; contrast `sameSilenceWithSlackKeepsMarginTest` |
| K5 | T9 | `ackedIsHeldOrSettled`: an acknowledged payload is still held by the hub, or is on the chain, or a node judged it | By design: the queue is in memory only, and the code logs what it loses (`zeronym/hub/src/batcher.rs:337-347`, `:467-478`). How much one unjudged flush drops is not triaged (finding 5) | `timely` | TLC, violated three ways | `ackedThenCrashedTest`, `ackedThenLostAtDrainTest`, `requeueDropsAckedAsExpiredTest` |
| K6 | T5 | `conformingEveryOfferBeforeExpiry`: G6b for every offer, not only the first | Found here (finding 7); not triaged | `staleLag` | TLC, violated | `requeuedPastExpiryTest` |
| K9 | T5 | G6b even when the expiry floor is wide enough to cover a tip silence: a stale hub's free-running clock flushes an empty queue early and spends the next epoch, so a transaction admitted after the tip returns waits a full interval | Found here (finding 2). #85 fixed admission's half (G8); the spent epoch remains, and this counterexample is unchanged | `staleLagWithSlack` | TLC, violated | `earlyFlushSpendsTheNextEpochTest` |

K1 is not an invariant because it would be false on the ordinary success path too: the wallet is told ok before the hub has the frame.

Behaviours that are accepted or only recorded:

| Behaviour | Threat | Shown by |
|---|---|---|
| **Accepted disclosure.** A third party that knows a txid learns that it is queued. The hub withholds the bytes, not the fact. The implementation leaves this open deliberately (`Hub::lookup`, `zeronym/hub/src/server.rs`): "the 200-versus-NotFound distinction still discloses that a given txid is queued here. Closing that too means answering NotFound, which costs a wallet the ability to tell "pending" from "never seen"" | T10 | `thirdPartyLearnsItIsQueuedTest`; witness `wQueuedDisclosed` on `baseline` |
| **Premature flush.** A Byzantine indexer reports a tip ahead of the chain and the hub flushes before the true boundary: a batching harm, not an expiry one. What a report far ahead of the chain does is finding 11 | T11 | `tipAheadOfChainFlushesEarlyTest` |
| **Early flush by the free-running clock.** A stale hub's clock is ahead of the chain and it flushes before the true boundary, with every component honest | T11, with no liar | `freeRunningClockFlushesEarlyTest` |
| **Unparseable and queued.** A payload the hub cannot parse has no txid, so a lookup misses it while it is queued | none | `unparseableIsQueuedAndMissedTest` |

## Findings

> Where the model disagrees with what the code or its comments assume.

Findings 6 and 8 are fixed by #85, and finding 2 in part. Nothing else here has been fixed. "Code read" means the cited lines were read and match the model; nothing was run against the Rust. The severity beside a code bug is a judgement from that reading, not from a reproduction: HIGH, one in-scope adversary degrades batching or availability for every user of the hub; MEDIUM, one wallet loses a transaction or is misled about one; LOW, a transient wrong status or a wrong operator signal. Only code bugs carry a severity, so a row without one is not thereby minor: finding 5 is by design and may matter more than any MEDIUM bug.

| # | Finding | Kind | Shown by | Against the Rust |
|---|---|---|---|---|
| 1 | **A short tip silence costs a supported wallet its mining margin.** The cadence follows the last tip seen, so a silence across a flush boundary delays the flush until the hub goes stale. The staleness window is 12 blocks and the slack is 10, so on the shipped constants the first offer is at `created + 37` against an expiry of `created + 40`: three blocks of margin where four are reserved. It needs the hub not to hear the tip while a broadcast would still succeed, and the tip not to return before the hub goes stale; the tip and the broadcast use the same endpoints, so an unreachable indexer delays both | Latent: a real gap between the constants, in a narrow environment | K4, `silenceAcrossBoundaryMissesMarginTest`; TLC on `staleLag` | Code read: `batcher.rs:40-71`, `:227-247`. The one-block shortfall reads 15 minutes as exactly 12 blocks |
| 2 | **An early free-running flush spends the next epoch.** A stale hub's clock runs ahead, flushes an empty queue and records that epoch. When the tip returns, admission counts on a flush that has already happened, and the transaction waits a full interval. A wider expiry floor does not fix it. The supported-wallet witness also needs a second, shorter silence | Code bug (MEDIUM), partly fixed by #85; one root cause with finding 6: the loop records the epoch from the cadence height, and admission estimates the next flush from the observed height | K9, `earlyFlushSpendsTheNextEpochTest`; TLC on `staleLagWithSlack` | Code read: `batcher.rs:236-247`, `:316-326`. The comment there calls a clock that runs ahead "the safe direction" |
| 3 | **The reorg slack holds by coincidence of constants.** The expiry floor minus the three-term budget (flush interval, mining margin, delivery lag) is 10 blocks, exactly the reorg allowance, and startup validation checks only the three-term sum. Without the slack a supported wallet's transaction misses its margin | Latent: no failure on the shipped constants | K3', `conformingMissesMarginWithoutSlackTest`; TLC on `flakyTipNoSlack` | Code read: `batcher.rs:40-59`, `:101-113` |
| 4 | **A flush's flight is bounded in time, not in blocks.** The budget leaves exactly the mining margin at the offer, so blocks that arrive while the batch is in flight come out of it. Each call is bounded at 10 s with 64 in flight, about 160 s at the 1,024-entry cap, which is about two nominal blocks against a margin of four. Block arrival is not bounded by the clock, and requeue does not enforce the entry cap | Bounded in time, not in blocks; reachable only with a slow or selectively hanging indexer | Not a checked property: the corollary of G6b under [Guarantees](#guarantees) assumes it away | Code read: `chain.rs:49`, `:59`. Estimate, not measured: with exponential block times at a 75 s mean, a flight of that length sees four or more blocks about one time in six |
| 5 | **An acknowledged payload can be lost three ways with every component honest:** a crash; a draining hub's final flush that finds the indexer unreachable; a requeue that gives the entry up as expired. On the shipped constants a requeue keeps an entry only if it was created at most 16 blocks before the flush (`40 - 20 - 4`). For wallets at the 40-block floor, one unjudged flush therefore drops between 20% and 50% of a batch, depending on delivery lag, each entry with 14 to 23 blocks of life left | By design; the magnitude is not triaged | K5 and its three runs; TLC on `timely` | Code read: the loss is logged at error level (`batcher.rs:337-347`, `:467-478`) and unit-tested (`queue.rs`, `a_requeue_gives_up_on_an_entry_that_can_no_longer_be_mined`); the rule is `queue.rs:507-520`. The percentages are arithmetic on the constants, assuming arrivals uniform over the interval |
| 6 | **An entry admitted while the tip is reported below a boundary already flushed waits a full interval.** Before #85 admission counted on the flush at that boundary, which will not happen again. Any late arrival met it, and one that expired before that next flush was first offered after its expiry; the supported-wallet case was a resend after a crash, which a restarted hub offered with less than the mining margin. Since #85 admission counts on the next unspent flush, and that resend is refused | Code bug (MEDIUM), fixed by #85; one root cause with finding 2 | G8 on `flakyTip`; `crashThenLateDuplicateTest` | Code read: `batcher.rs:177-186`, `:316-327`; `queue.rs:294`, `:507-520` |
| 7 | **On a stale hub a requeued entry is offered again past its expiry, and only the attempt bound stops it.** Each requeue judges the entry against the same stale observed tip, so the expiry rule never gives it up. A supported wallet's transaction is offered a third time after its expiry, and is finally dropped as exhausted. The late offers normally reach no node: a stale hub whose flushes come back unjudged almost always has an unreachable indexer | Real behaviour that contradicts a comment; low harm | K6, `requeuedPastExpiryTest`; TLC on `staleLag`; `expiringEntryDroppedAsExhaustedTest` | Code read: `batcher.rs:417-422`; `queue.rs:197` says "Only reachable for a payload with no expiry". The shipped bound is 8 requeues |
| 8 | **One indexer endpoint can make a wallet see "pending" for a transaction nobody holds.** "Found, height 0, no body" from an indexer is byte-identical to the hub's own queue-hit reply, and the hub forwards it unchanged | Code bug (MEDIUM), fixed by #85: an ambiguous wire reply, and no check on an indexer's answer | `sentinelCollisionTest`, `indexerEmptyAnswerIsUnavailableTest` | Code read: `server.rs:445-451`, `chain.rs:284-290`, `:305-319` |
| 9 | **Lookups choose a hub by apparent liveness.** A lookup starts at a rotating cursor and moves to the next address only on a timeout, the pattern the submit path forbids. Whoever can make one hub time out decides which hub answers | Undecided: deliberate and tested, and in conflict with a rule written for submits. The `PRODUCTION.md` that rule cites is not in the repository | Not modelled: needs more than one hub | Code read: `shim/src/nym.rs:746-797`, against the rule at `:628-633`; `shim/tests/nym.rs` asserts the behaviour today |
| 10 | **Anyone can make the hub answer "pending" for a published transaction, and pad its batch-size signal.** The queue deduplicates against resident entries only, so published bytes are admitted again if resubmitted, and a lookup answers from the queue first. The wallet reads "pending" for a transaction it has already been served, until the next flush. The flush also counts an already-known verdict toward the achieved batch size, which the code calls "the honest measure of the privacy", so free, public bytes can raise that figure and suppress the warning for a batch of one | Code bug (LOW): the comment on `Hub::lookup` calls a resubmit harmless. The batch-size figure is logged and returned but drives no decision | K2, `thirdPartyResubmitsPublishedTest`, `walletResendsPublishedTest` | Code read: `queue.rs:308-310`, `:358-359`; `Hub::lookup` in `server.rs`; `batcher.rs:352`, `:389`, `:480`. Not checked: that a node answers already-known for a transaction that is already mined |
| 11 | **One high tip report pins the hub's tip.** `observe` adopts any higher height and ignores a later drop larger than the reorg allowance. So one endpoint reporting a height P makes every real height look like an oversized regression until the chain reaches P minus the allowance, which for a large P is until restart. The epoch jumps, so the hub flushes once off cadence; after that admission refuses every expiring transaction, because the observed height is P | Code bug (HIGH): the code says taking the maximum over endpoints defends the clock against being advanced, and it defends only against slowing it | Found by reading; not modelled, because the model's heights are bounded. `tipAheadOfChainFlushesEarlyTest` shows the one early flush | Code read: `batcher.rs:19-22`, `:171-203`, `:316-326`; `chain.rs:176-201` |

On finding 2, the model lets the free-running clock be at most one flush interval ahead, so it can spend one epoch. `cadence_height` has no such cap. Reading that code, a clock further ahead would skip more than one boundary; the model does not exhibit that.

## How it is checked

> How much to rely on a green row, and how to produce one.

```sh
sh zeronym/spec/quint/check.sh
```

"Holds" means one of three things, and the "Checked by" column says which:

- **TLC, exhaustive.** The hub specification. TLC visits every reachable state of the named configuration. The configurations are small (two or three payloads, a schedule scaled down from the shipped one, heights up to 12), so this is exhaustive for those parameters and not beyond them.
- **Exhaustive test.** `quint test` over a small finite universe: every input of a function, or every reachable hub state.
- **Simulation.** The protocol specification. `quint run`: at most 40 or 80 steps per trace, 2000 random traces, one seed. Not a proof and not exhaustive to any depth; a property that holds is one no sampled trace violated.

`quint verify` has not been run on any part of this. TLC is run through `tlc.sh`, because the compiled specification is larger than the Apalache server accepts.

| Tier | What | Expectation |
|---|---|---|
| 1 | `quint typecheck` on every file | ok |
| 2 | `quint test` on every test file | all pass, and each file reports at least the count `check.sh` gives it |
| 3 | `quint run`, invariants | "holds" rows hold; K2 is violated |
| 3b | `quint run`, witnesses | every listed state is reached at least once; no invariant is violated on the way |
| 4 | `tlc.sh`, one row per invariant and configuration | "holds" rows hold over every reachable state; "violated" rows are violated by a counterexample no longer than the recorded one |

Quint 0.33.0 is pinned (`npx --yes @informalsystems/quint@0.33.0` by default; set `QUINT=quint` to use an installed one). Tier 4 needs Java and Apalache 0.62.1, whose jar carries TLC; without either the tier fails, it never skips. `CHECK_TIERS=simulation` runs tiers 1 to 3b and `CHECK_TIERS=tlc` tiers 1 and 4; CI runs them as two jobs. All four tiers take about six minutes on a 16-core machine. It has not been timed on a CI runner.

### Configurations

A configuration is a value held in the state and selected by a named init.

#### Protocol specification

At most 3 sends and 3 lookups by the wallet, 3 requests by the third party.

| Configuration | Hub | Indexer |
|---|---|---|
| `baseline` | honest | honest |
| `byzHub` | **Byzantine** | honest |
| `byzIndexer` | honest | **Byzantine** |

#### Hub specification

The schedule flushes every 3 blocks with a mining margin of 2, a delivery lag of 1, a reorg allowance of 1, a staleness window of 3 and an expiry floor of 7. It is the shipped schedule scaled down (interval 20, margin 4, lag 6, reorg allowance 10, staleness window 12 blocks, expiry floor 40), keeping the relations between the constants that the findings turn on.

| Configuration | Differs from `timely` by | For |
|---|---|---|
| `timely` | | G6b, K5 |
| `flakyTip` | the tip may be reported up to the reorg allowance behind | G6b under regression; G8, finding 6 |
| `flakyTipNoSlack` | and the expiry floor is 6, leaving no reorg slack | K3' |
| `staleLag` | the hub may hear no tip, and goes stale | K4, K6 |
| `staleLagWithSlack` | and the expiry floor is 8, enough to cover the silence | finding 2 |
| `byzIndexer` | a Byzantine indexer, and a tight-expiry payload | the trust matrix |
| `unknownUpgrade` | an unparseable payload; scripted runs only | the attempt bound |

In every configuration a due flush begins before the next block, and at most one block arrives while a flush is in flight. Under `staleLag` a hub is stale once it has heard no tip for the staleness window; its free-running clock is then assumed never behind the chain and at most one flush interval ahead of it. The implementation relies on "never behind" and does not enforce it (`zeronym/hub/src/batcher.rs:64-67`).

<details>
<summary>What TLC visits</summary>

With 8 workers, on the machine this was written on:

| Configuration | Invariant | Verdict |
|---|---|---|
| `timely` | G6b | holds, 20 030 states, depth 40 |
| `timely` | K6's predicate | holds, 20 030 states, depth 40 |
| `flakyTip` | G6b | holds, 81 518 states, depth 36 |
| `timely`; `flakyTip`; `staleLagWithSlack` | G8 | holds: 20 030; 81 518; 952 657 states |
| `flakyTipNoSlack` | G6b (K3') | violated, 12 states |
| `staleLag` | G6b; K6 | violated, 13; 13 states |
| `staleLagWithSlack` | G6b (finding 2) | violated, 19 states |
| `timely` | K5 | violated: 5 states; 8 without a crash; 12 without a shutdown either |
| `byzIndexer` | G6b | violated, 16 states |

TLC runs with deadlock checking off, so a machine whose steps had died would hold everything. The gate therefore also requires one reachable state per family of steps, and the antecedent of G6b, each as a violated `not(..)` row.

</details>

<details>
<summary>The abstraction lemma</summary>

The protocol specification's hub is the abstract one in `abstractHub.qnt`. `hubTest` checks, over every reachable state of the real hub function and every input, that each real step is a step of the abstract hub (`abstractionTest`), and that each abstract move has a real step behind it (`realisesTest`). So an invariant that holds over the abstract hub, and reads only queue membership and wire replies, holds over the real one: that covers G2, G3 and G4. It does not transfer reachability: the abstract hub answers where the real one is down or stale, so a violation shown over it is a state of the abstract hub. The reachable set is computed at a smaller schedule than the hub specification's and carried over by argument.

</details>

### Layout

Only `protocol.qnt` and `hubMachine.qnt` declare variables, and no module declares a constant. Every other module is pure.

| File | Owns |
|---|---|
| `spells/basicSpells.qnt`, `spells/soup.qnt` | `Option` and set and map helpers; the message soup |
| `types.qnt` | The vocabulary: payloads, verdicts, refusals, roles, observations |
| `wire.qnt` | The four frames; `render`, `meaning`, `interpretReply` |
| `indexer.qnt` | The chain and indexer as a relation, honest and Byzantine |
| `hub.qnt` | `hub(state, input)`: admission, the tip rule, the flush cycle, requeue; the Byzantine relation |
| `shim.qnt` | `shim(state, input)`: routing and reply correlation |
| `hubMachine.qnt` | The hub specification: one hub, the chain, its configurations and invariants |
| `abstractHub.qnt` | The hub as the protocol sees it |
| `protocol.qnt` | The protocol specification: state, steps, guarantees, gaps, witnesses, configurations |
| `tests/wireTest.qnt`, `indexerTest.qnt`, `shimTest.qnt`, `hubTest.qnt` | The functional properties; in `hubTest.qnt` also G7, A2, A3 and the abstraction lemma |
| `tests/hubScenariosTest.qnt` | Scripted runs of the hub specification: one per gap and per trust-matrix cell |
| `tests/scenariosTest.qnt`, `tests/trustTest.qnt` | Scripted runs of the protocol specification: gaps and witnesses; trust-matrix cells |
| `check.sh`, `tlc.sh` | The gate; one TLC check of one invariant on one configuration |

## Future work

> What would make these results bind the code.

1. **Failing Rust tests for the findings.** Findings 2, 6 and 8 have Rust tests, written with their fixes in #85; the part of finding 2 that #85 leaves has none. The other findings are shown on the model and matched to the code by reading. Finding 1 fits `batcher.rs`'s unit tests as 2 and 6 did. Finding 4 is impractical to reproduce.
2. **Model-based testing with `quint-connect`.** Depends on 1, and needs driver code in Rust. The specification is shaped for it: every branch of a step is a named action with its choices as named picks; each step gives one input to one component function and applies one output; and those functions map onto the seams in the table under [System](#system).
3. **`quint verify` with Apalache for a subset of the claims.** The compiled specification is too large for the Apalache server today. A subset small enough to pass would give bounded symbolic checking of the protocol guarantees, which are simulated only.

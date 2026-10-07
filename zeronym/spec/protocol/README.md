# The zeronym protocol, specified in Quint

A specification of the protocol between a wallet, the shim in front of an
operator's indexer, the hubs that batch diverted transactions, and the chain.
It is written from the protocol, not from the code's structure: it says what a
wallet can rely on, which components each guarantee trusts, where the known
gaps are, and it checks each of those statements.

It is separate from `zeronym/spec/divert.qnt`, which it does not replace or
modify.

## What "holds" means here

**Bounded random simulation.** Every "holds" below was produced by
`quint run`: fixed constants, at most 40, 60 or 80 steps per trace, 2000
random traces per run (a few rows more), one seed. It is not a proof and it is not exhaustive to any
depth. A property that "holds" is one no sampled trace violated.

**`quint verify` has not been run**, on Apalache or on TLC, by anyone, on any
part of this specification. The commands are given under
["Bounded model checking (not run)"](#bounded-model-checking-not-run).

**The two-state properties A1-A3 are typechecked only.** Their verdicts are
unknown.

Statements that do not rest on sampling are the ones backed by `quint test`:
the functional properties F1-F14, which are exhaustive over small finite
universes, and the scripted runs, each of which is one concrete execution.

## Running it

```sh
sh zeronym/spec/protocol/check.sh
```

Quint 0.33.0 is pinned (`npx --yes @informalsystems/quint@0.33.0` by default;
set `QUINT=quint` to use an installed one). The two-state properties use the
action-property syntax introduced in 0.33, so 0.32 does not typecheck the
specification. Tier 4 needs Java (21 in CI) and Apalache 0.62.1, whose jar
carries TLC and which Quint fetches into `~/.quint` on first use; without
either the tier fails.

| Tier | What | Command | Expectation |
|---|---|---|---|
| 1 | typecheck | `quint typecheck` on every file | ok |
| 2 | tests | `quint test` on the spells, the four functional test files, each scenario and trust module, each configuration | all pass |
| 3 | invariants | `quint run --invariants ... --max-samples=2000 --max-steps=40 --seed=7` ("fails" rows: 40 to 80 steps, a few with more traces) | "holds" rows hold; "fails" rows are violated |
| 3b | witnesses | `quint run --witnesses ... --invariants ...` | every witness reached at least once; no invariant violated on the way |
| 4 | hub specification | `tlc.sh hubMachine.qnt hubMachine <init> <step> <invariant>`, one row each | "holds" rows hold over every reachable state; "violated" rows are violated, by a counterexample no longer than the recorded one |
| 5 | two-state properties | `QUINT_TLC=1`, opt-in, **never run** | unknown |

Measured on the machine it was written on (Apple silicon, Quint's Rust
evaluator): 4 min 40 s wall with four rows at a time (`QUINT_JOBS=4`, the
default), about 14 minutes of CPU. It has not been timed on a CI runner.
`QUINT_SAMPLES` changes the trace count. The rarest witnesses are reached in
only 2 to 6 of the 2000 traces, so a lower count risks losing them. Five
"fails" rows have a larger count of their own, written on the row.

Tier 3 "fails" rows and tier 3b run under `step` or under one of two narrower
relations, `quietStep` (no faults, no outsiders) and `outageStep` (the indexer
is unreachable throughout). Each is a part of `step`, so a state or a violation
found under one is reachable under `step`. Uniform random choice over `step`
rarely gets a transaction as far as a block in 40 steps; the narrower relations
do. Tier 3b re-checks each configuration's guarantees on those deeper traces.

## Scope

### Protocol facts the specification rests on

| # | Fact | Source |
|---|---|---|
| S1 | Shim classifies `SendTransaction` by presence of Orchard actions; unparseable folds into "treat as migration" | `zeronym/shim/src/classify.rs:70-101`, `:246-248` |
| S2 | Shim-unparseable includes trailing bytes, which the hub's parser accepts, so "shim cannot parse" does not imply "hub computes no txid" | `zeronym/shim/src/classify.rs:269-283`, `zeronym/hub/src/queue.rs:281-289` |
| S3 | Divert arms: unreadable body fails closed; empty body INVALID_ARGUMENT; too large RESOURCE_EXHAUSTED; hub unreachable UNAVAILABLE; never the operator | `zeronym/shim/src/intercept.rs:180-283` |
| S4 | With a hub configured every `GetTransaction` goes to the hub; shim keeps no per-migration state | `zeronym/shim/src/intercept.rs:305-314`, `:58-64` |
| S5 | Lookup reply arms, in order: found/height 0/empty relayed as pending; found served only if the bytes' txid equals the query (L4), else NOT_FOUND; not-found; error fails closed | `zeronym/shim/src/intercept.rs:370-422`, `:453-466` |
| S6 | Two transports behind one enum: HTTP (verdict returned synchronously) and Nym | `zeronym/shim/src/hub.rs:277-328` |
| S7 | Nym submit is dispatch-only: success once one frame is handed over, fresh nonce per hub address, sent to every address; the ack is never awaited | `zeronym/shim/src/nym.rs:595-703` |
| S8 | Nym lookup tries addresses in turn; only a timeout moves on; fresh nonce per attempt | `zeronym/shim/src/nym.rs:708-797` |
| S9 | Correlation by nonce only; unknown nonce dropped; wrong reply kind for a known nonce ignored, waiter stays | `zeronym/shim/src/nym.rs:1040-1077`, `zeronym/hub/src/wire.rs:22-27` |
| S10 | Hub admit: tip-stale gate, then draining, too large, expiry survives next scheduled flush, payload-hash dedup, byte and entry budget. Admission never asks a node | `zeronym/hub/src/server.rs:343-392`, `:299-303`, `zeronym/hub/src/queue.rs:256-340` |
| S11 | Queue identity is `sha256(bytes)`; dedup is against resident entries only (`inner.entries.contains_key`), and a flush removes every entry (`inner.entries.drain()`); accepted entries are not put back. So bytes that were published are admitted again if resubmitted | `zeronym/hub/src/queue.rs:17-22`, `:308-310`, `:358-359`, `zeronym/hub/src/batcher.rs:389` |
| S12 | Hub lookup: queue first (found, height 0, no bytes), then indexer; unparseable entries never hit; flush window answers not-found, deliberately | `zeronym/hub/src/server.rs:403-462`, `zeronym/hub/src/queue.rs:455-475` |
| S13 | Lookup and submit to the hub are unauthenticated; the hub's Nym address is public with no ACL; the queue-hit reply discloses that a txid is queued | `zeronym/hub/src/server.rs:413-437`, `zeronym/hub/src/nym.rs:220-227` |
| S14 | Flush fires only when `cadence_height / flush_interval` exceeds the last flushed epoch; first observation adopts the epoch without flushing; shutdown flushes once more | `zeronym/hub/src/batcher.rs:316-335` |
| S15 | Flush drains everything, broadcasts, then: accepted / already-known leave; rejected dropped; retryable requeued | `zeronym/hub/src/batcher.rs:358-422`, `zeronym/hub/src/chain.rs:129-134` |
| S16 | Requeue: resident copy wins; attempts + 1; dropped if it no longer survives the next flush or attempts exceed 8; may overrun the byte budget; reports `held` / `dropped_expired` / `dropped_exhausted` | `zeronym/hub/src/queue.rs:186-199`, `:366-422`, `:96` |
| S17 | Tip is the max over answering endpoints; a regression within 10 blocks is followed; staleness stops admission only | `zeronym/hub/src/chain.rs:183-201`, `zeronym/hub/src/batcher.rs:161-205`, `:222-225` |
| S18 | Budget inequality `flush_interval + mining_margin + delivery_lag <= min_wallet_expiry` asserted at startup | `zeronym/hub/src/batcher.rs:93-118` |
| S19 | Drain closes admission before the final flush; the queue is RAM-only | `zeronym/hub/src/main.rs:142-177`, `zeronym/hub/src/queue.rs:226-243`, `zeronym/hub/src/batcher.rs:337-347` |
| S20 | Wire: four fixed-size frames; reply dispositions found / not_found / error; not_found or error with a payload is a decode error; `Draining` shares `QueueFull`'s code | `zeronym/hub/src/wire.rs:29-59`, `:278-290`, `:531-569` |
| S21 | Hub drops lookups past 64 in flight, replies older than 60 s, acks when the driver queue is full | `zeronym/hub/src/nym.rs:54`, `:75`, `:171-213`, `:285-292` |
| S22 | Indexer lookup answer is forwarded verbatim, so a zero `RawTransaction` is byte-identical to the queue-hit sentinel | `zeronym/hub/src/server.rs:445-451`, `zeronym/hub/src/chain.rs:284-290` |
| S23 | No attestation or STEVE handshake exists in code | `zeronym/README.md:88` |
| S24 | Replicate, never fail over: every hub that receives a migration queues and broadcasts it | `zeronym/README.md:86`, `zeronym/shim/src/nym.rs:602-647` |
| S26 | Shipped constants: `FLUSH_INTERVAL_BLOCKS = 20`, `MINING_MARGIN = 4`, `MAX_DELIVERY_LAG = 6`, `MIN_WALLET_EXPIRY = 40`, `REORG_ALLOWANCE = 10`. The slack `40 - (20 + 4 + 6) = 10` equals the reorg allowance exactly. `BatchParams::validate` asserts only the three-term sum; nothing asserts the four-term one | `zeronym/hub/src/batcher.rs:40-59`, `:101-113` |
| S27 | Lookup starts at a rotating cursor, so consecutive polls start at different hubs; a `NotFound` from the first hub asked is final | `zeronym/shim/src/nym.rs:756-793` |
| S28 | Indexer folds are asymmetric: tip is the max over answering endpoints (one endpoint can only win high; a low tip needs every endpoint); lookup returns the first `Found` in endpoint order (one endpoint suffices to inject an answer); publish takes the best verdict | `zeronym/hub/src/chain.rs:183-201`, `:305-319`, `:517-532` |
| S29 | Submit sweep tells the wallet ok when at least one frame was handed over, even if the loop broke before later addresses | `zeronym/shim/src/nym.rs:673-702` |
| S30 | L4 deserialises the returned bytes, computes their txid and compares it with the queried hash in both byte orders. It compares nothing else: not the bytes, not the height | `zeronym/shim/src/intercept.rs:453-466` |
| S31 | HTTP transport: one `SocketAddr`; hub answers `"accepted"` for both a fresh admission and a duplicate, so the client's `"already_known"` arm has no source; a 200 lookup without the octet-stream content type and `x-tx-height` is an error | `zeronym/shim/src/hub.rs:69-72`, `:201-208`, `:259-263`, `zeronym/hub/src/server.rs:741-747` |
| S32 | Two hub clocks. Admission and requeue use the observed height. The flush epoch uses the cadence height, which equals the observed height until no forward move has been seen for `TIP_STALE_AFTER` (15 min, 12 blocks at the nominal 75 s) and then free-runs at the nominal rate. The code comment claims the free-running clock runs ahead of the true height, "the safe direction"; nothing enforces it. Only the cadence loop (and startup) calls `observe`, and it does so before, never during, a flush | `zeronym/hub/src/batcher.rs:59-71`, `:227-247`, `:307-325`, `:414-422`, `zeronym/hub/src/main.rs:62` |
| S25 | The operator can recover a diverted transaction's txid from transparent-pool queries, so a txid can be known to an outsider before publication | `zeronym/README.md:34` |

Two comments in the implementation are quoted in `properties.qnt` next to the
definitions they justify.

- The accepted disclosure (W8), `zeronym/hub/src/server.rs`, in `Hub::lookup`:
  "What this does NOT close: the 200-versus-NotFound distinction still
  discloses that a given txid is queued here. Closing that too means answering
  NotFound, which costs a wallet the ability to tell "pending" from "never
  seen". That is a product decision, not a code one, and it is left open
  deliberately."
- The flush window (`truth`, used by G4), same file, on `Hub::lookup`: "Note
  the flush-in-flight gap: `flush()` drains the queue before `broadcast_batch`
  has reached the indexer, so a lookup in that window gets a queue miss then an
  indexer NOT_FOUND for a transaction it was told height-0 about seconds
  earlier. Wallets poll on multi-second intervals and tolerate a transient
  NOT_FOUND; a resubmit is harmless (deduped pre-flush, already-known
  post-flush). Holding entries until broadcast returns would extend how long
  the hub remembers a txid, which is the wrong trade."

### In the model

| Area | What is modelled | Why |
|---|---|---|
| Wallet / shim front door | `SendTransaction` input as `Clean(payload) \| Unreadable \| EmptyBody`; routing to divert / forward / fail-closed; `GetTransaction` always to the hub | S1, S3, S4. `divert.qnt` omits it |
| Shim / hub exchange | `Submit`, `Ack`, `Lookup`, `LookupReply` over a grow-only soup; nonce correlation; waiter kinds; lookup starting at a nondeterministic hub (the cursor, S27), timeout and failover; submit fan-out including the prefix send (S29); submit mode `DispatchOnly \| AwaitVerdict` | S6-S9. `AwaitVerdict` is the one HTTP difference this model represents (who the wallet hears from); the others are listed out of scope |
| Hub | lifecycle; admission with all five refusals; queue keyed by payload; flush cadence on tip epochs; flush window; per-entry verdicts; requeue; crash | S10-S19 |
| Chain / indexer | height; per-txid status; what the indexer has been offered; verdict and lookup-answer relations | S15, S22 |
| Wire encoding | pure `render` / `interpretReply` between hub outcome and wallet observation; frame size classes | S20, S22 |
| Trust | role `Honest \| Byzantine` for shim, hub, hub indexer | S23 |
| Third party | a client of the hubs' public, unauthenticated address: looks up txids it knows; submits payloads it has learned and payloads of its own making; its payload knowledge is derived from what it can observe | S13, S25 |
| Network | drop, duplicate, delay, reorder; cannot forge |  |
| Replication | `HUBS` is a set; one shared chain | S24 |
| Tip | `TipTimely \| TipMayRegress \| TipMayLag`, the observed tip and the cadence height as two hub clocks, `REORG_ALLOWANCE`, `STALE_WINDOW` and the wallet expiry floor as constants | S17, S26, S32 |

### Out of the model

| Item | Reason |
|---|---|
| Attestation, PCRs, TLS, STEVE, keymaker quorum | No in-protocol messages exist (S23). Represented by the roles |
| Mixnet internals: SURBs, Sphinx, cover traffic, gateways, throttling; shim client rotation supervisor (`zeronym/shim/src/nym.rs:942-1024`); both `nym_driver.rs` | Protocol-visible effect is loss and delay |
| Hub lookup concurrency bound, reply deadline, dropped acks (S21) | Refinements of "the network lost the message" |
| Wall-clock time | The staleness window is counted in blocks (`STALE_WINDOW`), and a free-running cadence height is chosen by the environment under the named assumption `freeRunNotSlowerThanChain`; there is no clock |
| Multiple indexer endpoints and their folds | One abstract indexer per model stands for all of a hub's endpoints. Because the folds are asymmetric (S28), this document states for each Byzantine-indexer behaviour whether one lying endpoint suffices or all must lie |
| Wire codecs `ZNS1` / `ZNA1` / `ZNL1` / `ZNR1` and the golden vectors (`zeronym/hub/src/wire.rs:576-579`) | Byte layouts are scoped out and are pinned by the Rust tests in both crates; the abstract `render` / `interpretReply` layer is the level this spec works at. The spec does not claim to bind the codec |
| HTTP `"already_known"` and the lookup content-type tripwire (S31) | Checked in code: `"already_known"` has no hub source, so the wallet can never observe it; the tripwire turns a malformed 200 into the same `Unavailable` the wallet sees for `error`. Neither is a distinct wallet observation that changes a property |
| Two or more hubs under `AwaitVerdict` | Does not exist in code: HTTP is one address (S31) and Nym never awaits the ack (S7). `awaitVerdictSingleHub` stays an assumption |
| Reorgs of included transactions, mempool eviction | Environment assumption: per-txid chain status is monotone |
| Anonymity-set size, shuffle, simultaneity, timing and length side channels | Not trace properties. Only the pure lemma "frame size is independent of content" is stated |
| Byte layout, malformed frames, `bad_frame` | Sum types make them unrepresentable; pinned by the Rust golden vectors |
| Forward-only shim, transparent-pool RPCs, health / address / attestation endpoints, DoS bounds, logging | Not divert-protocol state |
| More than one Byzantine component at once | The trust matrix is single-fault |
| A model-based test harness for the Rust | Later work; see "Model-based testing, later" |

### Assumptions

- **Roles.** The shim and the hubs run in enclaves and are honest in the
  baseline. The shim, each hub and the hubs' indexer can each be made
  Byzantine, one at a time. A Byzantine component draws its transitions from a
  wider relation than the honest one; no message, state field or observation
  records which it drew.
- **Network.** May lose, duplicate, delay and reorder frames. Cannot forge or
  read them.
- **Third party.** A client of the hubs' public address. It looks up txids it
  knows and submits payloads it has learned or made. It cannot read or forge
  frames, so it does not know a nonce and cannot answer the shim.
- **Nonces** are unique. A counter stands for an unguessable value.
- **Chain.** A transaction's status only moves forward: no reorg of an included
  transaction, no mempool eviction. The operator's indexer publishes nothing.
- **Flight time.** At most `MAX_FLIGHT_BLOCKS` blocks arrive while one flush
  is in flight, and that is fewer than the mining margin
  (`flightWithinMargin`). The implementation bounds each call to the indexer
  (`RPC_TIMEOUT`, `hub/src/chain.rs`), not the batch, and neither in blocks:
  the code does not enforce this. A hub whose flush is in flight does not look
  at the tip.
- **Tip.** In every model a due flush has begun before the next block.
  `TipTimely`: every running, idle hub asks for the tip at each block. An
  honest indexer answers with the true height; a Byzantine one is asked just
  as often and controls only the answer. `TipMayRegress`: a tip
  report may trail the chain by up to `REORG_ALLOWANCE`. `TipMayLag`: a hub may
  hear nothing for a while, and is stale once the silence reaches
  `STALE_WINDOW` blocks; a stale hub's free-running clock is assumed never
  behind the chain (`freeRunNotSlowerThanChain`) and at most one flush interval
  ahead of it.
- **Wallets.** A supported ("conforming") wallet sets an expiry at least
  `MIN_WALLET_EXPIRY` after the height it builds at, and its frame reaches a
  hub within `DELIVERY_LAG` blocks. A wallet asks only about transactions it
  has sent.
- **Honest indexer.** Answers lookups from chain state or "unavailable". A
  broadcast may always be rejected or left unjudged; it is accepted only if a
  node would take it, and reported already-known only if the chain has it.
- **Time.** There is no clock. A timeout may happen at any moment; the
  staleness window is counted in blocks.

## State machines

### Shim: `SendTransaction` routing

```mermaid
stateDiagram-v2
    [*] --> Inspect
    Inspect --> Forwarded: Clean and class PassThrough
    Inspect --> FailClosed: Unreadable or EmptyBody
    Inspect --> Framing: Clean and class OrchardTouching or Unparseable
    Framing --> FailClosed: oversize
    Framing --> Dispatched: DispatchOnly, frames to a non-empty prefix of the hubs, fresh nonce each
    Framing --> FailClosed: DispatchOnly, no frame handed over
    Framing --> AwaitingAck: AwaitVerdict
    Dispatched --> ToldOk
    AwaitingAck --> ToldOk: Ack accepted
    AwaitingAck --> ToldRejected: Ack refused
    AwaitingAck --> FailClosed: timeout
    Forwarded --> [*]
    ToldOk --> [*]
    ToldRejected --> [*]
    FailClosed --> [*]
```

### Shim: lookup request lifecycle (one `GetTransaction`)

```mermaid
stateDiagram-v2
    [*] --> Awaiting: send Lookup to the hub the cursor points at, fresh nonce
    Awaiting --> Awaiting: timeout and hubs remain, fresh nonce to next hub
    Awaiting --> Awaiting: wrong-kind or unknown-nonce frame ignored
    Awaiting --> Pending: reply found, height 0, no body
    Awaiting --> Tx: reply found, body txid equals query
    Awaiting --> NotFound: reply not_found, or found that fails L4
    Awaiting --> Unavailable: reply error, or timeout on last hub
    Pending --> [*]
    Tx --> [*]
    NotFound --> [*]
    Unavailable --> [*]
```

### Hub: lifecycle

```mermaid
stateDiagram-v2
    [*] --> Down
    Down --> Starting: restart, queue empty, no tip, no epoch
    Starting --> Running: first tip observed, epoch adopted without a flush
    Running --> Stale: no forward tip progress
    Stale --> Running: tip advances
    Running --> Draining: shutdown signal
    Stale --> Draining: shutdown signal
    Draining --> Stopped: final flush done, leftovers lost
    Starting --> Down: crash
    Running --> Down: crash, queue and in-flight batch lost
    Stale --> Down: crash, queue and in-flight batch lost
    Draining --> Down: crash
    Stopped --> Down
```

### Hub: flush cycle

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Broadcasting: tip epoch exceeds last flushed epoch, or draining; whole queue moves in flight
    Idle --> Idle: nothing queued, epoch recorded
    Broadcasting --> Broadcasting: indexer returns one entry's verdict
    Broadcasting --> Idle: all verdicts in; requeue retryable entries; record epoch
```

### Hub: per-payload entry lifecycle

```mermaid
stateDiagram-v2
    [*] --> Absent
    Absent --> Refused: admit fails (TipStale, Draining, TooLarge, ExpiryTooTight, Full)
    Refused --> Absent
    Absent --> Queued: admit
    Queued --> Queued: same bytes again (duplicate)
    Queued --> InFlight: flush begins
    InFlight --> Published: verdict Accepted or AlreadyKnown
    InFlight --> Rejected: verdict Rejected
    InFlight --> Queued: Retryable, still survives next flush, attempts within bound, no resident copy
    InFlight --> DroppedExpired: Retryable, no longer survives next flush
    InFlight --> DroppedExhausted: Retryable, attempts over bound
    Queued --> Lost: crash
    InFlight --> Lost: crash
    Published --> Absent
    Rejected --> Absent
    DroppedExpired --> Absent
    DroppedExhausted --> Absent
    Lost --> Absent
```

`Absent --> Queued` is reachable again after `Published` (S11).

### Chain: per-txid status (environment)

```mermaid
stateDiagram-v2
    [*] --> Absent
    Absent --> Mempool: a broadcast is accepted
    Mempool --> Mined: included at a height
    Mined --> [*]
```

### Network: one message in the soup

```mermaid
stateDiagram-v2
    [*] --> Sent: added to the soup, never removed
    Sent --> Sent: delivered to its destination (any number of times, any order)
    Sent --> [*]: never delivered (loss)
```

### Third party: knowledge of one transaction

```mermaid
stateDiagram-v2
    [*] --> Nothing
    Nothing --> KnowsTxid: learns a txid out of band (S25)
    KnowsTxid --> KnowsQueued: Lookup answered found, height 0, no body
    KnowsTxid --> KnowsPayload: payload published on chain
    KnowsQueued --> KnowsPayload: payload published on chain
    Nothing --> KnowsPayload: payload published on chain
    KnowsPayload --> KnowsPayload: may resubmit the payload to any hub
    Nothing --> Nothing: may submit payloads of its own making to any hub
```

### Encoding: hub outcome to wallet observation

```mermaid
flowchart LR
    QH[QueueHit] -->|render| S["found, height 0, no body"]
    IZ["indexer answers found, height 0, no body; an honest hub forwards it unchanged"] -->|render| S
    IF["indexer: found p at h"] -->|render| F["found, h, p"]
    INF[indexer: not found] -->|render| NF[not_found]
    IU[indexer: unavailable] -->|render| E[error]
    S -->|interpretReply q| P[Pending]
    F -->|"interpretReply q, txid(p) = q"| T[Tx p]
    F -->|"interpretReply q, txid(p) != q"| N[NotFound]
    NF -->|interpretReply q| N
    E -->|interpretReply q| U[Unavailable]
```

The zero-body indexer answer is not something the honest indexer relation produces, but the honest hub and honest shim pass it through (S22), and one endpoint out of several is enough to inject it (S28). It is therefore reachable only in `byzIndexer`, where "Byzantine indexer" includes "one misbehaving endpoint".

The encoding is not injective at that point, and that is by design: the pending
sentinel has no bytes to tell it apart by. The shim reads both as pending, for
every query (F2), so a wallet cannot tell "queued at the hub" from "an indexer
said found and returned nothing". `meaning` gives the two different meanings;
`interpretReply` after `render` gives one observation. `indexerForgesPendingTest`
is the trace-level consequence: the wallet is told pending for a transaction
nobody holds, and G4 fails.

## Layout

Only `protocol.qnt` declares a constant or a variable. Every other module is
pure.

| File | Module | Owns |
|---|---|---|
| `spells/basicSpells.qnt` | `basicSpells` | `Option`, and a few set and map helpers, each with its test |
| `spells/soup.qnt` | `soup` | The message soup: `Envelope[p, m]`, `Soup[p, m]`, `send`, `sendAll`, `inbox`, `outbox` |
| `types.qnt` | `types` | The vocabulary: payloads, verdicts, refusals, roles, observations, `Result[s, o]`, `Config` |
| `wire.qnt` | `wire` | The four frames; `render`, `renderAck`, `meaning`, `interpretReply`, `sizeOf` |
| `indexer.qnt` | `indexer` | The chain and indexer as a relation: honest and Byzantine outputs, and their effect |
| `hub.qnt` | `hub` | `hub(state, input)`; admission, the tip rule, the flush cycle, requeue; `byzHubResults` |
| `shim.qnt` | `shim` | `shim(state, input)`; routing, the lookup sweep, reply correlation; `byzShimResults` |
| `state.qnt` | `state` | `System`, `Label`, `Audit`; where each output goes; the derived views |
| `properties.qnt` | `properties` | `truth` and the audit monitor `advance`; guarantees, gaps, witnesses |
| `protocol.qnt` | `protocol` | The constant, the assumptions, the variables, `commit`, the steps, the property aliases, A1-A3, the run vocabulary |
| `instances.qnt` | `configs`, then one module per configuration | The fifteen configurations |
| `tests/wireTest.qnt`, `indexerTest.qnt`, `hubTest.qnt`, `shimTest.qnt` | | F1-F14 |
| `tests/scenariosTest.qnt` | one module per configuration used | Witnesses and pinned gap causes |
| `tests/trustTest.qnt` | one module per Byzantine configuration | One run and one control per "required" cell |

```mermaid
flowchart BT
    soup --> basicSpells
    types --> basicSpells
    wire --> types
    indexer --> types
    hub --> types
    shim --> wire
    state --> soup
    state --> wire
    state --> indexer
    state --> hub
    state --> shim
    properties --> state
    protocol --> properties
    instances --> protocol
    tests --> instances
    tests --> properties
```

### Components as functions

Each component is one total function from its state and one input to its next
state and one output. An input that is invalid in the current state returns an
error output and leaves the state alone. The state machine holds no protocol
logic: a step picks an input, calls the function, and puts the output where it
goes.

| Component | Inputs | Outputs | Seam in the implementation |
|---|---|---|---|
| `hub` | `SubmitHInput`, `LookupHInput` (with the indexer's answer), `TipHInput(height)`, `StaleHInput(estimate)`, `FlushDueHInput`, `VerdictHInput`, `FlushDoneHInput`, `DrainHInput`, `CrashHInput`, `RestartHInput` | `AckOutput`, `LookupReplyOutput`, `BroadcastOutput`, `RequeuedOutput`, `NoHubOutput`, `HubErrorOutput` | `Hub::admit`, `Hub::lookup` (`hub/src/server.rs`), `run_listener` (`hub/src/nym.rs`), `TipTracker::observe`, `cadence_height`, `flush` (`hub/src/batcher.rs`), `Queue::requeue`, `Queue::begin_draining` (`hub/src/queue.rs`) |
| `shim` | `SendTxSInput` (with how many hub addresses take a frame), `GetTxSInput` (with where the cursor points), `FrameSInput`, `LookupTimeoutSInput`, `AckTimeoutSInput` | `ForwardOutput`, `DivertedOutput`, `SendDoneOutput`, `LookupSentOutput`, `LookupDoneOutput`, `NoShimOutput`, `ShimErrorOutput` | `send_transaction`, `divert`, `get_transaction` (`shim/src/intercept.rs`), `NymHandle::submit`, `get_transaction`, `deliver` (`shim/src/nym.rs`) |
| indexer | `BroadcastIInput`, `LookupIInput`, `AdvanceIInput`, `MineIInput` | `VerdictOutput`, `AnswerOutput`, `NoIndexerOutput` | the mock indexer in `hub/tests/common/mod.rs` |

### Roles

`ROLES` gives the shim, each hub and the indexer a role. An honest component
takes exactly the transition its function gives. A Byzantine one takes any
member of a finite set that contains it (F12):

- **Byzantine hub.** Any ack for a submission it receives, with the payload
  queued or not, whatever admission says. Any reply to a lookup: a queue hit,
  not found, error, or found with no body or any payload that exists, at any
  height. It keeps the honest flush schedule.
- **Byzantine shim.** Any answer to the wallet for a send or a lookup. Any
  transaction handed to the operator. A frame carrying any payload to any hub.
- **Byzantine indexer.** Any verdict, with the transaction relayed to the
  network or not. Any lookup answer built from a payload it was offered, one
  the chain published, or a twin of either. Any tip up to `MAX_HEIGHT`.
- Any of them may disclose a payload it has seen (`byzDisclose`).

A hub folds several indexer endpoints into one answer, and the folds are not
symmetric: the tip is the maximum over endpoints, a lookup takes the first
"found", a broadcast takes the best verdict. So **one** misbehaving endpoint is
enough to raise the tip, inject a lookup answer or change a verdict, while
lowering or freezing the tip takes **every** endpoint. Each indexer cell below
says which it needs. That is prose about the abstraction: the model has one
abstract indexer, standing for the fold over all endpoints, and **does not
enforce** the difference. Its Byzantine relation can report any tip, high or
low. Modelling the endpoints as a set, so that the transition relation itself
separates "one endpoint" from "all of them", was considered and not done: one
abstract indexer per hub was a decision of the design.

### Configurations

One constant, `CONFIG`, holds a configuration; `protocol.qnt` names its fields
(`PAYLOADS`, `FLUSH_INTERVAL`, `ROLES`, `TIP`, ...).

| Module | Hubs | Submit mode | Roles (shim / hubs / indexer) | Tip |
|---|---|---|---|---|
| `baseline` | 1 | `DispatchOnly` | H / H / H | timely |
| `byzShim` | 1 | `DispatchOnly` | **B** / H / H | timely |
| `byzHub` | 1 | `DispatchOnly` | H / **B** / H | timely |
| `byzIndexer` | 1 | `DispatchOnly` | H / H / **B** | timely for honest reports |
| `awaitAck` | 1 | `AwaitVerdict` | H / H / H | timely |
| `awaitAckByzShim` | 1 | `AwaitVerdict` | **B** / H / H | timely |
| `awaitAckByzHub` | 1 | `AwaitVerdict` | H / **B** / H | timely |
| `awaitAckByzIndexer` | 1 | `AwaitVerdict` | H / H / **B** | timely |
| `replicated` | 2 | `DispatchOnly` | H / H, H / H | timely |
| `replicatedOneByz` | 2 | `DispatchOnly` | H / H, **B** / H | timely |

The schedule is the shipped one scaled down, keeping the relations between the
numbers:

| | Interval | Margin | Delivery lag | Reorg allowance | Staleness window | Expiry floor |
|---|---|---|---|---|---|---|
| Shipped | 20 | 4 | 6 | 10 | 12 blocks (15 min at 75 s) | 40 |
| Model | 3 | 2 | 1 | 1 | 3 | 7 |

The relations kept, each asserted by `assumptionsTest` through
`shippedRelationsKept`:

- the slack `floor - (interval + margin + lag)` equals the reorg allowance
  (10 and 1);
- the staleness window exceeds the margin (12 > 4, 3 > 2) and the slack
  (12 > 10, 3 > 1);
- `interval + margin + lag + (window - 1)` exceeds the floor by exactly one
  (41 = 40 + 1, 8 = 7 + 1);
- `interval + lag + (window - 1)` does not exceed it (37 <= 40, 6 <= 7).

The margin is 2, not 1, so that one block can arrive while a flush is in flight
and still be inside it: `MAX_FLIGHT_BLOCKS` is 1 everywhere except
`flakyTipSlowFlight`, where it is 2. `flakyTipNoSlack` uses a floor of 6 and
`staleLagWithSlack` a floor of 8; those two do not keep the relations, and
their tests assert that. Also: at most 2 requeues, room for 2 entries,
heights up to 12, at most 3 sends and 3 lookups by the wallet and 3 requests by
the third party.

Each configuration has an `assumptionsTest`. The simulator does not enforce
`assume`, so that test is the check that counts. Every `assume` in
`protocol.qnt` is true of every configuration. `reorgSlackFits` and
`flightWithinMargin` are assumed only where the configuration says it relies on
them (`reliesOnReorgSlack`, `reliesOnFlightWithinMargin`); `flakyTipNoSlack`
and `flakyTipSlowFlight` each drop one, and their tests assert it is false.
`staleSlackFits` is never assumed.

## Properties

### Functional properties (`quint test`, exhaustive over small universes)

| Id | Statement | Test |
|---|---|---|
| F1 | `interpretReply(render(o), q) == meaning(o, q)` for every outcome a queue or an honest indexer produces | `wireTest::renderThenInterpretIsMeaningTest` |
| F2 | The documented collision: a queue hit and an indexer's "found, height 0, no body" render to the same reply, and the shim reads both as pending for every query. `render` is injective on honest outcomes | `wireTest::sentinelCollisionTest` |
| F3 | The shim serves a transaction only if its txid is the one asked for. A twin is served; the height is passed through unchecked | `wireTest::servedOnlyOnMatchingTxidTest` |
| F4 | An error never becomes "not found" | `wireTest::errorIsNeverNotFoundTest` |
| F5 | The shim forwards only cleanly read pass-through transactions | `shimTest::onlyPassThroughIsForwardedTest` |
| F6 | A frame's size depends on its kind only | `wireTest::sizeIsIndependentOfContentTest` |
| F7 | Under the startup budget, a conforming payload arriving within the delivery lag passes the expiry check. This is about admission at one tip, not about when the flush happens | `hubTest::conformingTimelyPayloadIsAdmissibleTest` |
| F8 | The admission decision table, in the implementation's order | `hubTest::admissionDecisionTableTest` |
| F9 | Requeue, entry by entry, and the counts it reports | `hubTest::requeueTest` |
| F10 | Draining and full are one refusal on the wire | `wireTest::ackRenderingTest` |
| F11 | `hub` and `shim` are total; an invalid input returns an error and changes nothing | `hubTest::totalityTest`, `shimTest::totalityTest` |
| F12 | Each Byzantine relation contains the honest transition | `byzantineContainsHonestTest` in `hubTest`, `shimTest`, `indexerTest` |
| F13 | An accepted ack is given only for a payload the hub then holds; a Byzantine hub can do otherwise | `hubTest::ackImpliesQueuedTest`, `hubTest::byzantineHubTest` |
| F14 | The tip rule: first observation adopted; forward followed; a drop within the allowance followed; a larger drop ignored | `hubTest::tipRuleTest` |

### Guarantees

| Id | Name | What it says |
|---|---|---|
| G1 | `operatorBlind` | Everything the shim hands the operator is a pass-through transaction |
| G2 | `queuedBytesConfidential` | Everything the third party has learned is on the chain, or was a pass-through transaction given to the operator. Its knowledge is derived from the replies sent to it, the operator's view and explicit disclosures; nothing updates it at publication |
| G3 | `txidAuthenticity` | A transaction served to the wallet has the txid asked for. It need not be the bytes the wallet sent, and its height is whatever the hub said |
| G4 | `lookupValidityPerHub` | Every lookup answer other than "unavailable" was true at the hub that gave it at some point between request and answer. Not-found during the flush window counts as true. It does not say that successive answers agree, or that hubs agree |
| G5 | `toldImpliesQueued` | A wallet told ok can rely on some hub having queued the transaction. Claimed under `AwaitVerdict` only |
| G6a | `offeredBeforeExpiry` | Every transaction a hub offers is offered with the mining margin to spare: whatever was admitted, on every attempt. About the margin left when the flush begins, not about acceptance. Claimed under a timely tip |
| G6b | `conformingFirstOfferBeforeExpiry` | The same for supported wallets and for the first time a hub offers the transaction. Nothing about a later offer of a requeued entry. Also about the margin at the offer |
| G6c | `conformingFirstOfferJudgedBeforeExpiry` | End to end: when a node judges the first offer of a supported wallet's transaction, it has not expired. Needs G6b and `flightWithinMargin` |
| G7 | `wellFormed` | Structural sanity; checked in every configuration; not a trust-matrix row |
| G8 | `ackImpliesQueued` | An accepted ack from a hub is for a payload that hub had queued by then, whether or not anyone waits for the ack |

No guarantee reads a field written by the function it constrains. The history
the guarantees need (`audit`) is derived by `commit` from the state before and
the state after each step.

Each guarantee can be broken by a change to an honest component. These were
tried by hand, with the result shown, and reverted:

| Guarantee | Change | Result |
|---|---|---|
| F1 | `interpretReply` loses the pending arm | F1 fails |
| G1 | `shim` forwards an unparseable body | violated on `baseline` |
| G2 | `hub` answers a queue hit with the queued body | violated on `baseline` |
| G3 | `interpretReply` skips the txid comparison | **holds on `baseline`**; violated on `byzHub` and `byzIndexer` |
| G4 | `hub` answers not-found on a queue hit | violated on `baseline` and `replicated` |
| G5, G8 | `hub` acks accepted without inserting | G5 violated on `awaitAck`, G8 on `baseline` |
| G6a | `hub` admits without the expiry check | violated on `baseline` |

The G3 row is not what was predicted; see [Findings](#findings).

### Trust matrix

Which components must be honest for each guarantee. Single-fault. "holds" is a
tier 3 simulation row on the named configuration, with its antecedent witnessed
there in tier 3b. "required" is a scripted run in `tests/trustTest.qnt` in which
the component is Byzantine and the guarantee fails, followed by its control
(same wallet inputs, honest transition, guarantee holds); the simulation row
for such a cell shows polarity only.

Every cell was a prediction, except the G6c row, which was added after review
and derived by running. **Observed verdicts agree with the predictions in every
cell of this table except the G6b entries marked below.**

The two tip-withholding runs in the indexer column have the hub ask for the tip
at every block and the indexer answer with a stale one; their controls are the
same polls answered truthfully. The simulation rows for those cells classify a
verdict line and cannot say which lie a trace used. The counterexamples the
simulator finds at seed 7 were read by hand and both use reports below the true
height. With truthful answers `byzIndexer` behaves as `baseline`, where the
chain cannot pass a running, idle hub that has not asked.

| | All honest | Byzantine shim | Byzantine hub | Byzantine indexer |
|---|---|---|---|---|
| G1 | holds (`baseline`) | **required**: `operatorSeesMigrationTest` | holds (`byzHub`) | holds (`byzIndexer`) |
| G2 | holds (`baseline`) | **required**: `shimDisclosesPlaintextTest` | **required**: `hubServesQueuedBodyTest` | **required**: `indexerServesUnpublishedBodyTest`. One endpoint suffices |
| G3 | holds (`baseline`) | **required**: `shimServesAnotherTransactionTest` | holds (`byzHub`); a twin and a false height are both served (W16) | holds (`byzIndexer`) |
| G4 | holds (`baseline`, `replicated`) | **required**: `shimInventsStatusTest` | **required**: `hubDeniesQueuedTest`, `hubServesFalseHeightTest` | **required**: `indexerForgesPendingTest`. One endpoint suffices |
| G5 | holds (`awaitAck`) | **required**: `toldOkWithoutSendingTest` | **required**: `toldOkOnAFalseAckTest` | holds (`awaitAckByzIndexer`) |
| G8 | holds (`baseline`, `awaitAck`) | holds (`byzShim`) | **required**: `hubAcksWithoutAdmittingTest` | holds (`byzIndexer`) |
| G6a | holds (`baseline`) | holds (`byzShim`) | **required**: `hubAdmitsPastExpiryRuleTest` | **required**: `indexerWithholdsTipTest`. Needs every endpoint |
| G6b | holds (`baseline`, `flakyTip`). **Fails on `staleLag` (K4, predicted) and on `staleLagWithSlack` (predicted to hold)** | holds (`byzShim`) | **required**: `hubAdmitsBeforeFirstTipTest`. The cause differs from the one predicted | **required**: `indexerWithholdsTipFromConformingTest`. Needs every endpoint |
| G6c | holds (`baseline`, `flakyTip`). Fails on `staleLag` (K4), `flakyTipNoSlack` (K3'), `flakyTipSlowFlight` (K7), and by scripted run on `staleLagWithSlack` | holds (`byzShim`) | **required**: `hubAdmitsBeforeFirstTipTest` | **required**: `indexerWithholdsTipFromConformingTest`. Needs every endpoint |
| A3 | not run (TLC, `baseline`) | not run | **required**: `hubAdmitsWhileDrainingTest` | not run |

One Byzantine replica out of two (`replicatedOneByz`):

| Property | Observed | Backing |
|---|---|---|
| G2 | required of every hub | `oneReplicaServesQueuedBodyTest` |
| G4 | required of every hub | `cursorLandsOnLyingReplicaTest` |
| G3 | holds | simulation; `wrongTransactionIsRefusedTest` |
| G8, G6a, G6b, G6c for the honest hub | hold | simulation of `ackImpliesQueuedForHonestHubs`, `offeredBeforeExpiryForHonestHubs`, `conformingFirstOfferBeforeExpiryForHonestHubs`, `conformingFirstOfferJudgedBeforeExpiryForHonestHubs`; `honestReplicaKeepsItsGuaranteesTest` |
| G5 | not applicable | `AwaitVerdict` has one hub |
| "some honest hub queued it" after told ok | not a guarantee | `honestReplicaKeepsItsGuaranteesTest` |

The `...ForHonestHubs` names are the same predicates restricted to the hubs
whose role is honest. They are not weaker properties.

In short: a Byzantine shim voids every wallet-facing guarantee (G1-G5); the
hub-side G6 and G8 survive it. G3 is the only wallet-facing guarantee that
survives a Byzantine hub or indexer, and it authenticates the txid only. G1
depends on the shim alone. Replication does not dilute trust: one Byzantine
replica is enough to void G2 and G4. A3 needs the hub: its "required"
cell is a scripted step, and its "holds" cells are the unrun TLC property.

### Known gaps, with every component honest

| Id | What is lost | Where | Form | Observed | Scripted runs |
|---|---|---|---|---|---|
| K1 | Under `DispatchOnly`, told ok does not mean any hub ever admits it | `baseline`, `replicated` | reachable states `wToldRefusedEverywhere`, `wToldNeverDelivered`, `wToldPrefixOnly` | reached | `toldOkThenRefusedTest`, `toldOkAndNeverDeliveredTest`, `toldOkAfterPrefixSendTest` |
| K2 | `statusNeverRegresses`: what a wallet sees of one transaction never goes backwards | `baseline`, `replicated` | violated invariant | violated | `repliesReorderedTest`, `walletResendsPublishedTest`, `thirdPartyResubmitsPublishedTest`, `flushWindowTest`, `rejectedAtFlushTest`, `hubsDisagreeTest` |
| K3 | G6a for a tight-expiry transaction: admitted against a tip reported below a boundary already flushed | `flakyTip` | violated invariant | violated, as predicted | `tightExpiryAdmittedBehindFlushedBoundaryTest` |
| K3' | G6b, and with it G6c, when the expiry floor equals the three-term budget | `flakyTipNoSlack` | violated invariant | violated, as predicted | `conformingMissesMarginWithoutSlackTest`; contrast `conformingSurvivesRegressionTest` |
| K4 | G6a, and G6b and G6c on the shipped relation, across a silence shorter than the staleness window | `staleLag` | violated invariant | violated, as predicted; the node then cannot accept | `silenceAcrossBoundaryMissesMarginTest`; contrast `sameSilenceWithSlackKeepsMarginTest` |
| K5 | `ackedIsHeldOrSettled`: an acknowledged payload is still held by the hub, or is on the chain, or a node judged it (accepted, already known, rejected) | `baseline`, `awaitAck` | violated invariant | violated, by a crash, by a final flush nothing judged, and by a requeue that drops the entry as expired | `ackedThenCrashedTest`, `toldOkAdmittedThenLostTest`, `ackedThenLostAtDrainTest`, `requeueAndDropTest` |
| K6 | `conformingEveryOfferBeforeExpiry`: G6b without "first offer" | `staleLag` | violated invariant | violated, as predicted | `requeuedPastExpiryTest`; control `requeueUnderTimelyTipDropsTest` |

| K7 | G6c when a flush may stay in flight for as many blocks as the mining margin | `flakyTipSlowFlight` | violated invariant | violated; G6b holds there | `slowFlightSpendsTheMarginTest`; contrast `conformingSurvivesRegressionTest` |
| K8 | A supported wallet's transaction, acknowledged on time, then lost to a crash and resent, is first offered by the restarted hub with less than the mining margin. G6b and G6c do not cover it: to the restarted hub the resend is a late first arrival | `flakyTip` (hub specification) | scripted run | shown; not a TLC row | `crashThenLateDuplicateTest`; control `lateDuplicateWithoutCrashTest` |

K7 was added after review. The four-term budget (`reorgSlackFits`) holds with
equality in the shipped constants, so a transaction that uses all of it is
offered with exactly the mining margin left. The margin is then the only thing
that pays for blocks arriving while the batch is in flight, and nothing in the
code bounds a flight in blocks.

K1 is not stated as a violated invariant because the invariant is false on the
ordinary success path too: under `DispatchOnly` the wallet is told ok before
any hub has the frame. In `toldOkAndNeverDeliveredTest` the run ends with the
frame undelivered, and nothing obliges the network ever to deliver it.

### Witnesses

Each has a scripted run and is counted in tier 3b.

| Id | Witness | Name | Configuration |
|---|---|---|---|
| W1-W3 | the wallet sees pending; its transaction in the mempool; mined | `wPending`, `wTxInMempool`, `wTxMined` | `baseline` |
| W4 | each of the five refusals | `wRefusedTipStale`, `wRefusedDraining`, `wRefusedTooLarge`, `wRefusedExpiryTooTight`, `wRefusedFull` | `baseline` |
| W5-W7 | an entry is requeued; dropped as expired; dropped as exhausted | `wRequeued`, `wDroppedExpired`, `wDroppedExhausted` | `baseline` |
| W8 | **Accepted disclosure**: a third party that knows a txid learns it is queued. The hub withholds the bytes, not the fact. See the quoted comment under [Scope](#scope) | `wQueuedDisclosed` | `baseline` |
| W9 | a queued payload the hub cannot parse is asked for and missed | `wUnparseableMissed` | `baseline` |
| W12 | a queue holds more than its capacity after a requeue | `wQueueOverCapacity` | `baseline` |
| W13 | a lookup moves on after a timeout and the next hub answers | `wFailoverAnswered` | `replicated` |
| W14 | two hubs publish the same payload in their own flushes | `wPublishedByTwoHubs` | `replicated` |
| W15 | **Premature flush**: a Byzantine indexer reports a tip ahead of the chain and the hub flushes before the true boundary. A batching harm, not a G6 one. One endpoint suffices | `wPrematureFlush` | `byzIndexer` |
| W16 | **Twin served**: the wallet is served a twin of what it sent, and a transaction at a false height; G3 holds throughout | `wTwinServed`, `wFalseHeightServed` | `byzHub` |
| W17 | the third party's own payload is queued | `wThirdPartyPayloadQueued` | `baseline` |
| W18 | **Early flush by the free-running clock**: a stale hub's clock is ahead of the chain and it flushes before the true boundary, every component honest | `wEarlyFreeRunFlush` | `staleLag` |

Non-vacuity: for each guarantee, a state where its antecedent holds, reached on
every configuration where the guarantee is claimed: `vOperatorBlind`,
`vQueuedBytesConfidential`, `vTxidAuthenticity`, `vLookupValidityPerHub` (the
log has a pending, a served transaction and a not-found), `vToldImpliesQueued`,
`vOfferedBeforeExpiry`, `vConformingFirstOfferBeforeExpiry`,
`vConformingFirstOfferJudged`, `vAckImpliesQueued`,
and on `flakyTip` also `vConformingOfferAdmittedBehind` (a conforming first
offer of a payload admitted while the hub's tip was behind the chain).

### Two-state properties: not checked

Written in `protocol.qnt` as `temporal` definitions in the 0.33 action-property
form, and typechecked. **None has been run.**

| Id | Name | What it says | Class |
|---|---|---|---|
| A1 | `chainMonotone` | A transaction's chain status never moves backwards | assumption about the environment |
| A2 | `neverEvict` | An entry leaves a hub's queue only into a flush, or because the hub went down | guarantee |
| A3 | `drainIsFinal` | A draining honest hub's queue gains only what a flush hands back | guarantee |

A3 is stated over the honest hubs only. Draining is an admission rule, and a
Byzantine hub is not bound by admission rules: `hubAdmitsWhileDrainingTest`
takes a submission into the queue after the drain began, and its control
refuses the same frame. That run asserts the step, because the simulator
does not check `temporal` definitions. A2 is stated over every hub: the
Byzantine hub relation only ever adds to a queue.

No liveness property is claimed: the network may lose everything, and under
`DispatchOnly` nobody waits for an ack.

## Findings

These are what the model showed that the predictions did not, or showed about
the code. Nothing here has been fixed, and no property or role relation was
changed to make a prediction come out.

**1. K4, confirmed: a short tip silence costs a supported wallet its mining
margin, on the shipped relation between the constants.** In
`silenceAcrossBoundaryMissesMarginTest`: a transaction built at height 2 with
expiry 9 (the floor) is admitted at 3. The hub last sees the tip at 5, one
block short of the flush at 6. Its cadence follows the tip it last saw, so
nothing is flushed until it goes stale at 8. The transaction is offered at
height 8 with one block of margin where two are reserved (`9 < 8 + 2`). One
block then arrives while the batch is in flight, which the model permits and
the margin exists to pay for, and at height 9 no honest node can accept it:
G6b and G6c both fail. With the shipped numbers the same shape gives a first
offer at `created + 6 + 20 + 11 = created + 37` against an expiry of
`created + 40`: three blocks of margin where four are reserved.
`staleSlackFits` (`interval + margin + lag + window - 1 <= floor`) is false of
the shipped constants (41 > 40). The one-block figure depends on reading 15
minutes as exactly 12 blocks; blocks are not that regular, so the real
shortfall is sometimes larger.

**2. The relation that fixes K4 does not give G6b: an early free-running flush
spends the next epoch.** This was predicted to hold on `staleLagWithSlack` and
does not. In `earlyFlushSpendsTheNextEpochTest`: a stale hub's free-running
clock reads 6 at true height 4, so the flush scheduled for 6 runs then, with
nothing to publish, and its epoch is recorded as done. The hub then sees the
tip again, at 5. Admission knows the tip and not the schedule's history: it
admits a transaction counting on the flush at 6. That flush has already
happened; the cadence loop flushes only when the epoch exceeds the last one it
recorded. The transaction waits for the flush at 9, and a further silence of
two blocks, short of the staleness window, makes that one late: it is offered
at 11 with expiry 12, half its margin gone, and after one block in flight the
node cannot accept it. By the arithmetic of that run each half alone is
harmless at these numbers; simulation finds the combination about once in a few
thousand traces of 60 steps. The implementation's comment calls a free-running
clock that runs ahead "the safe direction". It is safe for what is in the queue
when it runs. It is not safe for what is admitted after the tip returns, while
the chain is still behind an epoch the clock has already spent.

The model lets the free-running clock be at most one flush interval ahead of
the chain, so it can spend one epoch and no more. `cadence_height`
(`hub/src/batcher.rs`) adds elapsed time over the nominal block time with no
cap. Reading that code, a clock further ahead would record a later epoch and
skip more than one boundary. **That is a reading of the code. The model does
not exhibit it and no run here shows it.**

**3. K5: an acknowledged payload can be lost three ways, all with every
component honest.** The invariant first written, "held or offered", counted an
offer at the start of a flush as settling the payload, and so was not violated
by a final flush that nothing judged. Review pointed that out. It is restated
as `ackedIsHeldOrSettled`: held by the hub, or on the chain, or judged by a
node. That is violated by a crash (`ackedThenCrashedTest`), by a draining hub's
final flush that finds the indexer unreachable (`ackedThenLostAtDrainTest`),
and by a requeue that drops an entry as expired after an outage
(`requeueAndDropTest`). The third was not predicted; simulation found it.

**4. G3 does not depend on the shim's txid check when every component is
honest.** Removing the comparison from `interpretReply` leaves G3 holding on
`baseline`, because an honest hub and indexer never return another
transaction. It fails on `byzHub` and `byzIndexer`, which is where the check is
claimed to matter. G3 is kept as a guarantee: it is falsifiable where it is
claimed "by the check", and other changes to honest code would break it on
`baseline`.

**5. G6b and G6c need the hub, but not for the predicted reason.** A hub that admits
past the expiry rule cannot break G6b, because the expiry rule never refuses a
conforming, timely transaction (F7). What breaks it is a hub that admits while
it has no tip, when an honest hub refuses everything
(`hubAdmitsBeforeFirstTipTest`).

**7. G6 stops at the offer; G6c and K7 were added to see past it.** G6a and
G6b stamp an offer when the flush begins. The node judges later, and the chain
may have moved. With the first scaling (margin 1) the runs that showed "the
slack is exactly enough" ended one enabled block before the transaction became
unacceptable. The schedule is now scaled with a margin of 2, flight time is
bounded by `MAX_FLIGHT_BLOCKS`, and G6c is checked at the verdict. Observed:
G6c holds on `baseline`, `flakyTip` and `byzShim`, and for the honest hub of
`replicatedOneByz`; it fails wherever G6b fails, and on `flakyTipSlowFlight`
where G6b holds.

**6. The second clause of G1, "and no lookup", is not stated.** No output of
the shim function routes a lookup to the operator, so the clause would hold by
construction and could not be broken by any of the listed changes.

**9. An entry with an expiry can be dropped as exhausted.** In
`expiringEntryDroppedAsExhaustedTest` (hub specification, `staleLag`): `late`,
expiry 11, is queued by a hub whose tip stops at 5. Three flushes come back
unjudged. Each requeue judges the entry against the observed tip, as admission
does, so the next flush it knows of is still the one at 6 and the expiry rule
never gives the entry up; the attempt bound does. The implementation's requeue
has the same two checks in the same order and is passed the observed tip
(`zeronym/hub/src/queue.rs:408-415`, `zeronym/hub/src/batcher.rs:413-422`),
while `queue.rs:197` says of the exhausted count "Only reachable for a payload
with no expiry". Shown on the model; the code was read at those lines and not
run. The bound there is 8 requeues, so the shape needs nine unjudged flushes
of a hub that sees no tip throughout.

**8. "Timely" did not survive a restart; restated, G6b and G6c hold where the
tip may be reported behind the chain, and what is left is K8.** As first
written, "timely" remembered a payload's first entry into the hub's queue
forever. TLC then violated G6b and G6c on `flakyTip`, and G6b on
`flakyTipSlowFlight`, with every component honest and the reorg slack in
place; simulation had reported them holding. The trace: `early` (built at 2,
expiry 9) is admitted at 2 and the hub crashes. It restarts at height 6,
adopts that epoch without flushing, and is then told the tip is 5. A
duplicate of the same submission arrives and is admitted, because
`9 >= 6 + 2` and admission counts on the flush at 6. The hub is shut down at
8; its final flush offers the transaction with `9 < 8 + 2`.

Timeliness is now forgotten when the hub goes down (a crash, or the final
flush that stops it), as its queue is. Under that definition TLC exhausts
`flakyTip` with G6b and G6c holding, and `flakyTipSlowFlight` with G6b
holding (table below). `freshAfterRestartMeetsMarginTest` shows a restarted
hub giving a fresh arrival the whole margin with the tip one block behind
throughout, and the reachability row `wTimelyQueuedBehindEpoch` shows that a
timely payload does get queued while the cadence epoch is behind the one the
hub last recorded, so the "holds" is not vacuous there.

The three facts the trace rests on were read in the code, and the model has
each right. A restarted hub has no tip and no recorded epoch, and its first
observation adopts the current epoch without flushing
(`zeronym/hub/src/batcher.rs:316-322`). A regression within the reorg
allowance is followed (`batcher.rs:177-186`), and a cadence epoch below the
recorded one flushes nothing (`batcher.rs:323-327`). Admission computes its
deadline from the observed tip alone (`zeronym/hub/src/server.rs:351-356`,
`zeronym/hub/src/queue.rs:294`, `:507-519`). So the trace is the code's
behaviour, and what the restatement changes is only which guarantee claims
it. That behaviour is K8: the wallet did everything right, was acknowledged
on time, and its resend after the crash is offered with one block of margin
where two are reserved. Its control, `lateDuplicateWithoutCrashTest`, has no
crash: the first offer is at 3, in time, and the late offer at 8 is a second
offer (K6's ground, not G6b's).

## Model-based testing, later

Not built. The specification is shaped so it can be:

- Every branch of `step` is a named action and every choice is a named `nondet`
  inside it, so `--mbt` traces carry `mbt::actionTaken` and `mbt::nondetPicks`.
- `lastAction` records the step and the input it gave a component, in the
  state, so scripted runs carry the same information.
- Each step gives one input to one component function and applies one output;
  the pairs map onto the seams in the table above.
- All protocol state is in `s`. `audit` is a monitor a harness ignores.
- ITF variable names are qualified by configuration. Model nonces are counters,
  to be bound to real nonces as frames appear. A payload's `id` maps to a
  fixture. The frames a step emits are `s.net` after it minus before.

## The hub specification under TLC

`hubMachine.qnt` is one hub, the chain and the two things the hub asks its
indexer, built on the same `hub(state, input)` as everything else. Its
observer keeps four sets of payloads and one height and no history, which is
what lets TLC visit every reachable state. `tlc.sh FILE MAIN INIT STEP
INVARIANT` checks one invariant of one configuration and prints `holds
<distinct states> <depth>` or `violated <trace length>`; anything else,
including a run that TLC has not finished in five minutes, is a failure.

A configuration is a value held in the state and selected by a named init
(`initTimely`, ...), whose guard is the assumptions that configuration is
checked under. A guard that is false leaves no initial state, and `tlc.sh`
fails on that.

Measured on the machine this was written on (Apple silicon, 16 cores, 64 GB;
Quint 0.33.0, Apalache 0.62.1, Java 27), under the first definition of
timeliness (finding 8) and the hub function before the capacity refusals are
removed.
Every run had a five-minute limit. Times are for the whole route (compile,
export, TLC), of which compile and export are about 10 s; "peak" is the
resident size of the largest process.

Exhausting each configuration (`step`, an invariant that is true everywhere):

| Configuration | Payloads | Distinct states | Depth | 8 workers, 8 GB | 2 workers, 4 GB |
|---|---|---|---|---|---|
| `timely` | 3 | 189 297 | 44 | 17 s, 3.1 GB | 27 s, 1.9 GB |
| `flakyTip` | 3 | 1 319 986 | 44 | 67 s, 6.3 GB | 165 s, 4.4 GB |
| `flakyTipNoSlack` | 3 | 1 319 986 | 44 | 72 s, 6.3 GB | 166 s, 4.4 GB |
| `flakyTipSlowFlight` | 3 | 1 761 078 | 45 | 95 s, 6.8 GB | 229 s, 4.4 GB (122 s with 4 workers) |
| `staleLag` | 3 | not exhausted: 6 521 452 at depth 32, 679 376 on the queue | | 300 s, 8.5 GB | |
| `staleLag` | 2 (`early`, `late`) | 1 130 260 | 42 | 62 s, 6.1 GB | 139 s, 4.4 GB |
| `staleLagWithSlack` | 3 | not exhausted: 7 713 309 at depth 34, 580 992 on the queue | | 300 s, 8.6 GB | |
| `staleLagWithSlack` | 2 (`early`, `late`) | 1 131 714 | 42 | 63 s, 6.2 GB | 141 s, 4.4 GB |

The 8-worker runs were two at a time and the 2-worker runs three at a time,
on 16 cores, so each is slower than it would be alone; the two timeouts were
measured that way and were not repeated alone. The two lagging-tip
configurations are therefore checked with two payloads. What that costs: with
`tight`, TLC's counterexample to G6a on `staleLag` is 8 states long; without
it, 13. No verdict differs.

Verdicts (`step` unless said; 8 workers, 8 GB; every row 11 to 17 s):

| Configuration | Invariant | Verdict |
|---|---|---|
| `timely` | G6a and G6b and G6c | holds, 189 297 states, depth 44 |
| `timely` | `conformingEveryOfferBeforeExpiry` (K6's predicate) | holds, 189 297 states, depth 44 |
| `timely` | `ackedIsHeldOrSettled` (K5) | violated: 5 states under `step`, 8 under `noCrashStep`, 9 under `quietStep` |
| `flakyTip` | G6a (K3) | violated, 8 states |
| `flakyTip` | G6b; G6c | violated, 18; 19 states. **Holds, 1 468 808 states, depth 44, once timeliness is forgotten on going down** |
| `flakyTipSlowFlight` | G6b; G6c (K7) | violated, 18; 14 states. **G6b holds, 2 020 400 states, depth 44, once timeliness is forgotten** |
| `flakyTipNoSlack` | G6b (K3') | violated, 12 states |
| `staleLag` | G6a; G6b; G6c; K6 | violated, 12; 13; 14; 13 states |
| `staleLagWithSlack` | G6b; G6c | violated, 19; 20 states |

Under the first definition G6b and G6c are violated on `flakyTip` and G6b on
`flakyTipSlowFlight`, where simulation of the whole protocol reports that they
hold. The counterexample needs a crash and a late duplicate of a submission
that was first admitted on time; it is about what "timely" means across a
restart (finding 8). With timeliness forgotten on going down, the restated
rows were run with 2 workers and 4 GB, side by side: `timely` 229 339 states
at depth 51 in 38 s, `flakyTip` 205 s, `flakyTipSlowFlight` 286 s. The
restatement raises the state counts, because `seen` and the timely set now
differ between states that agreed before. Every other verdict and trace
length is unchanged by it.

Trace lengths are with one worker, when TLC's search is breadth first and its
counterexample a shortest one. Three were first recorded from runs with more
workers and were one to three states too long: G6a on `staleLag` (13, now 12),
K5 under `quietStep` (10, now 9), `wStale` on `staleLag` (9, now 6).

Reachability, each as `not(..)` and each violated: on `timely`,
`wOfferWithExpiry` (6 states), `wConformingFirstOffer` (6),
`wConformingFirstOfferInFlightABlock` (7), `wOffered` (7), `wRequeued` (9),
`wDown` (2), `wRestartedOwing` (6), `wBlockInFlight` (7), `wStopped` (4); on
`flakyTip`, the first three (6, 6, 7) and `wTimelyQueuedBehindEpoch` (6);
`wStale` on `staleLag` (6) and on `staleLagWithSlack` (6).

### The schedule rows, before and after the move

Every row about the schedule, as the gate of the whole-protocol specification
has it (bounded simulation), under simulation of the hub machine with the same
bounds (`quint run hubMachine.qnt --init=<init>`, seed 7, 60 or 80 steps, the
row's trace count), and under TLC. "v" is violated; the number is the length
of TLC's counterexample in states.

| Configuration | Invariant | Protocol gate | Hub machine, simulated | Hub machine, TLC | TLC, timeliness restated |
|---|---|---|---|---|---|
| `baseline` / `timely` | G6a, G6b, G6c | holds | holds | holds, exhaustive | holds, exhaustive |
| `flakyTip` | G6a (K3) | v | v | v, 8 | v, 8 |
| `flakyTip` | G6b | holds | holds | **v, 18** | holds, exhaustive |
| `flakyTip` | G6c | holds | holds | **v, 19** | holds, exhaustive |
| `flakyTipSlowFlight` | G6b | holds | holds | **v, 18** | holds, exhaustive |
| `flakyTipSlowFlight` | G6c (K7) | v | v | v, 14 | v, 14 |
| `flakyTipNoSlack` | G6b (K3') | v | v | v, 12 | v, 12 |
| `flakyTipNoSlack` | G6c (K3') | v | v | not a TLC row; G6b's is | |
| `staleLag` | G6a, G6b, G6c (K4) | v | v | v, 12; 13; 14 | v, 12; 13; 14 |
| `staleLag` | K6 | v | v | v, 13 | v, 13 |
| `staleLagWithSlack` | G6b (finding 2) | v, 8000 traces | v, 8000 traces | v, 19 | v, 19 |
| `baseline` / `timely` | K5 | v | v | v, 5; 8 without a crash; 9 without a shutdown | the same |
| `byzHub` | G6a, G6b, G6c | v (5000 and 12000 traces for the last two) | v | v, 8; 12; 13 | the same |
| `byzIndexer` | G6a, G6b, G6c | v | v | v, 8; 16; 17 | the same |

The three rows in bold are finding 8 under the first definition. Simulation,
of either machine, does not find the counterexample in the traces it samples;
TLC does. The protocol gate's rows are still simulated on the whole-protocol
machine, with timeliness as first defined, until they are removed from it.

Configuration in the state against configuration as a constant, on `timely`
with G6a, G6b and G6c: the compiled JSON is 13.7 MB with named inits and
23.6 MB with `const CONFIG` and an instance module; both give 189 297 states
at depth 44. TLC alone took 5 s on the constant form; the named form was timed
only as a whole row (17 s, beside another run). Named inits are kept.

Not measured: Apalache at bounded depths on this machine (one attempt failed
on its configuration and was not repeated); the route with an empty `~/.quint`
and Quint fetched by `npx`.

## Bounded model checking (not run)

**None of the commands in this section has been executed.** Both backends need
Java 21 (Quint 0.33.0's default Apalache is 0.62.1), which the machine this was
written on does not have. Whether Apalache or TLC accept the specification as
written is unknown.

Each "holds" cell, with Apalache:

```sh
quint verify --main=baseline --invariant=operatorBlind --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=queuedBytesConfidential --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=txidAuthenticity --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=lookupValidityPerHub --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=offeredBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=conformingFirstOfferBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=ackImpliesQueued --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=wellFormed --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzShim --invariant=offeredBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzShim --invariant=conformingFirstOfferBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzShim --invariant=ackImpliesQueued --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzHub --invariant=operatorBlind --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzHub --invariant=txidAuthenticity --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzIndexer --invariant=operatorBlind --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzIndexer --invariant=txidAuthenticity --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzIndexer --invariant=ackImpliesQueued --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=awaitAck --invariant=toldImpliesQueued --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=awaitAck --invariant=ackImpliesQueued --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=awaitAckByzIndexer --invariant=toldImpliesQueued --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=flakyTip --invariant=conformingFirstOfferBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
```

The same for G6c, and the configurations whose point is a violation (each
should report one; `flakyTipNoSlack` and `flakyTipSlowFlight` satisfy every
`assume`, so a checker that honours `assume` still has states to explore):

```sh
quint verify --main=baseline --invariant=conformingFirstOfferJudgedBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzShim --invariant=conformingFirstOfferJudgedBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=flakyTip --invariant=conformingFirstOfferJudgedBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=flakyTipNoSlack --invariant=conformingFirstOfferBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=flakyTipSlowFlight --invariant=conformingFirstOfferJudgedBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=staleLag --invariant=conformingFirstOfferBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=staleLagWithSlack --invariant=conformingFirstOfferBeforeExpiry --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant=ackedIsHeldOrSettled --max-steps=12 zeronym/spec/protocol/instances.qnt
```

Several of the scripted counterexamples are longer than 12 steps, so
`--max-steps=12` may not reach these violations; raise it as needed.

Each witness, as a reachability check that should report a violation:

```sh
quint verify --main=baseline --invariant='not(wPending)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wTxInMempool)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wTxMined)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wRefusedTipStale)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wRefusedDraining)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wRefusedTooLarge)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wRefusedExpiryTooTight)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wRefusedFull)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wRequeued)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wDroppedExpired)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wDroppedExhausted)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wQueuedDisclosed)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wUnparseableMissed)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wQueueOverCapacity)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wThirdPartyPayloadQueued)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wToldRefusedEverywhere)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=baseline --invariant='not(wToldNeverDelivered)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzHub --invariant='not(wTwinServed)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzHub --invariant='not(wFalseHeightServed)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=byzIndexer --invariant='not(wPrematureFlush)' --max-steps=12 zeronym/spec/protocol/instances.qnt
quint verify --main=staleLag --invariant='not(wEarlyFreeRunFlush)' --max-steps=12 zeronym/spec/protocol/instances.qnt
```

The two-state properties, with TLC (also `QUINT_TLC=1 sh check.sh`):

```sh
quint verify --backend tlc --main=baseline --temporal=chainMonotone,neverEvict,drainIsFinal zeronym/spec/protocol/instances.qnt
```

The commands use one-hub configurations: nested maps of records are supported
by Apalache but slow.

What the specification does to give those runs a chance, from Apalache's
documentation and not from running it:

| Construct | Consequence |
|---|---|
| `run`, `.then`, `.expect`, `--witnesses`, `--mbt` are simulator-only | The commands above cover invariants only; reachability is `not(w)` expected to be violated |
| `oneOf` on an empty set | Every pick is guarded |
| Unbounded integers, `powerset`, `allLists` | Not used; every universe is a finite set bounded by constants, the Byzantine sets included |
| A list in the state (the wallet's log) | Bounded by `--max-steps` |
| `assume` | Behaviour under verify unknown; `assumptionsTest` is the check that counts |
| Temporal definitions | For TLC only |

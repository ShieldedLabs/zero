# Taylor Hornby review — status of all 42 findings

Tracking table. One row per finding, named by its file in
`zeronym-22aa9851caf68-high-medium/`. Assessed against `origin/main` @ `7edc373`,
with `hub-7` / `shim-11` live in clearnet mode. Narrative and quotes are in
`REVIEW_PLAN.md`.

### Column meanings

**Sev** — the containing folder: `high` or `medium`.

**Live** — can this fire in the deployment we are actually running?
`yes` = affects the live clearnet pair · `nym` = mixnet path only, parked behind
the vsock latency problem, blocks shipping Nym but is not live risk ·
`process` = documentation, CI or deploy tooling rather than a running component.

**Status** — `FIXED` (with commit) · `PARTIAL` (bounded, mechanism survives) ·
`CLEAR` (obvious fix, not yet done) · `DECIDE` (needs a person) ·
`MEASURE` (needs a number first) · `EXTERNAL` (cannot be fixed in this repo).

**Test** — which harness actually exercises it. Four of them run, and they do
not overlap:

- `cargo` — a plain `cargo test`. **336 tests** (hub 147, shim 189).
- `cargo+f` — only under `--features mixnet-driver`. **8 further tests** the
  plain run never compiles (hub 152, shim 192 with it). A separate CI job runs
  these, because a plain `cargo test` silently skips them.
- `guard` — `zeronym/guards.sh`, 12 assertions over documents and config, in CI.
- `smoke` — `smoke.sh`, needs a live deployed pair. **Never runs in CI.**

And the routes not yet taken: `unit` = easy pure-logic test, `intg` = easy
integration test with a harness that already exists, `hard` = needs new
infrastructure, `none` = not mechanically testable.

Four tests are `#[ignore]`d and all four are deliberate: three write fixtures,
one needs a reachable indexer (`ZIH_TEST_INDEXER`). None is a skipped assertion.

**Owner** — who can actually close it.

---

## High (10)

| #   | Finding                                                                    | Sev  | Live    | Status                  | Test  | Owner        | Notes                                                                                                                                                                                                       |
|-----|----------------------------------------------------------------------------|------|---------|-------------------------|-------|--------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| H1  | `caution verify` reproduces from a repo the operator nominates             | high | yes     | **MITIGATED** `48cd321` | guard | Caution + us | The auditor procedure now says to re-assemble from OUR repo and diff against the tree caution verify cloned -- which Taylor calls 'the whole fix'                                                           |
| H2  | Core linkage survives: wallet leg unpadded, published tx self-timestamping | high | yes     | MEASURE                 | hard  | wallet teams | **The shim cannot fix this** — padding must happen at the sender. Verified today: no padding in `proxy.rs`/`intercept.rs`. Measure the batch size distribution before deciding severity or asking wallets   |
| H3  | `deploy.sh` defaults to debug, turning attestation off                     | high | process | **FIXED** `a6c35d0`     | guard | us           | Defaults to attested; guarded by guards.sh                                                                                                                                                                  |
| H4  | `GetTransaction` flood starves migration diversion                         | high | nym     | DECIDE                  | intg  | us           | 61 sphinx packets per unauthenticated ~100-byte request. Part of the one ingress design (see H6)                                                                                                            |
| H5  | Hub HTTP lookup path has no concurrency bound                              | high | yes     | **FIXED** `e37e2a7`     | cargo | us           | 64 in flight, 503 rather than queueing -- the mixnet sibling's own stated rule. Two tests: refusal when full, and that the bound reaches lookups only                                                       |
| H6  | Hub queue unauthenticated fill destroys migrations silently                | high | yes     | DECIDE                  | intg  | us           | The bind: an entry must never carry a submitter identifier, "which is also the identifier a per-submitter quota would need"                                                                                 |
| H7  | SURB-starved lookup replies grow the SDK buffer without bound → OOM        | high | nym     | DECIDE                  | hard  | us           | 64-byte request makes the hub buffer 64 KiB it can never send, forever                                                                                                                                      |
| H8  | Junk `SendTransaction` flood consumes the shim's whole mixnet egress       | high | nym     | **MITIGATED** `48cd321` | cargo | us           | A zero-length transaction is refused before it costs a frame -- Taylor's 'at minimum'. The broader junk-flood amplifier is unaddressed                                                                      |
| H9  | `log_verdict` logs migration value balance at INFO                         | high | yes     | **FIXED** `72df7a7`     | cargo | us           | `classify_logging.rs` asserts the fields are absent                                                                                                                                                         |
| H10 | Shim submits to every hub, so an operator appends their own                | high | nym     | **MITIGATED** `48cd321` | unit  | us           | Warns at startup when >1 hub is configured, naming each. Sending to all is the rule; the mitigation is that it is no longer quiet. Config is measured into PCR0/1, so an appended hub changes a measurement |

## Medium (32)

| #   | Finding                                                                    | Sev    | Live    | Status                                       | Test    | Owner   | Notes                                                                                                                                                                                                                                |
|-----|----------------------------------------------------------------------------|--------|---------|----------------------------------------------|---------|---------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| M1  | Constant tip offset is a tunable expiry-keyed admission filter             | medium | yes     | DECIDE                                       | unit    | us      | Moves with the TipTracker decision (M19/M20)                                                                                                                                                                                         |
| M2  | Attested enclave console reopenable; `debug.ssh_keys` ungated              | medium | yes     | **MITIGATED** `48cd321` / EXTERNAL (root)    | guard   | Caution | Both assemble scripts now exit 2 on --ssh-key without --debug, which closes the deploy path -- Taylor's 'three-line change'. Gating debug.ssh_keys on debug.enabled is still Caution's                                               |
| M3  | TLS binding verified once by hand, if ever                                 | medium | yes     | DECIDE                                       | hard    | us      | Unbounded undetected window for certificate substitution. Natural home for `pcrs.sh`, still untracked                                                                                                                                |
| M4  | Auditor recipe omits two checks, names a defence the platform does not use | medium | process | **FIXED** `a023129`                          | guard   | us      | Rewritten around the checks that exist. Guarded by guards.sh                                                                                                                                                                         |
| M5  | Unknown consensus branch id diverts all shielded traffic                   | medium | yes     | **MITIGATED** `48cd321`                      | cargo   | us      | An unrecognised branch is now told apart from garbage and reported once per process. Two tests pin zebra's wording. Fail-open vs fail-closed still undecided                                                                         |
| M6  | `deploy.sh` tells operators to expect PCR0/1 failure, accept PCR2 alone    | medium | process | **FIXED** `064e0f3`                          | guard   | us      | Guarded by guards.sh, which distinguishes asserting the advice from quoting it while correcting it                                                                                                                                   |
| M7  | Egress allowlist discarded by the platform; unrestricted outbound          | medium | yes     | **FIXED (docs)** `8e5d23d` / EXTERNAL (root) | guard   | Caution | Both manifests say the rules are unenforced; guarded by guards.sh. The property still does not exist -- only Caution can change that                                                                                                 |
| M8  | Hub README says the attestation binds the binary to `EXPECTED_SHA256`      | medium | process | **FIXED** `a023129`                          | guard   | us      | Eleven passages rewritten. guards.sh checks a published table against EXPECTED_SHA256 -- the drift that actually shipped                                                                                                             |
| M9  | Unbounded indexer response body                                            | medium | yes     | **FIXED** `b68fc59`                          | cargo   | us      | `Limited` at 4 MiB, matching the shim's idiom on the mirror-image hop                                                                                                                                                                |
| M10 | Zaino node rejections are never verdicts                                   | medium | yes     | PARTIAL                                      | unit    | us      | `MAX_REQUEUE_ATTEMPTS = 8` bounds it; a `Rejected` is still unobtainable. Needs a verdict source                                                                                                                                     |
| M11 | Flush destroys a migration on one unverifiable verdict                     | medium | yes     | PARTIAL                                      | unit    | us      | `best_of` added; a hostile `Rejected` still beats every `Retryable` and drops the last copy                                                                                                                                          |
| M12 | Indexer TLS optional in code, required in every document                   | medium | yes     | **FIXED** `e37e2a7`                          | cargo   | us      | Startup aborts; `--allow-plaintext-indexer` is the explicit escape, measured into PCR0/1. Tests cover unset, empty and whitespace                                                                                                    |
| M13 | Liveness probe reads its own send backlog as gateway silence               | medium | nym     | **FIXED** `3d7121b`                          | cargo+f | us      | Three regression tests                                                                                                                                                                                                               |
| M14 | Lookup fall-through hands every wallet's txid to the operator's indexer    | medium | yes     | **FIXED** `e169794`                          | guard   | us      | deploy.env.example no longer points BACKEND and INDEXERS at one host; the placeholder is TEST-NET-3 so it cannot be copied into production unnoticed                                                                                 |
| M15 | Automatic fresh identity permanently invalidates every shim                | medium | nym     | **MITIGATED** `48cd321`                      | unit    | us      | address_generation on /nym-status, bumped only on a real change, with an error!. The fresh-identity fallback still happens                                                                                                           |
| M16 | Hub Nym identity has no trust anchor                                       | medium | nym     | DECIDE                                       | hard    | us      | The address *is* the key, fetched over WebPKI on operator DNS                                                                                                                                                                        |
| M17 | Nym lookup flood starves `GetTransaction` fleet-wide                       | medium | nym     | DECIDE                                       | intg    | us      | Part of the one ingress design (H6)                                                                                                                                                                                                  |
| M18 | Reorg branch resets `last_advance`, masking a stale tip                    | medium | yes     | **FIXED** `c7440b5`                          | cargo   | us      | Regression test added, with a test-only clock seam                                                                                                                                                                                   |
| M19 | Unbounded tip advance drives the flush clock                               | medium | yes     | DECIDE                                       | unit    | us      | Root of M1/M19/M20/M22                                                                                                                                                                                                               |
| M20 | Tip overshoot latches the hub permanently stale                            | medium | yes     | DECIDE                                       | unit    | us      | "every subsequent truthful observation is discarded as an implausible regression". **The trap**: `REVIEW.md` #8's max-over-nodes is where the unbounded advance comes from                                                           |
| M21 | Unauthenticated pre-publication transaction disclosure                     | medium | yes     | **FIXED** `45e408f`                          | cargo   | us      | Queue hits answer found/height-0 with an EMPTY body, both transports. Existence disclosure remains open deliberately -- closing it costs a wallet 'pending' vs 'never seen'. Three tests, plus smoke.sh proving via height not bytes |
| M22 | Indexer chooses which batch members reach the chain                        | medium | yes     | **MITIGATED** `48cd321`                      | unit    | us      | Alarms on achieved>=1 && requeued>=1 from a SINGLE endpoint, which an outage cannot produce. Detection only; the indexer still chooses                                                                                               |
| M23 | Nym submit acks are never read, so every hub refusal is invisible          | medium | nym     | DECIDE                                       | intg    | us      | **Clearnet reads a real verdict** (`hub.rs:128`, `:202`); this is the mixnet path only                                                                                                                                               |
| M24 | Operator-controlled DNS permits a layer-4 relay                            | medium | yes     | **MITIGATED** `48cd321`                      | guard   | us      | The auditor procedure now includes resolving the domain against the Caution-managed target -- one dig, and it makes a layer-4 relay observable                                                                                       |
| M25 | Publish verdict strings are zcashd's vocabulary only                       | medium | yes     | **FIXED** `8fb91b9`                          | cargo   | us      | Default inverted to `Retryable`, test pins it                                                                                                                                                                                        |
| M26 | Reproduce gate has never run on a commit that published a hash             | medium | process | **FIXED** `cec619c`                          | guard   | us      | Both gates run on push; guarded by guards.sh                                                                                                                                                                                         |
| M27 | Shim config has no fail-closed mode                                        | medium | yes     | **FIXED** `e37e2a7`                          | cargo   | us      | `--require-diversion` refuses to start forward-only. Four tests, including that the flag takes an explicit true/false rather than mere env presence                                                                                  |
| M28 | Shim has neither retransmission bound the hub has                          | medium | nym     | CLEAR                                        | hard    | us      | Measured and "fixed **on the hub only**"                                                                                                                                                                                             |
| M29 | Every shim teardown path destroys acknowledged submits                     | medium | nym     | **MITIGATED** `48cd321`                      | hard    | us      | Stop/Rebuild/Died now report what they abandon, counts only. The loss is unavoidable (no drain-then-disconnect in the SDK); the silence was not                                                                                      |
| M30 | Shim proxy has unbounded inbound concurrency                               | medium | yes     | **FIXED** `e37e2a7`                          | cargo   | us      | 256 in flight, permit held for the connection's life so an idle socket costs a slot. Tests cover refusal at the limit and permit release                                                                                             |
| M31 | README amount overclaim                                                    | medium | process | **FIXED** `a023129`                          | guard   | us      | Says the operator can recover the txid AND its value; guarded by guards.sh                                                                                                                                                           |
| M32 | Widening the flush window cannot raise the delivered anonymity set         | medium | yes     | MEASURE                                      | none    | us      | Explicitly **not** a re-report of the k=1 residual: it says the named remedies do not deliver. Second half (`achieved_batch_size` over-count) blocked on the `AlreadyKnown` semantics question                                       |

---

## Roll-up

| Status                           | High | Medium | Total  |
|----------------------------------|------|--------|--------|
| FIXED                            | 3    | 15     | **18** |
| MITIGATED (partial, no tradeoff) | 3    | 6      | **9**  |
| PARTIAL                          | 0    | 2      | 2      |
| CLEAR (obvious fix, pending)     | 0    | 1      | 1      |
| DECIDE                           | 3    | 7      | 10     |
| MEASURE                          | 1    | 1      | 2      |

By transport: **25 live**, **11 mixnet-only** (parked), **6 process**.

By test route, what CI would actually catch on a bad push:

| Route                    | Findings | Runs in CI?       |
|--------------------------|----------|-------------------|
| `cargo`                  | 11       | yes               |
| `cargo+f`                | 1        | yes, separate job |
| `guard`                  | 11       | yes               |
| **covered subtotal**     | **23**   |                   |
| `unit` (easy, unwritten) | 8        | no                |
| `intg` (easy, unwritten) | 4        | no                |
| `hard`                   | 6        | no                |
| `none`                   | 1        | never             |

**23 of 42 are covered by something that runs automatically.** Nothing relies on
`smoke.sh`, which is the right outcome: it needs a live pair, so a finding whose
only coverage was there would be untested on every push.

## What the table makes obvious

- **18 of the 42 are waiting on a decision, not on effort.** Four of them
  (M1, M19, M20, M22) collapse into the single TipTracker choice.
- **Five findings (H4, H6, H7, H8, M17) are one design**, not five fixes.
- **Six findings are one document** — the verification chain (H1, M2, M3, M4,
  M8, M24) — and three are already corrected.
- **A quarter of the work is not live.** Eleven mixnet-only findings block
  shipping Nym and nothing else.
- **Seven cheap guard tests** would lock down the class of drift that has already
  shipped twice: the hash table disagreeing with `EXPECTED_SHA256`, and stale PCR
  advice surviving a documentation sweep because it lived in a `.sh` file.

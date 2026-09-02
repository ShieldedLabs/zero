# `log_verdict` logs every transaction's exact value balance, size and expiry at INFO, so the shipped debug deployment hands the operator the amount the README says they never learn

**Severity**: High
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/intercept.rs:111` (the call site), `:577-637` (`log_verdict`), `:83-87` (`PREFIX_LOG_BYTES`); the default filter at `audit-target/zeronym/shim/src/main.rs:21-30`; the fields' provenance at `audit-target/zeronym/shim/src/classify.rs:275-302`; read against `audit-target/zeronym/shim/src/proxy.rs:812-827`, `audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:178-182`, `audit-target/zeronym/hub/REVIEW.md` (rules "Never log a txid…" and #10), `audit-target/zeronym/README.md:33` and `:54`
**Found by agent:** Local (file audit of `shim/src/intercept.rs`); validated 2026-08-18
**In scope of audit?** Yes — priority area 6, "Log and telemetry discipline"

## Description

For **every** `SendTransaction` the shim sees, `intercept::log_verdict` emits a
`tracing::info!` line on target `zis::classify`, before any routing decision.
For the `Class::Migration` arm — that is, for every transaction the shim is about
to divert to the hub — that line carries:

* `orchard_vb` — the Orchard bundle's **value balance in zatoshis**, i.e. the exact
  amount of value leaving the Orchard pool,
* `ironwood_vb`, `sapling_vb` — the same for the other two pools,
* `orchard_actions` — the Orchard action count,
* `expiry` — `nExpiryHeight`,
* `inputs`, `outputs` — transparent input/output counts,
* `tx_len` — the serialized transaction length in bytes,
* `version` — the transaction version.

The default log filter is `info` (`shim/src/main.rs:26-30`), and the deployed
manifest leaves `RUST_LOG` unset (`caution.hcl.tmpl:178-182`), so this line is
emitted in a deployed enclave with no configuration change. Under
`--debug` the manifest sets `RUST_LOG = "zis::proxy=debug,info"`, whose trailing
`info` directive keeps `zis::classify` at INFO as well — so the line is emitted in
**both** configurations.

**Where it goes is the whole question, and it is now settled.** Enclave console
output reaches the Nitro parent host — the operator — **only** when the enclave is
launched in debug mode (AWS: `nitro-cli console` "can be used only on an enclave
that was launched with the `--debug-mode` option"; Caution: `debug.enabled`
"Allows reading enclave console output but disables attestation verification").
The project confirms this in-tree: `shim/deploy/README.md:289` says `/nym-diag`
exists because its data is "neither readable on an attested enclave, which has no
console."

So the correct framing is **not** "the enclave leaks its logs". It is:

> the shim writes the one quantity the product promises to withhold into a log
> line at default level, and the repository's shipped deploy default is the
> configuration that delivers that log line to the adversary's disk.

`deploy.sh:52` is `DEBUG=${DEBUG:-1}` and `deploy.env.example:66` ships `DEBUG=1`
(filed as the parent issue,
`deploy-script-defaults-to-debug-mode-which-turns-attestation-off.md`). Under
that default, `assemble-caution.sh:567-573` opens SSH on the parent host and
`caution.hcl.tmpl:191-193` states this "opens port 22 on the parent so the console
can be read at `/var/log/nitro_enclaves/enclave-console.log`". The operator's
runbook additionally *instructs* operators to flip debug on to diagnose a shim
that "boots but will not serve" (`shim/deploy/caution/OPERATORS.md:358-360`) —
while that enclave is serving real wallets on its real public hostname.

The result directly contradicts `README.md:33`, which lists under **Not
protected**: "The operator learns *that* a client migrated, **though not the
amount or which transaction**." Under the shipped default they learn both, in
real time, with no chain join required at all.

The project's own rule forbids exactly this. `hub/REVIEW.md`:

> Never log a txid, a transaction body, or a per-entry identifier at any level. In a
> Nitro enclave the tracing output reaches the parent via the console […] Log counts,
> reasons and aggregate timings only.

And the shim's own `proxy.rs:812-819` applies the same premise to a **far less**
sensitive line:

```rust
    // DEBUG, not INFO, and that is a privacy decision rather than a noise one.
    // A per-request line naming the method a wallet called is exactly the
    // metadata this component exists to deny the operator, and it would be
    // sitting in a log file on the operator's box. `RUST_LOG=zis::proxy=debug`
    // turns it on for a demo or a debugging session; nothing turns it on by
    // default. The classifier's own `zis::classify` lines stay at INFO, because
    // in this proof of concept they are the only visible output.
```

That last sentence is the defect in one line: the INFO placement of
`zis::classify` is justified by the shim being *a proof of concept whose only
visible output is the verdict*. The shim is now the deployed product
(`README.md:88`: "Deployed: classify and divert … attested Nitro enclaves"), so
the justification has expired while the log level has not. A method **name** is
held at DEBUG on privacy grounds; zatoshi **amounts** sit at INFO.

## Attack Scenario and Steps

**The live case — the shipped deploy default.**

1. The operator deploys with `deploy.sh`'s default (`DEBUG=1`), which passes
   `--debug --ssh-key <their key>` and therefore has SSH on the parent host and a
   captured console log at `/var/log/nitro_enclaves/enclave-console.log`.
2. A wallet at IP `X` opens a TLS connection to the shim's public endpoint. The
   parent host records the source IP and connection timing at the TCP/vsock
   layer — a stated residual (`README.md:33`).
3. The wallet sends its Orchard-touching migration. `intercept.rs:111` runs
   `log_verdict` *before* the divert at `:117-120`, writing e.g.
   `version=V6 orchard_actions=2 orchard_vb=+250000 ironwood_vb=-240000 sapling_vb=+0 expiry=Some(3433150) inputs=0 outputs=0 tx_len=11994 diverted_in_production=true`
   to the console.
4. The operator reads the log. **They now hold the amount directly** — no chain
   join, no batch analysis, no timing correlation. Joining the log timestamp to
   the connection open at the TCP layer yields `IP X → 0.0025 ZEC moved out of
   Orchard`, permanently.
5. If they also want the on-chain transaction, every logged field is a **public
   field of the transaction once it is on chain**, so the tuple is a near-unique
   join key: `orchard_vb` is `valueBalanceOrchard`, `expiry` is `nExpiryHeight`,
   `tx_len` is `raw.len()` — the serialized length (`classify.rs:299`) — and
   `inputs`/`outputs` are the transparent counts. Twenty blocks later the hub
   publishes a shuffled batch; the operator scans it (or the whole chain, at
   leisure, forever) for the match. `README.md:34` states the measured modal
   batch is zero or one, so the match is unambiguous even on `tx_len` alone.
   The shuffle, the 20-block delay, the mixnet hop and the enclave are all
   bypassed, because the join key was written to the adversary's own disk before
   any of them ran.

**The attested case, stated honestly and not overstated.** With
`debug { enabled = false }` the parent has **no console channel at all**, so this
line is not readable and this finding is **not live**. The defect that remains in
that configuration is latent: the moment anyone follows
`shim/deploy/caution/OPERATORS.md:358-360` and flips debug on to diagnose a
problem, every wallet using that endpoint has its migration amounts written to
the operator's disk for the duration.

**Attack Requirements and Assumptions:**
- The attacker is the indexer operator — adversary #1 in the threat model, and the
  party the whole component exists to defend against. No special position is
  required beyond the one they hold by definition (they own the parent host and
  deploy the enclave).
- No exploitation step is needed: the data is written to a file the operator
  already has shell access to, by the default deployment path.
- The chain-side half of the join needs no privilege: the chain is public and
  permanent, so it can be performed retrospectively, at any time.
- The only thing that would defeat the chain join is many simultaneous migrations
  with *identical* value balance, action count, expiry and byte length.
  `README.md:34` states the measured modal batch is zero or one. And under the
  shipped default the join is not needed anyway, because the amount is logged
  directly.

## Impact on Users

A user who migrates through a zero-indexer shim is told by the project's own
README that the operator does not learn the amount or which transaction. On the
repository's default deployment the operator learns both, plus the linkage to the
wallet's IP address. That is the single outcome the product is built to prevent
(`README.md:54`: *"Joining them links **IP address to on-chain transaction to
balance**"*), and it is unrecoverable because the chain is permanent and the
console log is a file on someone else's disk.

This is worse than not deploying the shim in one respect: a user who broadcasts
directly at least knows their indexer sees them. A user behind a zeronym shim
believes they are protected, and `README.md:68` tells them they need "install
nothing and change no setting" to get that protection.

The persistence matters as much as the operator's own intent: the console log
survives the session on the parent host, so the exposure also reaches anyone who
breaches that host, anyone who obtains a backup, and any legal process served on
the operator.

## Technical Details / Code Analysis

**The call site runs on every `SendTransaction`, before any routing decision**
(`shim/src/intercept.rs:99-124`):

```rust
 99    let (parts, body) = req.into_parts();
100
101    // The only buffering in the entire shim, and it is bounded.
102    let collected = match Limited::new(body, MAX_SEND_TX_BYTES).collect().await {
103        Ok(collected) => collected,
104        Err(err) => return Ok(body_read_failed(err)),
105    };
106
107    let trailers = collected.trailers().cloned();
108    let frame = collected.to_bytes();
109
110    let (inspection, tx_data) = inspect(&parts.headers, &frame);
111    log_verdict(&inspection, &frame);
112
113    // A migration bound for the hub is diverted here, and ONLY here does the
114    // operator's indexer stay undialled: …
117    if inspection.treat_as_migration() {
118        if let Some(diversion) = diversion {
119            return divert(&diversion, tx_data).await;
120        }
```

**The Migration arm** (`shim/src/intercept.rs:581-601`):

```rust
581        Inspection::Classified(evidence) => match evidence.class {
582            Class::Migration => tracing::info!(
583                target: "zis::classify",
584                version = %evidence.version,
585                // The deciding fact, first on the line.
586                orchard_actions = evidence.orchard_actions,
587                orchard_vb = %format!("{:+}", evidence.orchard_vb),
588                ironwood_vb = %format!("{:+}", evidence.ironwood_vb),
589                sapling_vb = %format!("{:+}", evidence.sapling_vb),
590                expiry = ?evidence.expiry_height,
591                inputs = evidence.inputs,
592                outputs = evidence.outputs,
593                tx_len = evidence.len,
594                diverted_in_production,
...
601            ),
```

The `Class::PassThrough` arm (`:602-615`) logs the identical field set for every
non-Orchard broadcast.

**Each field is filled directly from the parsed transaction**
(`shim/src/classify.rs:275-302`):

```rust
275    let orchard_actions = orchard_action_count(&tx);
276    let orchard_vb = tx.orchard_value_balance().orchard_amount().zatoshis();
277    let ironwood_vb = tx.ironwood_value_balance().ironwood_amount().zatoshis();
278    let sapling_vb = tx.sapling_value_balance().sapling_amount().zatoshis();
...
296        expiry_height: tx.expiry_height().map(|height| height.0),
297        inputs: tx.inputs().len(),
298        outputs: tx.outputs().len(),
299        len: raw.len(),
```

`orchard_vb` is `valueBalanceOrchard`, a cleartext transaction-level field;
`expiry` is `nExpiryHeight`; `len` is the length of the raw serialized
transaction, i.e. exactly its on-chain serialized length. All are recoverable from
the chain, which is what makes the log line a join key rather than mere telemetry.
The project's own review says so explicitly, in rule #10: *"`orchard_vb` is public
on every diverted transaction … An observer partitions any batch by
`orchard_vb`."*

**The default filter is `info`** (`shim/src/main.rs:21-30`):

```rust
21    // `info` deliberately does NOT include the per-request `zis::proxy` line:
22    // that line names the method each wallet called, which is a metadata source
23    // this component exists to deny the operator, and it would live in a log
24    // file on the operator's box. `RUST_LOG=zis::proxy=debug,info` turns it on
25    // when someone is debugging or demoing. `zis::classify` stays at info.
26    tracing_subscriber::fmt()
27        .with_env_filter(
28            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
29        )
30        .init();
```

and the deployed enclave leaves `RUST_LOG` unset
(`shim/deploy/caution/caution.hcl.tmpl:178-182`):

```
      # Default is `info`, which deliberately omits the per-request zis::proxy
      # line naming the method each wallet called. That line is exactly the
      # metadata this component exists to deny an operator, so it stays off in
      # a deployed enclave. Turn it on only for a local demo, never here.
      # RUST_LOG = "zis::proxy=debug,info"
```

That comment is the project stating, in the deployment template, that a *method
name* is too sensitive for a deployed enclave's log. `zis::classify` at `info` is
not excluded by that filter and carries orders of magnitude more. Note also that
under `--debug` `assemble-caution.sh:568` uncomments exactly that line, and its
`,info` directive keeps `zis::classify` emitting — so debug mode turns the smaller
leak on *without* turning the larger one off.

**The smaller instance on the fail-safe arms.** `log_verdict`'s `Unparseable` and
`Failsafe` arms (`shim/src/intercept.rs:616-635`) log `frame_len` and

```rust
621                body_prefix = %hex_prefix(frame, GRPC_PREFIX_LEN + PREFIX_LOG_BYTES),
```

i.e. 13 raw bytes of the request frame plus its exact length, at `warn` (so also
emitted under the default filter). Both arms are reachable **on demand by any
wallet** — send a truncated frame, or a `RawTransaction` whose `data` does not
parse — so this is attacker-triggerable transaction-derived material going to the
same reader. The doc comment at `:83-87` claims those eight bytes "carry the
version and version group id"; on the `Failsafe` arm they are the leading bytes of
the *protobuf* message (tag plus length varint) rather than of the transaction, so
the comment is also inaccurate.

**Stale message text.** The `Migration` and `PassThrough` arms still say "this PoC
still forwards it; production diverts it to the hub" (`:598-600`). The shim *is*
production. (Filed separately as
`forward-only-log-claims-migration-was-diverted.md`, which concerns the
`diverted_in_production` field's meaning rather than the leaked values.)

## Recommendations

1. **Reduce the `Class::Migration` and `Class::PassThrough` arms to what
   `hub/REVIEW.md` already requires of the hub: counts, reasons and aggregate
   timings.** Concretely, drop `orchard_vb`, `ironwood_vb`, `sapling_vb`,
   `expiry`, `inputs`, `outputs` and `tx_len` from both arms, keeping at most
   `version`, `orchard_actions` and the verdict. Better still, emit only a
   periodic aggregate counter (`migrations_diverted_total`).
2. **Drop `body_prefix` and `frame_len` from the fail-safe arms**, or move both
   arms behind the same gate as `zis::proxy`, so a wallet cannot cause raw request
   bytes to be logged on demand.
3. **If any per-transaction evidence is genuinely needed for operations, put it
   behind the `zis::classify=debug` gate**, so `RUST_LOG` is the single switch and
   the deployment template's existing warning covers it. Then remove the "in this
   proof of concept they are the only visible output" rationale at
   `proxy.rs:818-819`, which no longer holds.
4. **Fix `deploy.sh:52` so `DEBUG` defaults to `0`** (tracked as the parent issue),
   so the configuration in which this line is certainly readable is not the
   shipped default.
5. **Document the console premise in `README.md`.** Every logging decision in both
   binaries rests on "does enclave stdout reach the parent?", the answer is
   "only in debug mode", and stating it would let operators reason about the
   diagnostic procedure in `OPERATORS.md:358-360` correctly.

## Validation Information

**Verdict: CONFIRMED. Severity: High.**

Every mechanical claim was re-verified against the target:

| Claim | Verified at |
|---|---|
| `log_verdict` called on every `SendTransaction`, before routing | `intercept.rs:110-111`, divert at `:117-120` |
| Migration arm logs all nine fields at INFO | `intercept.rs:582-601` |
| PassThrough arm logs the same set at INFO | `intercept.rs:602-615` |
| Fail-safe arms log 13 raw frame bytes + length at WARN, wallet-triggerable | `intercept.rs:616-635`, `:66`, `:87` |
| `orchard_vb` = `valueBalanceOrchard`; `len` = raw serialized length | `classify.rs:276`, `:299` |
| Default filter is `info`; deployed manifest leaves `RUST_LOG` unset | `main.rs:26-30`; `caution.hcl.tmpl:182` |
| `--debug`'s `RUST_LOG="zis::proxy=debug,info"` keeps `zis::classify` at INFO | `assemble-caution.sh:568`; `EnvFilter` semantics (trailing `info` is the global default directive) |
| Method name held at DEBUG for privacy, `zis::classify` left at INFO because "in this proof of concept" | `proxy.rs:812-819` |
| Project's counts-only rule | `hub/REVIEW.md`, "Never log a txid, a transaction body, or a per-entry identifier at any level" |
| `orchard_vb` publicly partitions a batch — the project's own words | `hub/REVIEW.md` #10 |
| README claims the operator does not learn the amount | `README.md:33` |
| Console readable on the parent **only** in debug | Coordinator open item 7 (AWS + Caution vendor docs); corroborated in-tree at `shim/deploy/README.md:289` and `caution.hcl.tmpl:191-193` |
| Runbook instructs operators to enable debug for diagnosis | `shim/deploy/caution/OPERATORS.md:358-360` |

**Corrections made during validation.** The draft's "Case B" left the attested-mode
console premise open. It is no longer open: coordinator open item 7 resolved it
from two vendor sources, and `shim/deploy/README.md:289` states the same thing
in-tree. The issue has been rewritten so that:

- the attested configuration is described as **not** leaking this line, plainly
  and without hedging — this finding must not be cited as evidence that an
  attested enclave leaks logs;
- the live exposure is attributed to its actual cause, the `DEBUG=1` deploy
  default, with the parent issue cited;
- a second live route is added that the draft missed and that survives a fix to
  the default: `shim/deploy/caution/OPERATORS.md:358-360` **instructs** operators
  to enable debug on a shim that boots but will not serve, and that shim is
  serving real wallets on its real hostname while the console is open.

Two claims were also strengthened, both verified: under `--debug` the manifest's
`RUST_LOG="zis::proxy=debug,info"` does **not** suppress `zis::classify`, so debug
mode adds the method-name leak on top of this one; and the fail-safe arms'
`body_prefix` is reachable **on demand by any wallet**, which makes that sub-case
attacker-triggerable rather than incidental.

**Severity justification — High, and how it moves if the parent issue is fixed.**

*Impact:* the operator obtains the exact zatoshi amount, size and expiry of every
transaction a wallet broadcasts through the shim, in real time, next to the TCP
connection that carries the wallet's source IP. That is the precise linkage
`README.md:54` calls "the attack" and the precise quantity `README.md:33` promises
is not learned. It affects every user of the endpoint, it is retrospective and
permanent, and it needs no chain analysis at all.

*Likelihood:* high **as the system ships today**, because the exposure is gated on
debug mode and debug mode is the default of `deploy.sh` and
`deploy.env.example`. `docs/AVOIDING-FALSE-POSITIVES.md` §4 and §7 both warn
against grading debug-only leaks highly — and both name the identical exception
that applies here: the leak is real when the insecure mode is **enabled by
default**, which it is.

*If `deploy.sh` is fixed to `DEBUG=0` but this log line is left alone*, the
correct grade becomes **Medium**: the sanctioned diagnostic procedure in
`OPERATORS.md:358-360` still opens the console on a shim serving live wallet
traffic, so the exposure remains reachable by a documented operator action rather
than by a default. That conditional is stated here so the report can present the
two fixes as independent and both necessary.

*Why not Critical:* no funds move; the exposure requires the debug configuration
rather than being present in the attested deployment; and it is bounded to users
of the affected endpoint.

**Relationship to the parent issue.** This is the blast radius of
`deploy-script-defaults-to-debug-mode-which-turns-attestation-off.md`, but it is a
**separate defect with a separate fix**: inverting the `DEBUG` default does not
remove these fields from the log line, and removing the fields does not restore
attestation, close SSH, or suppress the `zis::proxy` method log. Both fixes are
needed and neither substitutes for the other. Per coordinator open item 7 the
report should present them as parent and blast radius, in that order.


---

## [ADDENDUM — Global auditor, focus area G18 (log and telemetry discipline as one policy), 2026-08-18. The CONFIRMED verdict and severity are untouched. Two corrections to the RECOMMENDATIONS, and one to the scenario bound.]

**(a) Recommendation 1 is incomplete: the minimal fix leaves a per-migration
arrival feed intact.** Stripping the fields from the `Class::Migration` arm
removes the *amount*, not the *event*. Two timestamped INFO lines are emitted per
diverted migration, not one:

- `shim/src/intercept.rs:582-601` — `log_verdict`'s `Class::Migration` arm, the
  subject of this issue; and
- `shim/src/intercept.rs:197-201`, inside `divert`, after the hub answers:

```rust
            tracing::info!(
                target: "zis::classify",
                accepted = error_code == 0,
                "migration diverted to the hub"
            );
```

The second line survives every fix proposed above and is, on its own, exactly
what `hub/REVIEW.md` #157 forbids on the other binary: a per-entry event with a
timestamp. It is the shim-side twin of
`hub-per-admission-info-log-is-a-real-time-per-entry-arrival-feed.md`, on the side
where the parent host is the primary adversary rather than a third party, and it
additionally carries the hub's verdict (`accepted`) — so a reader of the console
learns not only that a migration was diverted at time T but whether the hub took
it. Recommendation 1 should be extended to: **reduce both call sites to a single
periodic aggregate** (`migrations_diverted_total`, `diverts_refused_total`), which
is the form REVIEW #157 permits and which `hub/src/batcher.rs:396-403` already
uses correctly.

**(b) A scoping refinement that makes the fix cheaper, and which should be stated
so it is not lost: the `Class::PassThrough` arm is not a leak.** A pass-through
transaction is forwarded to the operator's own indexer in full
(`shim/src/intercept.rs:127-131`), so its value balances, expiry, counts and
length are already in the operator's hands from the transaction itself. Logging
them adds nothing. The arm that must lose its fields is `Class::Migration`, plus
the two fail-safe arms (which are also diverted). Removing the fields from the
`PassThrough` arm as well is still worth doing for uniformity and to keep the two
arms from drifting, but it is not the security-bearing half, and a maintainer
weighing the diagnostic value of the line should know which half is which.

**(c) The Case B bound is narrower than "not live in an attested deployment".**
This issue correctly records that with `debug { enabled = false }` the parent has
no console. What was not established at the time is that the console is closed by
a **parent-side launch flag**, not by attestation: `--debug-mode` is appended to
`nitro-cli run-enclave` by a systemd unit on the parent's disk
(Caution platform, `terraform/modules/aws/nitro-enclave/user-data.sh:166`), the
EIF is at `/opt/nitro/enclave.eif` (`:52`), `aws-nitro-enclaves-cli` is installed
unconditionally (`:15`), and `debug.ssh_keys` — which is **not** gated on
`debug.enabled` (`src/api/src/deployment.rs:2158-2164`) — puts the operator's key
on the parent's default administrative account. So an operator who set an SSH key
in an otherwise fully attested manifest can re-launch the same image in debug
mode and read this exact line, with `RUST_LOG` still at the measured `info`. That
is filed separately as
`attested-enclave-console-is-reopenable-from-the-parent-because-debug-mode-is-a-launch-flag-and-ssh-keys-is-not-gated-on-it.md`
(Medium) rather than being folded in here, so the harm is not counted twice — but
the two must be read together, because it is the reason Recommendation 1 should
not be deferred on the grounds that attestation contains this line.


DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

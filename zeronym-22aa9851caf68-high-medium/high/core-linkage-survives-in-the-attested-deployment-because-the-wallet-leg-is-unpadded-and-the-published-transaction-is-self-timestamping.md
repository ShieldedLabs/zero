# The IP -> transaction -> amount join survives the product in the *attested* deployment: the unpadded wallet leg gives the operator `|tx|`, and the published transaction timestamps itself, so the delivered anonymity set is ~1 whatever the batch size

**Severity**: High
**Validation Status**: Confirmed
**Location**: Whole-system. Principal loci:
`audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:79-127` (in-enclave TLS termination, so the wallet's ciphertext is relayed across the parent host unpadded), `:186-201` (the `debug { }` block whose `ssh_keys` list zeronym asserts is inert);
`audit-target/zeronym/shim/src/intercept.rs:110-131` (divert returns before `pool.get()`, so the operator sees the *absence*), `:158-166` (the code naming `|tx|` as the secret and paying an availability cost to protect it on the next hop);
`audit-target/zeronym/shim/src/proxy.rs:744-783` (`route_for` — every sync method is `PassThrough`);
`audit-target/zeronym/hub/src/batcher.rs:40` (`FLUSH_INTERVAL_BLOCKS = 20`, a public deterministic schedule), `:55` (`MIN_WALLET_EXPIRY = 40`);
`audit-target/zeronym/hub/src/queue.rs:363-393` (`next_flush_height` / `survives_next_flush`), `:258-264` (shuffle);
`audit-target/zeronym/hub/src/chain.rs:199-210` (`broadcast_batch` — one simultaneous burst);
`audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:114-123` (the "SSH is closed under attestation" note);
`audit-target/zeronym/shim/deploy/caution/OPERATORS.md:59` (the operator owns the wallet-facing DNS name), `:64-69` (managed vs BYOC);
`audit-target/zeronym/deploy.sh:156` (`caution apps create`), `:162-176` (the DNS record the operator writes);
`audit-target/zeronym/README.md:27`, `:33`, `:34`, `:54`, `:69` (the claims); `audit-target/zeronym/OPEN-QUESTIONS.md:109` (the in-tree, undisclosed-to-users statement of the size channel)
**Found by agent:** Global, focus area G29 — "link wallet IP -> on-chain Orchard-touching transaction -> balance, using only what the operator sees"
**In scope of audit?** Yes

## Description

This is the composed form of the attack the product exists to prevent. It is
filed separately from its components because the composition supports a
conclusion none of them supports alone:

> **`README.md:34`'s residual — "the modal batch is zero or one... The lever is
> adoption, not code" — names a remedy that does not restore the protection.**
> Two channels re-identify an individual submitter *inside a batch of any size*,
> and both are readable by the primary adversary in a correctly attested
> deployment with `debug { enabled = false }` and a clean `caution verify PASSED`.

The two channels:

**(1) The wallet->shim leg is unpadded and carries `|tx|`.** Under the shipped
Caution manifest the wallet's TLS terminates on an in-enclave Caddy
(`caution.hcl.tmpl:79-127`). The enclave has no NIC: the parent host relays port
443 in over vsock with `socat`, so the wallet's ciphertext crosses the parent
byte for byte. TLS record length fields are cleartext in every TLS version, no
mainstream stack pads, and nothing in zeronym pads, chunks or normalises the
wallet-facing request. Anyone holding that socket reads the serialized length of
the transaction. The project pays a real availability cost on the *adjacent* hop
to deny the same reader the same number — `intercept.rs:161-166`: *"not fitting
the frame is the price of leaking zero bits of length... that number would
otherwise reach the parent host, which is the one reader D4 exists to keep it
from"* — and `wire.rs`'s fixed 64 KiB frame implements `hub/REVIEW.md` #12 for
exactly this reason. **The identical quantity is available for free one hop
earlier, to the same reader.**

**(2) The published transaction timestamps itself, to one block.** For
non-ZIP-318 traffic a wallet anchors near its own tip and sets
`nExpiryHeight = build_height + 40` — ZIP 203's Blossom default, and the same 40
the hub's `MIN_WALLET_EXPIRY` is derived from (`batcher.rs:47-55`).
`anchorOrchard` and `nExpiryHeight` are **public, immutable fields of the
published transaction**, fixed by the wallet before the shim ever sees it, so
neither the hub's queue nor its shuffle nor its batch can touch them. The
operator observes the divert at wall-clock time `T`, converts `T` to a block
height with their own node, and keeps the batch members whose expiry is
`h(T) + 40`. Batching cannot remove a timestamp the wallet baked in.

**Why this is not fixed by growing the batch.** A `W`-block flush window admits
`k ~ lambda*W` entries **and spans ~`W` distinct expiry values**, so the members
sharing the target's expiry number `1 + (k-1)/W ~ 1 + lambda`. `W` cancels
(`audit-state/globals/G3-anonymity-set-arithmetic.md`). At the README's own
measured `lambda = 0.77` Orchard-touching transactions per block, the delivered
set on channel (2) alone is **~1.77**, and channel (1) cuts it further and
independently. Raising `k` raises the *candidate* set and leaves *selection*
untouched.

**Both channels close only under the same unmet precondition.** A ZIP 318
conforming migration is length-uniform, carries a shared boundary anchor and a
shared bucketed expiry, so conformance closes (1) and (2) together.
`audit-state/SPEC-NOTES.md` §5 records that ZIP 318 is `Status: Draft`, that its
reference-implementation section is a literal `TODO`, and that no shipped wallet
has been shown to implement it. `README.md:69` states the anchor half of that
precondition only as a *wallet-developer* requirement and the length half not at
all, while `README.md:27` and `:33` affirm the protection unconditionally and
volume-independently.

## Attack Scenario and Steps

Attacker: **the operator of the indexer the shim fronts** — adversary #1, and the
party the product exists to defend against. Target: a wallet at source IP `X`.
Goal: `IP X -> the specific on-chain transaction -> the amount it moves`.

Every step below holds with `debug { enabled = false }`, all three PCRs
reproducing, and `caution verify` printing `Attestation verification PASSED`.

### Step 1 — obtain the wallet-leg byte stream

*Cost:* one manifest flag, one DNS edit, or nothing at all, depending on the
deployment shape.

The enclave has no NIC, so **every** wallet TLS session is relayed in by a socket
on the Nitro parent host. Whoever holds that socket sees the wallet's source IP,
connection timing and every cleartext TLS record length. `deploy.sh:156` runs
`caution apps create`, which `OPERATORS.md:64` defines as *"Fully managed: in
Caution's AWS account"* — so in the shipped deployment the operator does **not**
hold that socket by default. They reach it by any of three routes, none of which
changes a PCR and none of which any documented check catches:

- **1A — BYOC.** `OPERATORS.md:66-69` documents `caution init --byoc`, which puts
  the parent host in the operator's own AWS account. Then the position is free
  and inherent. `deploy.sh` does not use this path, but the runbook offers it as
  a first-class option and an operator who wants their traffic in their own
  account will take it.
- **1B — `debug { enabled = false; ssh_keys = [...] }`, and this is the sharpest
  one.** `assemble-caution.sh --ssh-key <k>` without `--debug` renders the key
  into the manifest and prints only *"SSH is closed when attestation is on, so
  the key is recorded in the HCL but unused until a `--debug` build"*
  (`assemble-caution.sh:120-123`); `caution.hcl.tmpl:196-198` repeats the claim.
  **The platform disagrees.** `src/api/src/main.rs:2420-2421` reads
  `debug.enabled` and `debug.ssh_keys` independently, and
  `src/api/src/deployment.rs:2159-2164` emits `ingress 22 from 0.0.0.0/0`
  whenever the key list is non-empty, with `user-data.sh:17-27` installing the
  keys on the parent. So one flag, in a fully attested deployment, gives the
  operator a root shell on the host that runs
  `socat TCP-LISTEN:443,reuseaddr,fork VSOCK-CONNECT:16:443`. `tcpdump` there is
  the whole of step 1. `ssh_keys` reaches only terraform, so **no PCR moves** —
  and since it is unmeasured, the tree the operator publishes as `app_sources`
  can carry `ssh_keys = []` while the deployment carries a key, and
  `caution verify` still reproduces every PCR.
- **1C — a layer-4 relay on the DNS name the operator already owns.**
  `OPERATORS.md:59` lists *"A DNS name you control for wallets"* as a
  prerequisite and `deploy.sh:162-176` rewrites that record on every deploy. An
  operator who points it at a TCP forwarder they run sees the wallet's IP,
  timing and record lengths while the TLS session still terminates inside the
  enclave. **Every documented verification step passes, and passes correctly**:
  `validate_attested_tls` (`src/cli/src/lib.rs:353-378`) compares the attested
  `certfp` against the leaf of the verifier's own connection, and under a pure
  forwarder that leaf is unchanged. The platform's one catching check,
  `dns_contains_deployment_ip` (`:261-277`), runs only on the raw-IP verify path
  (`:6902`, `:6930`), which `README.md:71` and `OPERATORS.md:178` never use — and
  attempting it without an out-of-band deployment address resolves the domain to
  the relay and compares it to itself. Filed separately as
  `operator-controlled-dns-permits-a-layer-4-relay-that-every-documented-verification-step-passes.md`.

(A fourth route, `DEBUG=1`, is `deploy.sh:52`'s shipped default and gives the
same shell — but it also zeroes the PCRs and makes `caution verify` refuse, so it
is a *different* scenario, already filed as
`deploy-script-defaults-to-debug-mode-which-turns-attestation-off.md`. This chain
deliberately does not use it.)

### Step 2 — recover `|tx|`

*Cost:* arithmetic on captured TLS record headers; no decryption.

TLS records are framed `type(1) || version(2) || length(2)` with the length in
**cleartext**, so for a connection carrying `n` records the observer computes the
application plaintext as `sum(length_i) - 17n` (16-byte AEAD tag plus the inner
content type), with `n` directly observed. The HTTP/2 plaintext of a
`SendTransaction` is `preface + SETTINGS + HEADERS + DATA`, where DATA is
`5-byte gRPC prefix + protobuf RawTransaction{ data: tx }` — i.e. `|tx|` plus a
small constant the operator calibrates once by sending a transaction of known
size through their own shim.

Two honest notes:

- **Direction and shape do most of the work.** A `SendTransaction` is the only
  large *client->server* upload a light wallet makes; every sync method is a small
  request with a large response. The broadcast burst is separable by inspection,
  not by subtraction.
- **Multiplexing does not defeat it, and precision is not needed.** If the wallet
  shares one h2 connection between sync and broadcast, concurrent sync *requests*
  add tens to hundreds of bytes. Orchard bundle size grows in steps of
  ~3.1 KB per action, so an error of a few hundred bytes never merges two
  candidates. On a dedicated broadcast connection — which ZIP 318's own
  sync-decoupling rule *requires* wallets to use — recovery is exact. **The
  specification's privacy rule sharpens this channel.**

### Step 3 — establish that the request was diverted

*Cost:* zero. *Reliability:* certain; conceded by the project as unfixable.

`intercept::send_transaction` (`intercept.rs:110-131`) returns through `divert`
**before** `pool.get()` is called, so a diverted transaction opens no TCP
connection to the backend. The operator sees a large wallet-leg upload with no
counterpart at their own indexer. `README.md:33`: *"A diverted request is the one
thing it does not see, and that asymmetry survives padding."* Size alone also
separates a diverted `SendTransaction` from the only other non-forwarded request
class, `GetTransaction`, whose body is capped at `MAX_TX_FILTER_BYTES = 1 KiB` (`intercept.rs:81`, `:249`).

### Step 4 — bound the candidate set to one flush

*Cost:* zero. *Reliability:* deterministic.

`queue::survives_next_flush` (`queue.rs:380-393`) admits an entry only if it
survives the next scheduled flush, and `next_flush_height` (`:363-368`) puts
flushes on multiples of `FLUSH_INTERVAL_BLOCKS = 20`. Flush heights are therefore
publicly computable; `batcher.rs:8-17` chooses this deliberately (*"the cost (the
schedule is public) is acceptable"*). `chain::broadcast_batch`
(`chain.rs:199-210`) issues the full (transaction x endpoint) product
concurrently, so the batch enters the mempool as one simultaneous burst any node
observes. The candidate set for a divert at time `T` is exactly the batch
published at the next multiple of 20 after `T`.

### Step 5 — resolve the candidate set to one transaction

*Cost:* one comparison per batch member.

- Keep the members whose `nExpiryHeight` equals `h(T) + 40` (or, for a wallet with
  a different delta, whose expiry is a fixed offset from `h(T)` — the offset is a
  per-wallet-implementation constant the operator learns once). Expected
  survivors: `1 + lambda ~= 1.77` at the README's own measured rate.
- Independently, keep the members whose serialized length matches step 2.
- `anchorOrchard` is a third, correlated selector for a latest-anchor wallet.

Together these are near-certain to be unique today. **They collapse to nothing
against a ZIP-318-conforming population** — that is the honest limit of the
finding, and it is also the reason it matters: the README states the protection
unconditionally while it is in fact contingent on a wallet behaviour the README
itself only *requests* at `:69` and that no shipped wallet implements.

At batch size 0-1, the measured condition (`README.md:34`), step 5 is
unnecessary — the batch *is* the answer.

### Step 6 — read the amount

*Cost:* zero. *Reliability:* certain, permanent, retrospective.

`valueBalanceOrchard` and the Ironwood value balance are public fields of the
published transaction. `README.md:54` states this as the premise of the whole
product. The join completes: **IP `X` -> this transaction -> this amount**, and it
can be run at any future date against archived captures and a permanent chain.

**Attack Requirements and Assumptions:**

- **Access needed:** being the operator, plus one of routes 1A/1B/1C. All three
  are things the operator either already has (the DNS record, by documented
  prerequisite) or obtains with one flag. Plus their own indexer logs and a Zcash
  node, both definitional.
- **What makes it realistic:** no exploit, no race, no privileged bug, nothing
  active on the wallet leg. The whole chain is passive and retrospective. Route
  1B in particular is a *documented* flag whose danger three zeronym texts
  explicitly deny.
- **What limits it, stated plainly:**
  1. Step 5 collapses against a ZIP-318-conforming population. The finding's
     force comes from that population not existing yet.
  2. The operator resolves only *their own* clients out of a batch; another
     operator's wallets are just "one of these is not mine".
  3. A wallet that routes its broadcast over Tor or Nym itself — which ZIP 318
     requires wallets to *offer* — defeats step 1 outright.
  4. In the shipped fully-managed deployment the operator must take one of the
     three actions in step 1; they do not hold the parent socket by default.
     This is a real threat-model correction, not a refutation: all three actions
     are free, undetectable by every documented check, and available to the
     adversary the product is built against.

## Impact on Users

Every user of every zeronym endpoint, for as long as their wallet does not
implement ZIP 318 — which is every wallet today, on a migration that is
mandatory, mass and concentrated.

A user reads `README.md:27` (*"Protected — **Source IP.** ... Volume-independent:
it holds however few others are migrating"*) and `:33` (*"the operator learns
*that* a client migrated, **though not the amount or which transaction**"*) and
concludes that broadcasting through a zeronym endpoint stops their indexer
operator linking their IP to their migration and its value. Against that
operator, in the attested deployment, it does not.

The result is the permanent, retrospective linkage the product's own Background
section calls *"the attack"*: IP address -> the specific on-chain transaction ->
the balance it moved. It is worse than a live leak because the chain is permanent
and the captures are archivable.

The behavioural harm is the one ICTM exists to catch. A user who believes the
claim migrates now, over a zeronym endpoint, and does not take the measure that
would actually have protected them — routing the broadcast over Tor or Nym at the
wallet. Neither the chain nor the operator's captures forget.

The finding also removes the comfort in the disclosed residual. `README.md:34`
tells the reader the weakness is a volume problem and that *"the lever is
adoption, not code"*. That is false for both channels: `W` cancels, so the
delivered set stays at ~`1 + lambda` however wide the window and however large
the batch. The lever for these two channels is **wallet conformance and
wallet-side padding** — code, in the wallet, that the README does not ask for.

## Technical Details / Code Analysis

**1. Nothing on the wallet-facing leg pads, and the manifest guarantees the
ciphertext crosses the parent host.**

`shim/deploy/caution/caution.hcl.tmpl:79-89`:

```
    # `e2e_encryption { enabled = true }` is Caution's in-enclave TLS
    # termination, shipped 2026-08-03. The platform runs a Caddy INSIDE the
    # enclave: it obtains the certificate for `domain`, terminates the wallet's
    # TLS in there, and forwards to our process on `port`. So the private key is
    # generated and held inside the enclave and the operator never holds it,
    # which is the property the whole attestation argument depends on.
```

and `:106-127` declares `http { domain … port = 8083; upstream_protocol = "h2c";
e2e_encryption { mode = "tls" } }`.

This is correct as far as it goes: the operator does not hold the key. But an
enclave has no NIC. The Caution parent relays 443 in with
`socat TCP-LISTEN:$port,reuseaddr,fork VSOCK-CONNECT:16:$port`
(`terraform/modules/aws/nitro-enclave/user-data.sh:197-215`), and TLS record
length fields are not encrypted. **The protection is of content, and `|tx|` is
not content.**

`shim/src/intercept.rs:94-131` buffers the request whole and either diverts or
replays it verbatim; there is no padding on either arm. The synthesized
wallet-facing replies also differ in length (`grpc_send_response` at `:220-227`
vs `grpc_error` on the three divert-failure arms at `:151`, `:173` and `:209`),
handing the same observer the *verdict* as well.

**2. The code states the exact secret being protected, and pays for it on the
other hop.** `shim/src/intercept.rs:158-166`:

```rust
        // Too large for the transport's fixed frame. RESOURCE_EXHAUSTED, not
        // UNAVAILABLE: this can never succeed, and UNAVAILABLE is the status
        // that tells a wallet to retry. It is never forwarded to the operator
        // and never broadcast another way; not fitting the frame is the price
        // of leaking zero bits of length.
        //
        // The log line carries the LIMIT, never the transaction's own size:
        // that number would otherwise reach the parent host, which is the one
        // reader D4 exists to keep it from.
```

Two sentences, both about `|tx|`, both naming the parent host as the reader to be
denied. The price is real: a transaction over `MAX_NYM_TX_BYTES = 65,503` is
permanently refused rather than leak its length. **The system pads and shapes the
hop whose observer is weakest, and leaves unpadded the hop whose observer is
adversary #1.**

`OPEN-QUESTIONS.md:109` is the only place in the repository this is written down:

> **Accepted non-defenses.** Active wallet-tagging ... and the transaction-size
> side channel (a distinctive migration size re-links via TLS ciphertext length)
> are out of scope by design. **Confirm these are acceptable**, or scope
> mitigations.

That sentence asks the security reviewer to ratify the acceptance. This issue is
the answer: it is not acceptable while `README.md:33` tells users the opposite and
`README.md:34` tells them adoption is the fix.

**3. The mixnet leg, for contrast, is genuinely defended — do not claim
otherwise.** Fixed 64 KiB `SubmitV1` frames (REVIEW #12) plus the nym client's own
send shaping mean a real submit *displaces* cover traffic rather than adding to
it. By the crate's own model (`shim/src/nym.rs:1089-1131`: `PACKET_BYTES = 2048`,
a shaped floor of ~8.33 packets/s, a submit = 45 packets, a lookup = 61) there is
no clean per-migration egress burst. This link was checked and refuted as a
channel and should be reported as a defence that works.

**4. Everything a wallet needs is forwarded to the operator, and the flush clock
is public.** `route_for` (`proxy.rs:744-783`) sends `GetLatestBlock`,
`GetBlockRange`, `GetSubtreeRoots`, `GetTreeState` and `GetLatestTreeState` to
`Route::PassThrough`. `shim/ENDPOINTS.md:135` names the consequence in the
project's own words:

> `GetLatestTreeState` ... **Anchor correlation, the strongest non-argument
> leak**: this supplies the Orchard anchor the wallet spends against, and that
> anchor root is a public field of the published tx.

One correction to how this is usually stated, established by the platform read:
the operator's *indexer* does not get a per-wallet boundary, because the parent
relays wallet connections into the enclave and Caddy multiplexes many of them
onto few upstream connections, so pass-through traffic is **not** attributable to
a source IP. **The chain does not need it to be.** Channel (2) works off the
operator's own node clock and the transaction's own `nExpiryHeight`; the divert
timestamp comes from the wallet leg in step 1, not from the indexer.

**5. Neither selector can be touched by the hub.** `anchorOrchard` is inside the
ZIP 244 `orchard_digest` and `nExpiryHeight` is a consensus field; both are fixed
by the wallet before the shim sees the transaction and are identical on chain
whether the hub published it or the wallet did. The queue keys on
`sha256(tx_bytes)` and `chain::broadcast_batch` publishes those bytes unmodified,
so the on-chain serialized length equals the `|tx|` measured in step 2.

**6. The stated remedy does not act on either channel.** Raising the batch size
from 1 to `k` raises step 4's candidate set from 1 to `k` and leaves steps 2 and
5 untouched. And widening the window does not help either: a `W`-block window
admits `k ~ lambda*W` entries *and* spans ~`W` expiry values, so the delivered set
is `1 + (k-1)/W ~ 1 + lambda` and `W` cancels — a 24-hour window delivers exactly
what a 1-block window delivers (`audit-state/globals/G3-anonymity-set-arithmetic.md`,
filed as `widening-the-flush-window-cannot-raise-the-delivered-anonymity-set-...md`).
The delivered anonymity set is not `k`; it is **the number of batch members
sharing the target's `(length, anchor, expiry)` tuple**, which for today's
heterogeneous Orchard traffic is ~1.

## Recommendations

1. **Correct `README.md`'s Security section so the documented property matches the
   delivered one.** Move the ZIP 318 conformance requirement from *Usage -> Wallet
   developers* into *Not protected*, and state that until wallets conform, an
   operator who observes the wallet leg can re-identify their own clients'
   transactions inside a batch of any size by serialized length, anchor and
   expiry. Replace *"The lever is adoption, not code"*: it is true of the
   batch-size residual and false of these two channels.
2. **Disclose the transaction-size side channel to users.** It exists today only
   at `OPEN-QUESTIONS.md:109`, in a list asking reviewers to confirm it is
   acceptable, while `README.md:33` tells users the opposite.
3. **Specify wallet-side padding as a second hard requirement, beside aligned
   anchors.** It can only be fixed there: the length is fixed by the wallet before
   any zeronym code runs. HTTP/2 `DATA` frames carry a `PADDED` flag, so a wallet
   can pad every `SendTransaction` to a fixed size (65,503 bytes, the hub's frame
   budget, is the natural target) with no protocol change.
4. **Close the three step-1 routes, which are the only part of this chain zeronym
   can fix in its own repository.** (a) Correct `assemble-caution.sh:120-123` and
   `caution.hcl.tmpl:196-198`, which assert that `ssh_keys` is inert under
   attestation — it is not — and make `--ssh-key` without `--debug` a hard error.
   (b) Add a DNS-target check to the auditor recipe: resolve the wallet-facing
   name and require it to reach the Caution deployment address obtained out of
   band, or invoke `caution verify` against the raw deployment IP so
   `dns_contains_deployment_ip` runs. (c) State in `THREATMODEL.md` §3 and in
   `README.md` which deployment model the live endpoints use, because managed and
   BYOC give the operator materially different positions.
5. **Do not attempt to fix this in the shim by padding the wallet-facing reply
   alone.** That closes the verdict oracle — worth doing on its own merits — but
   not the request-length channel, which is the one that matters.
6. **Measure the achieved anonymity set on the fields that discriminate.** Count
   distinct `(length, anchor, expiry)` tuples in a batch, not batch cardinality;
   `batcher::flush`'s `achieved <= 1` warning counts the wrong thing.
7. **Sequence the fixes.** Widening the flush window is null before wallet-side
   expiry bucketing and useful only after it; do not ship it as a standalone
   mitigation.

## Validation Information

**Verdict: CONFIRMED. Severity: High** (filed High; upheld).

Every mechanical link was re-verified against the target and, for the platform
claims, against the Caution platform source (`codeberg.org/caution/platform`,
whose location `shim/deploy/caution/OPERATORS.md:44-46` names, cloned during this
audit) rather than against prose.

**Verified in `audit-target/zeronym/`:**

- `caution.hcl.tmpl:79-127` — in-enclave Caddy, `mode = "tls"`, `h2c` upstream.
  Confirmed verbatim.
- `intercept.rs:110-131` — divert returns before `pool.get()`; `:158-166` — the
  "zero bits of length" comment naming the parent host. Confirmed verbatim.
- `proxy.rs:744-783` — every sync method falls to `Route::PassThrough`. Confirmed.
- `batcher.rs:40,47-55` — `FLUSH_INTERVAL_BLOCKS = 20`, `MIN_WALLET_EXPIRY = 40`
  derived from librustzcash's default. `queue.rs:363-368` — flushes on multiples
  of 20. `chain.rs:199-210` — `join_all` over the full product, "simultaneity is
  the property". All confirmed.
- `README.md:27,33,34,54,69` and `OPEN-QUESTIONS.md:109` — quoted accurately.
- `deploy.sh:156` — `caution apps create`; `:162-176` — the operator writes the
  wallet-facing CNAME on every deploy. Confirmed.

**Verified in the Caution platform source (step 1):**

- `terraform/modules/aws/nitro-enclave/user-data.sh:197-215` — 443 is added to
  `standard_ports` under `e2e_mode == "tls"` and relayed with
  `socat TCP-LISTEN:443,reuseaddr,fork VSOCK-CONNECT:16:443`. **The wallet's
  ciphertext demonstrably crosses the parent host.**
- `src/api/src/main.rs:2420-2421` — `debug_enabled` and `ssh_keys` are read as
  two independent fields; `src/api/src/deployment.rs:2159-2164` — the terraform
  template emits `ingress { from_port 22 ... cidr_blocks ["0.0.0.0/0"] }` iff the
  key list is non-empty; `user-data.sh:17-27` installs them on the parent.
  **Route 1B is confirmed from source, and zeronym's own text
  (`assemble-caution.sh:120-123`, `caution.hcl.tmpl:196-198`) asserts the
  opposite.** `ssh_keys` reaches only terraform, so no PCR moves.
- `src/cli/src/lib.rs:353-378` (`validate_attested_tls`) — the certfp check
  compares the attested fingerprint against the leaf of the verifier's own
  connection. Under a pure layer-4 forwarder that leaf is unchanged, so the check
  **passes, and passes correctly**: it answers "did my TLS session terminate in
  the attested enclave?", to which the answer under a relay is genuinely yes.
  `:222-240` + `:6902` + `:6930` — `dns_contains_deployment_ip` runs only when
  the verifier supplies a raw deployment IP, which the documented recipes never
  do. **Route 1C is confirmed.**

**Threat-model correction applied to the filed text.** The draft granted step 1
to the audit's standing premise that the operator owns the parent host. That
premise is model-dependent: `deploy.sh` ships the fully-managed model, where the
parent is in Caution's AWS account. Step 1 has been rewritten as three explicit
routes and the chain now states exactly which deployment shapes it holds in:

| deployment shape | step 1 | chain |
|---|---|---|
| managed + attested, operator takes no action | not held | **does not complete** |
| managed + attested + `debug.ssh_keys` (one flag, no PCR change) | held | **completes** |
| managed + attested + operator L4 relay on their own DNS name | held | **completes** |
| BYOC + attested | held, free | **completes** |
| `--debug` (`deploy.sh:52` default) | held | completes, but attestation is off — different scenario, separately filed |

This is a *strengthening* of the finding's practical status, not a weakening:
before this correction, parent-host access looked like an inherent property of
Nitro with no fix. It is instead an operator action, undetectable by every
documented check, and two of the three routes are things zeronym's own repository
can close (recommendation 4).

**Two corrections made to the filed technical argument, both against the issue:**

1. *"The operator holds the plaintext of every non-diverted exchange on that
   connection and subtracts"* was too strong as a per-wallet claim. The operator's
   indexer receives multiplexed connections from the enclave with no per-wallet
   boundary and no client address, so pass-through requests are not attributable
   to a source IP. The step has been restated on the mechanism that actually
   works: a `SendTransaction` is the only large client->server upload a light
   wallet makes, so it is separable by direction and shape, and residual error
   from concurrent sync requests is a few hundred bytes against a ~3.1 KB
   per-action granularity. Recovery is exact on a dedicated broadcast connection,
   which ZIP 318's sync-decoupling rule requires.
2. *"Match anchor and expiry against the per-IP sync tip known from pass-through
   traffic"* rested on the same wrong premise. The channel is stronger without
   it: `nExpiryHeight = build_height + 40` is a self-published, one-block
   timestamp, matched against the divert time the operator reads off the wallet
   leg using their own node's clock. No per-IP sync attribution is needed.

**Quantification added.** The delivered anonymity set on the expiry channel alone
is `1 + (k-1)/W ~ 1 + lambda`, i.e. **~1.77** at the README's own measured
0.77 Orchard-touching transactions per block, **independent of the window `W` and
of the batch size `k`**. Per coordinator item 6s, "widen the window" must not be
offered as a mitigation: the window provably cancels.

**False-positive checks applied.**

- *§7 Configuration-dependent?* No. Routes 1B and 1C are not insecure
  configurations a well-meaning operator stumbles into; they are deliberate acts
  by the adversary the product names first, and 1C uses a DNS record
  `OPERATORS.md:59` makes a prerequisite. 1B is aggravated rather than excused by
  configuration, because three zeronym texts tell the operator (and any reviewer)
  that the flag is inert.
- *§8 Requires prior compromise?* No. Nothing here requires reaching inside the
  enclave. Every capability used is one the operator has by construction or
  acquires with one flag.
- *§3 Information disclosure overstated?* No. The disclosed value is a specific
  user's shielded-transaction amount joined to their IP address — the exact harm
  `README.md:54` defines as "the attack".
- *§6 Intentional design?* Partly, and it is stated as such: the absence signal
  (`README.md:33`) and the batch-size residual (`:34`) are disclosed. What is
  **not** disclosed is that selection inside a batch defeats the disclosed
  remedy, and that the size channel exists at all outside `OPEN-QUESTIONS.md`.

**Why High and not Critical:** no funds are stolen or destroyed, the attacker must
be the endpoint's own operator, and a ZIP-318-conforming wallet population would
close both channels. **Why High and not Medium:** the harm is the product's
single reason to exist, it lands on every user of an affected endpoint, it is
passive, free and retrospective against archived captures and a permanent chain,
the three step-1 routes are all available to adversary #1 without detection, and
the project's own stated remedy ("adoption, not code") provably does not act on
it.

**Deliberately not merged, to avoid double-counting.** This chain uses *no*
defect in the classifier, the queue, the batcher or the enclave boundary. The
operator's stronger options — appending their own hub
(`shim-submits-every-migration-to-every-configured-hub-...md`, confirmed High) and
the `DEBUG=1` log leak (`log-verdict-logs-migration-value-balance-at-info.md`,
confirmed High) — are cited as context, not folded in; the report should present
this as the chain that survives when those are fixed, and should not stack the
three severities as three separate losses of the same secret.


---

## ADDENDUM (Global auditor, focus area G4 — parent-host side channels, 2026-08-18). NOTHING ABOVE IS WITHDRAWN OR CHANGED. This is a **completeness correction to the remediation**, plus the disposition of a `[***]` brainstorm item that was explicitly deferred to the G4 pass.

**Padding the wallet-leg REQUEST is not sufficient. The four synthesized
wallet-facing REPLIES are unpadded too, they are distinguishable by length, and
they are read by the same socket on the same connection.**

The obvious fix for the channel this issue documents is to pad or chunk the
wallet→shim request so `|tx|` stops being recoverable from TLS record lengths.
That fix leaves a second, smaller channel in place on the return leg, and the
return leg is entirely under the shim's control — it *synthesizes* every reply on
the divert path, so unlike the request it can be made constant-size with no wallet
change at all.

`shim/src/intercept.rs:137-213` (`divert`) produces exactly four wallet-facing
shapes, each a different number of bytes on the wire:

| divert outcome | reply | body bytes on the wire |
|---|---|---|
| `Submit::Accepted { txid }` | `grpc_send_response(0, <64-hex txid>)` | 5-byte gRPC prefix + 66-byte `SendResponse` (`errorCode` = 0 is a proto3 default and is omitted; `errorMessage` is tag+len+64) = **71** |
| `Submit::AlreadyKnown { txid: None }` | `grpc_send_response(0, "")` | both fields default → empty message = **5** |
| `Submit::Rejected { reason }` | `grpc_send_response(-1, reason)` | `errorCode` = -1 encodes as a 10-byte varint, so ≥ **16** plus the reason |
| body unreadable / hub unreachable / too large | `grpc_error(...)`, trailers-only | **0** body, with `grpc-message` of 47, 34 and 69 characters respectively in the HEADERS frame (`intercept.rs:150-154`, `:207-212`, `:170-178`) |

Scope, stated honestly so the report does not overclaim: this does **not**
distinguish a divert from a pass-through — a successful pass-through returns the
backend's own `SendResponse`, which is also a 71-byte body — and the `TooLarge`
arm's information (`|tx| > 65503`) is already implied by the request-length
channel this issue is about. What the reply lengths add is the **divert
outcome**: whether the migration was carried or destroyed, per wallet, per
attempt. Against today's code that is a small increment on a channel the same
reader already has, which is why it is recorded here rather than filed
separately (items 8/9 double-counting precedent). Against the *fixed* code it is
the whole channel, which is why it must be part of the fix.

**Recommendation, to be applied together with this issue's existing ones:** pad
every synthesized wallet-facing reply on the intercepted paths to one constant
size — the same discipline `shim/src/wire.rs` already applies to the mixnet
frames for exactly this reader, and the discipline `shim/src/intercept.rs:156-166`
already cites as the reason it refuses an over-64-KiB migration rather than
leaking its length. Concretely: give `grpc_send_response` and the divert-path
`grpc_error` arms a common fixed-length `errorMessage`/`grpc-message` envelope, so
`Accepted`, `AlreadyKnown`, `Rejected` and all three failure arms are
byte-identical in length. This is a change inside one function and needs no
wallet cooperation.

**Disposition recorded so nobody re-chases it:** this closes `BRAINSTORM.md`'s
`[***]` item "The four synthesized wallet-facing responses have distinguishable
sizes, giving the parent host a verdict oracle on the diverted path", which was
explicitly deferred to the parent-host side-channel global pass. It is disposed
of **into this issue's remediation**, not filed as a separate finding.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

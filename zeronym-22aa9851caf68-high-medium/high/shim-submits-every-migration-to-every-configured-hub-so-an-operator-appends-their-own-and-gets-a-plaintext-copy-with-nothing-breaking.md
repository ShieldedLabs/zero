# `NymHandle::submit` sends every diverted transaction to **every** address in `ZIS_HUB_NYM`, so an operator appends a hub they run and receives a real-time plaintext copy of every migration while the canonical hub keeps publishing normally — nothing breaks, and the one check the audit recommends elsewhere does not catch it

**Severity**: High
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/nym.rs:595-689` (`NymHandle::submit`, the fan-out), `:602`, `:618-634` (the code's own statement of what makes it safe); `audit-target/zeronym/shim/src/nym_driver.rs:180` (`targets` = number of configured addresses); `audit-target/zeronym/shim/src/config.rs:65-77` (`ZIS_HUB_NYM` is an unbounded list) and `:262-289` (the only validation); `audit-target/zeronym/shim/src/hub.rs:228-240` (the divert call site); `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:228-235` (per-entry shape check, no cap) and `:345-356` (the rendering, and a comment that misdescribes the fan-out); `audit-target/zeronym/README.md:90` (the public claim that submit *rotates*); `audit-target/zeronym/README.md:70` (operators may run a hub)
**Found by agent:** Global, focus area G30/G32/G17 — the indexer operator's full capability sweep after the `unit.env`/certfp reversals
**In scope of audit?** Yes — `shim/src/nym.rs` is priority area 4, `README.md` claims are in scope as security claims, and `AUDIT-INSTRUCTIONS.md` unverified lead #10 names send-to-all-targets directly.

## Description

`ZIS_HUB_NYM` is a comma-separated **list** of hub Nym addresses, and
`NymHandle::submit` sends the transaction to **all** of them
(`shim/src/nym.rs:602`, `:642`):

```rust
        // Send to EVERY configured hub address, not one (REVIEW #6).
        …
        for target in 0..targets {
```

The list exists for failover: a diskless hub mints a new Nym address on every
restart, so a shim carries "the current address and the one it just rotated away
from" (`shim/src/config.rs:70-75`, `shim/src/nym.rs:622-626`). The code is
explicit that this is only safe under that assumption
(`shim/src/nym.rs:622-634`):

> Sending to every address is therefore safe **only while the other addresses
> are DEAD** … If two hubs were ever live at once, both would broadcast the same
> transaction. On-chain that is harmless (the second is a known txid), but it
> would publish the migration in two different batches at two different moments
> … and **it doubles the number of enclaves holding the plaintext**. Running a
> hot standby therefore needs an explicit passive mode in the hub, which does not
> exist today.

Nothing enforces the assumption. `Config::hub_selection` rejects only malformed
entries and exact duplicates (`shim/src/config.rs:276-282`); there is no cap on
the count, no notion of a primary, and no way for the shim to know whether an
address is live. `assemble-caution.sh:228-235` applies the same shallow shape
check per entry and never counts them.

The operator writes that list. So an operator who wants the plaintext does not
need to *repoint* the shim at a hostile hub — a repoint is a functional change,
and a hub that fails to batch or publish correctly produces visible breakage
that a user, a wallet developer or a monitoring third party could notice. They
**append**:

```
ZIS_HUB_NYM = "<canonical Shielded Labs hub>,<a hub the operator runs>"
```

Every diverted transaction is then framed twice, with a fresh nonce each time
(`shim/src/nym.rs:645`), and dispatched to both. The canonical hub queues,
batches, shuffles and publishes exactly as before, so **the wallet's transaction
confirms on the normal cadence and nothing anywhere behaves differently**. The
operator's hub receives the identical `SubmitV1` frame, decodes it, and holds the
raw transaction in plaintext — which is what a hub is *for*; it needs no
modification, no vulnerability and no enclave. It is `zero-indexer-hub`, running
on a VPS, from the public repository.

Two independent harms follow, and the second does not need the operator to know
any wallet's IP address:

1. **Real-time per-migration content disclosure to adversary #1.** The operator
   holds the full transaction bytes of every Orchard-touching transaction their
   users send, milliseconds after the wallet sends it — the exact thing
   `THREATMODEL.md` P1 says they do not receive and `README.md:83` says "goes to
   the hub instead of the operator".
2. **Anonymity-set subtraction against every *other* operator's users.** The
   batching design's whole product is that a published batch is the union of many
   operators' migrations and nobody can attribute a member. An operator with a
   listening hub knows precisely which members of every published batch came from
   *their own* shim, and can subtract them. For the other operators' users the
   effective anonymity set shrinks by exactly that number, permanently and
   retrospectively, with no signal to anyone. This is the SYBIL set-subtraction
   attack in `THREATMODEL.md` §3, executed for free by a party the design already
   admits to the system.

And when the operator also holds the wallet leg — which they do, either by owning
the Nitro parent host (BYOC) or by the layer-4 relay on the DNS record they
control (`operator-controlled-dns-permits-a-layer-4-relay-that-every-documented-verification-step-passes.md`)
— the two halves compose into the product's headline threat with no analysis
required: the wallet's source IP and connection timestamp on one side, the exact
transaction bytes arriving at their own hub milliseconds later on the other,
matched by time and by length. No batch reasoning, no chain join, no statistics.

## Attack Scenario and Steps

Attacker: the indexer operator. `README.md:70` already contemplates that they
*"run the shim in front of their indexer, and **optionally a hub**"*.

1. The operator runs `zero-indexer-hub` — the published binary, unmodified, on
   an ordinary VM — and reads its address from `GET /nym-address`. The hub has
   no submitter allow-list and no authentication by design, so it will accept
   frames from any shim, including their own.
2. In `deploy.env` they set
   `HUB_NYM="<canonical hub>,<their hub>"`. `deploy.sh:113` passes it through as
   `--hub-nym`; `assemble-caution.sh:228-235` accepts both entries;
   `:352` renders `ZIS_HUB_NYM = "<canonical>,<theirs>"` into `unit.env`.
3. They deploy attested, publish the app-source, and run `caution verify`. All
   three PCRs reproduce, the TLS certificate binding verifies, and
   `✓ Attestation verification PASSED` prints — correctly. The shim is the
   genuine shim; it is simply configured with two hubs, which is a configuration
   the code, the CLI and the runbook all accept.
4. Every wallet that migrates through this endpoint has its transaction
   delivered to the canonical hub **and** to the operator's hub.
5. The canonical hub publishes on the normal 20-block cadence. The migration
   confirms. The operator's hub also publishes it; the network answers "already
   known" and `classify_publish_error` records that as achieved, so even the
   duplicate broadcast produces no error anywhere. Because both hubs use the same
   `FLUSH_INTERVAL_BLOCKS = 20` cadence, the two publications are essentially
   simultaneous, so a chain observer sees nothing anomalous either.
6. The operator reads the plaintext off their own hub — from `POST /transaction`
   (`hub/src/queue.rs:328-348`, `Queue::find_by_txid`, which returns the raw
   transaction bytes of an entry that has not yet been published), or simply from
   a one-line patch, since it is their process on their own machine. (Not from the
   hub's logs: `hub/src/server.rs:269` logs `parseable = <bool>` and nothing else,
   which is the counts-only discipline working as intended.)

**Attack Requirements and Assumptions:**

- **Access needed:** the operator's own configuration, plus one VM. No enclave,
  no Caution account for the second hub, no code change to either binary, no
  mixnet position, and no vulnerability.
- **Detectability — the important part, stated precisely.** The value **is**
  measured into PCR0/PCR1 (open item 6q) and **is** served verbatim in
  `.manifest.run_command` of every `/attestation` response, so this is
  detectable *in principle*. But:
  - No zeronym document tells anyone to read `ZIS_HUB_NYM`. This is already
    filed as
    `auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`.
  - **The remediation that issue recommends does not catch this.** Its
    recommendation 2 is to publish the canonical hub address so that checking
    becomes "a string comparison rather than a judgement call". A checker asking
    *"does `ZIS_HUB_NYM` name the canonical hub?"* answers **yes** for an
    appended list. Only exact equality of the whole list catches it, and only if
    the checker knows that a list of two is not the ordinary failover state the
    code documents as normal.
  - **The public documentation actively points the wrong way.**
    `README.md:90` states: *"multi-hub failover. The shim **rotates which hub
    address each submit targets**"*. That is false for `submit` — rotation
    (`each_target`, `shim/src/nym.rs:733-760`) applies to *lookups*; submits go
    to all addresses unconditionally. A reader who checks the README before
    judging a two-address list concludes each migration reaches one hub.
    `assemble-caution.sh:351` repeats the error inside the manifest the auditor
    reads: *"The driver tries each address until one acks."* Submit awaits no
    ack at all.
  - A two-address list is exactly the shape the design says to expect
    (`shim/src/config.rs:70-75`), so its presence is not itself suspicious.
- **What makes it realistic:** it is the *cheapest* way for an operator to
  obtain migration plaintext, and uniquely it has **no functional side effect** —
  unlike repointing, it cannot break a user's transaction, cannot be caught by
  any smoke test, and leaves the system's end-to-end behaviour identical.
- **What limits it:** the operator's own hub is a second enclave-less process, so
  the transaction plaintext also sits outside any TEE; if that is discovered it is
  unambiguous. And an auditor who reads the manifest *and* knows to require exact
  equality does catch it.

## Impact on Users

For every wallet using an endpoint configured this way:

- The operator obtains the complete bytes of every Orchard-touching transaction
  the wallet sends, at the moment it sends it. `THREATMODEL.md` P1 ("OPERATOR
  does not receive the contents of your Orchard-touching transaction") and
  `README.md:83` ("goes to the hub instead of the operator") do not hold.
- `THREATMODEL.md` C2 — *"The party that sees your transaction in the clear and
  the party that sees your IP address are two different parties"* — is the
  invariant this destroys most directly, and it is the one the whole two-component
  architecture exists to create.
- With the wallet leg (parent host or DNS relay), the IP → transaction → amount
  linkage is direct and certain rather than statistical.

For **every other operator's users**, whose wallets never touched this endpoint:
the anonymity set of every published batch shrinks by the number of members this
operator contributed, because those members are known to them and can be
subtracted. At the project's own measured rate (0.77 Orchard-touching
transactions per block, modal batch 0–1) that is frequently the whole batch.

## Technical Details / Code Analysis

### The fan-out

`shim/src/nym.rs:595-601`:

```rust
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<(), NymError> {
        let targets = self.targets.load(Ordering::Relaxed);
        if targets == 0 {
            // No hub address to send to: nothing was dispatched. Fail closed.
            return Err(NymError::TransportGone);
        }
```

`shim/src/nym.rs:639-676`, the loop, in full:

```rust
        let deadline = tokio::time::Instant::now() + self.dispatch_timeout;
        let mut dispatched = 0usize;

        for target in 0..targets {
            // A FRESH nonce per address: two hubs answering the same nonce would be
            // indistinguishable to the correlator, and the ack is unread anyway.
            let nonce = fresh_nonce();
            …
            let frame = wire::encode_submit(&nonce, tx_bytes).map_err(NymError::Encode)?;
            let (ack_tx, _drop_receiver) = oneshot::channel();
            let request = Request {
                nonce,
                frame,
                reply_surbs: SUBMIT_REPLY_SURBS,
                waiter: Waiter::Ack(ack_tx),
                target,
            };
            match tokio::time::timeout_at(deadline, self.requests.send(request)).await {
                Ok(Ok(())) => dispatched += 1,
                Ok(Err(_)) | Err(_) => break,
            }
        }
```

`targets` is set once from the configured list
(`shim/src/nym_driver.rs:180`):

```rust
    targets.store(hub_addresses.len(), Ordering::Relaxed);
```

so `targets` is exactly the number of comma-separated entries in `ZIS_HUB_NYM`.
`tx_bytes` is the same buffer for every iteration; each address receives the
whole transaction.

### The only validation

`shim/src/config.rs:272-286`:

```rust
                let mut seen: Vec<&str> = Vec::new();
                for addr in &addresses {
                    if !is_nym_address(addr) {
                        return Err(ConfigError::MalformedNymAddress((*addr).to_owned()));
                    }
                    if seen.contains(addr) {
                        return Err(ConfigError::DuplicateNymAddress((*addr).to_owned()));
                    }
                    seen.push(addr);
                }
                Ok(HubSelection::Nym(
                    addresses.iter().map(|addr| (*addr).to_owned()).collect(),
                ))
```

`is_nym_address` is a shape check for `identity.encryption@gateway`
(`shim/src/config.rs:297-315`). Distinct well-formed addresses are accepted in
any number.

### The deploy tooling agrees, and misdescribes the result

`shim/deploy/caution/assemble-caution.sh:228-235`:

```sh
	OLDIFS=$IFS; IFS=','
	for addr in $HUB_NYM; do
		case "$addr" in
			?*.?*@?*) : ;;
			*) echo "error: --hub-nym entry '$addr' is not identity.encryption@gateway" >&2; exit 2 ;;
		esac
	done
	IFS=$OLDIFS
```

and `:345-352`, which writes both the value and the incorrect gloss into the
manifest an auditor reads:

```sh
	# ZIS_HUB_NYM is the address list; the driver picks a live one and fails over
	# (D10). …
	{
		printf '\n      # Divert Orchard-touching transactions over the Nym mixnet to these hub\n'
		printf '      # addresses. The mixnet is the confidentiality boundary; there is no TLS\n'
		printf '      # name to verify on this hop. The driver tries each address until one acks.\n'
		printf '      ZIS_HUB_NYM = "%s"\n' "$HUB_NYM"
```

*"picks a live one and fails over"* and *"tries each address until one acks"* are
both descriptions of `each_target` (the **lookup** path,
`shim/src/nym.rs:733-760`), not of `submit`. `submit` neither picks nor waits.
`PROVENANCE` likewise records only `hub(s): $HUB_NYM`
(`assemble-caution.sh:595`) with no comment on multiplicity.

### Why the second hub is invisible downstream

- **Dedup is per-hub.** `shim/src/nym.rs:618-621` states it: each hub
  deduplicates its *own* queue on the payload hash; there is no cross-hub
  dedup, and the hub "has no notion of being active or standby, so any hub that
  RECEIVES a migration will queue and broadcast it."
- **The duplicate broadcast is benign and silent.** Both hubs flush on heights
  ≡ 0 mod `FLUSH_INTERVAL_BLOCKS` (20), so the two publications land at the same
  boundary; whichever is second gets an "already known" response, which
  `chain::classify_publish_error` maps to `AlreadyKnown` and `batcher::flush`
  counts as achieved.
- **The shim's own telemetry cannot show it.** `/healthz` and `/nym-status` are
  mixnet-client lifecycle only; the shim's per-migration log line reports
  `accepted = error_code == 0`, which on the mixnet path is always true.
- **The wallet cannot show it.** The txid it is shown is computed by the shim
  from its own bytes (`THREATMODEL.md` N2), and the transaction confirms.

## Recommendations

1. **Make multi-hub an explicit, loud decision rather than a silent one.** At
   startup, if `hub_selection()` yields more than one Nym address, emit a
   `warn!` naming every address and stating that **each migration is sent to all
   of them in plaintext**. Better: require an explicit
   `--allow-multiple-hubs` / `ZIS_ALLOW_MULTIPLE_HUBS=true` before accepting a
   list of length > 1, so that the failover shape the design intends is chosen
   deliberately and appears in the measured `unit.env` where an auditor can see
   the intent as well as the addresses.
2. **Correct `README.md:90` and `assemble-caution.sh:345`/`:351`.** Submits go to
   every address; only lookups rotate. Both texts currently tell a reader the
   opposite, and both are the texts someone would consult when judging a
   two-address list.
3. **Publish the canonical hub address *and* state that the whole list must
   equal it.** This is the strengthening of recommendation 2 of
   `auditor-recipe-omits-…`: the check must be exact-list equality, not
   membership. Add it to `README.md:71` and to both `OPERATORS.md` "Verify"
   sections, as:
   `curl -sX POST https://<domain>/attestation -d '{"nonce":"…"}' | jq -r '.manifest.run_command' | grep ZIS_HUB_NYM`
   with the expected line published verbatim.
4. **Implement the passive/standby mode the code says is missing**
   (`shim/src/nym.rs:633-634`), so the failover list can be satisfied by one
   *active* address and the fan-out stops being the mechanism. This removes the
   capability rather than documenting it.
5. Consider having the hub refuse to broadcast a transaction it can see is
   already published, so a second live hub is at least detectable on chain — a
   weaker measure than 4, listed because it needs no shim change.

Cross-references:
`auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`
(the missing check; this issue shows its recommended form is insufficient);
`shim-config-hub-identity-is-unattested-unobservable-operator-configuration.md`
(the repoint variant, with its post-reversal correction);
`nym-submit-fanout-always-starts-at-address-zero-and-reports-success-on-a-partial-sweep.md`
(the same function, for *partial* fan-out causing loss — disjoint from this);
`operator-controlled-dns-permits-a-layer-4-relay-that-every-documented-verification-step-passes.md`
(the wallet-leg half of the composition).

## Validation Information

**Validated 2026-08-18. CONFIRMED at High.** Every load-bearing claim was
re-derived from the audit target and, for the platform half, from the Caution
platform's own public source (cloned during this audit from
`codeberg.org/caution/platform`, `git.distrust.co/public/bootproof.git`, and
`aws-nitro-enclaves-image-format 0.4.0`). This issue was written *after* the
`unit.env`/certfp reversals and was re-tested against them specifically, because
those reversals invalidated the sibling repoint finding.

### 1. The fan-out is real, and it is unconditional

Verified line by line in `shim/src/nym.rs:595-689`. `submit` reads
`self.targets` (set once at `shim/src/nym_driver.rs:180` to
`hub_addresses.len()`) and loops `for target in 0..targets`, framing the same
`tx_bytes` with a fresh nonce for each and handing every frame to the driver.
There is no primary, no ack wait, and no early exit on success — the only `break`
arms are transport failure. `shim/src/nym_driver.rs:613` resolves `out.target`
into `hub_addresses[target]` and sends anonymously.

Verified that `each_target` (`shim/src/nym.rs:733-784`), the *rotating* sweep, is
called from exactly one place — `get_transaction` at `:696`. **Submits never
rotate.** This is the fact that makes `README.md:90` and
`assemble-caution.sh:345`/`:351` false, and both were read in place.

### 2. The list is uncapped at every layer

- `shim/src/config.rs:65-77`: `ZIS_HUB_NYM` is `Vec<String>` with
  `value_delimiter = ','`.
- `shim/src/config.rs:250-289` (`hub_selection`): shape check via
  `is_nym_address` plus a reject-exact-duplicates pass. No cap, no primary, no
  identity check. Confirmed by reading the function in full.
- `shim/deploy/caution/assemble-caution.sh:228-235`: per-entry
  `case "$addr" in ?*.?*@?*)` shape check inside a comma-split loop. No count,
  no cap.
- `deploy.sh:113`: `HUB_NYM` is passed straight through as one `--hub-nym`
  argument.

### 3. Nothing breaks downstream — re-verified

- `hub/src/chain.rs:513-533` (`classify_publish_error`) folds hyphens and maps
  `already known` / `already in mempool` / `already in block chain` / `duplicate`
  to `Publish::AlreadyKnown`, and `hub/src/chain.rs:94` states that
  `AlreadyKnown` is a success. The hub's own test at `:559-574` is named
  `duplicate_submissions_are_success_not_failure` and its comment reads
  *"Every shim submits to every hub, so duplicates are normal operation."*
- The hub has no submitter ACL on the Nym ingress (`hub/src/nym.rs:305-343`
  decodes a `SubmitV1` and calls `hub.admit` with no notion of who sent it),
  which is a stated design property, so the operator's second hub accepts their
  own shim's frames without modification.

### 4. THE KEY TEST — does the attack survive the `unit.env` reversal? **Yes, completely.**

The reversal is real and was re-derived here from source rather than taken on
trust:

- `src/caution-config/src/lib.rs:233-274` — `UnitConfig::run_command_string()`
  emits `export KEY=<shlex-quoted value>` for every `Expression::String` entry in
  `unit.env`.
- `src/api/src/main.rs:2413-2417` → `src/enclave-builder/src/build.rs:355-361`,
  `:468` — that string becomes `{{USER_CMD}}` in the generated `run.sh`.
- `src/enclave-builder/templates/Containerfile.eif` — `COPY run.sh /build/run.sh`,
  `RUN cp /build/run.sh /build/initramfs/run.sh`, `cpio … | gzip > rootfs.cpio.gz`,
  then `eif_build … --ramdisk /build/rootfs.cpio.gz` with **exactly one**
  `--ramdisk`.
- `aws-nitro-enclaves-image-format-0.4.0/src/utils/mod.rs:660-691` — that ramdisk
  is written into `image_hasher` (PCR0) and, being index 0, into
  `bootstrap_hasher` (PCR1).

So appending an address **does** change PCR0/PCR1. **That changes nothing about
this attack**, for one decisive reason: `deploy.sh:206-219` publishes the
*deployed* tree — the one containing `ZIS_HUB_NYM = "<canonical>,<theirs>"` — to
`APP_SOURCE`, and `caution verify` reproduces PCRs from *that* tree. The
measurement therefore reproduces exactly and `✓ Attestation verification PASSED`
prints (`src/cli/src/lib.rs:7255-7305`, read in full). **The measurement
discloses the appended address; it never detects it.** Detection requires a human
to read a value and compare it against a reference — and see §5.

The value is disclosed in three places, all of which were confirmed: the
published `caution.hcl` in the app-source tree; `.manifest.run_command` in every
`/attestation` response (`bootproofd`'s `NoncedAttestationResponse` carries the
build manifest alongside the signed document); and `PROVENANCE`
(`assemble-caution.sh:595` writes `hub(s): $HUB_NYM`, plural, without comment).
None of these is read by any check the project runs or documents.

### 5. The distinguishing result: this defeats the fix proposed for the sibling issue

`auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-…`
recommends publishing the canonical hub address so the check becomes a string
comparison. Confirmed that this does **not** catch an appended list, and for
three independent reasons rather than one:

1. A *membership* test ("does `ZIS_HUB_NYM` name the canonical hub?") answers
   **yes**. Only exact whole-list equality catches it.
2. A two-address list is the documented normal state, twice over.
   `shim/src/config.rs:70-75` gives two separate innocent reasons for a list —
   *"a diskless hub mints a new address on every restart, so shims carry the
   current and the just-rotated one at once"* **and** *"one hub is hosted at
   several gateways for uptime"*. A reviewer seeing two entries has two
   documented benign explanations before they reach a hostile one.
3. **The documentation tells the reviewer the wrong thing about what a list
   means.** `README.md:90` — *"multi-hub failover. The shim rotates which hub
   address each submit targets"* — is false for `submit`, verified above.
   `assemble-caution.sh:351` writes *"The driver tries each address until one
   acks"* into the manifest the auditor reads, which is also false for `submit`
   (no ack is awaited at all). A reader who checks either text before judging a
   two-address list concludes each migration reaches **one** hub.

There is currently no published canonical value to compare against in any case,
which is why the sibling issue asks for one; this issue constrains the form that
publication must take.

### 6. The attack is cheaper and quieter than the issue text says

Two strengthenings found during validation, both folded into the text above only
as notes here rather than rewriting the scenario:

- **The second endpoint need not be a hub.** Submit is dispatch-only and the ack
  is never read (`shim/src/nym.rs:578-594`, `:639-676`), so a bare Nym client
  that receives the frame and runs `wire::decode` is sufficient. That variant
  publishes nothing, so there is **no duplicate broadcast at all** and step 5's
  reasoning about `AlreadyKnown` becomes unnecessary rather than merely
  satisfied.
- **No telemetry anywhere counts targets.** The mixnet startup arm
  (`shim/src/main.rs:228-239`) logs neither the addresses nor their count — the
  clearnet arm at `:126-130` logs `hub = %hub_addr`, the mixnet arm logs a bare
  sentence. `/nym-status` reports a `diversion_configured` boolean
  (`shim/src/nym.rs:207-215`). Nothing in the running system is a function of the
  list length.

### 7. Deflations applied, so the report does not double-count

- **Against `core-linkage-survives-…` (G29).** For the operator's *own* wallets,
  G29 already yields near-certain linkage from the unpadded wallet leg. This
  issue's marginal gain there is exactness and the raw bytes rather than a new
  class of harm — but it is **not** redundant, because the two are
  anti-correlated in time: G29's channel closes under wallet-side padding and
  ZIP 318 conformance, which the project is actively asking wallet developers
  for, and **this one does not close under anything a wallet can do.** State both,
  do not stack them.
- **The set-subtraction half is real but second-order.** The operator learns the
  exact bytes, hence the exact txid, of every migration *their own* shim
  contributed, and can subtract those members from every published batch. That
  harms *other* operators' users. It is an upgrade from G29's statistical
  subtraction to a certain one, not a new capability, and should be reported as
  the upgrade.
- **Not deniable.** Unlike the on-path and inference attacks elsewhere in this
  audit, this one leaves a durable public artefact: `deploy.sh:206-219` pushes the
  tree and a `deploy-<app-id>` tag to a public repository, so the appended address
  is recorded permanently. This is the single strongest thing arguing the
  severity down, and it is why the recommendation to publish the expected list is
  worth so much: it converts a permanent record nobody reads into a permanent
  record that fails a check.

### Why High rather than Medium

Impact is the maximum the product can suffer: the primary adversary named in the
threat model obtains the complete plaintext of every Orchard-touching transaction
their users send, in real time, with certainty, and `THREATMODEL.md` C2 ("the
party that sees your transaction in the clear and the party that sees your IP
address are two different parties") — the invariant the whole two-component
architecture exists to create — is destroyed for that endpoint. Exploitation
needs one comma in a config file and one VM: no vulnerability, no code change to
either binary, no on-path position, no mixnet position. Nothing observable
changes anywhere — the canonical hub keeps batching and publishing, wallets
confirm on the normal cadence, and no smoke test, health endpoint or telemetry is
a function of the list length. Every check the project documents passes on its
own terms, including `caution verify` with all three PCRs reproducing and the TLS
certificate binding verified, and the one remediation the audit proposes
elsewhere for this trust boundary does not catch it. That combination —
maximal impact, trivial cost, zero functional signature, and a defence that
exists but is aimed one inch to the side — is High.

It is held below Critical because it requires a deliberately hostile operator
(not an accident or a stranger), it affects one endpoint's users at a time rather
than everyone at once, and it leaves the durable public record described above.

### Also confirmed, and worth a sentence in the report

The code comment at `shim/src/nym.rs:618-634` is an accurate and well-reasoned
statement of exactly this hazard — *"safe only while the other addresses are
DEAD"*, *"it doubles the number of enclaves holding the plaintext"*, *"needs an
explicit passive mode in the hub, which does not exist today"*. The design's own
author identified the property and the missing control. What is missing is not
the analysis; it is (a) any enforcement or warning at the configuration layer,
and (b) two public documents that describe the mechanism correctly. That makes
recommendations 1 and 2 the cheap, high-value fixes here.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

# The hub's indexer hop is authenticated only if an environment variable happens to be set: unset `ZIH_INDEXER_TLS` fails OPEN into plaintext h2c under a comment that says the hub refuses to run, and when it *is* set nothing in the attested binary constrains what it is set to

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/config.rs:47-53` (the field), `:100-108` (`Config::indexer_tls`), `:142-144` (the unit test that pins the optionality); `audit-target/zeronym/hub/src/main.rs:30-39` (the warn-and-continue, and the comment that says otherwise) versus `:41-45` (the invariant that *does* abort startup); `audit-target/zeronym/hub/src/chain.rs:124-128` (`tls: Option<Arc<IndexerTls>>`), `:316-334` (the live plaintext branch), `:438-445` (`classify_send_response`), `:459-467` (`best_of`), `:212-266` (relayed lookups); `audit-target/zeronym/hub/src/batcher.rs:342` (`drain_shuffled`), `:365-390` (an `Accepted` verdict consumes the entry); `audit-target/zeronym/hub/src/tls.rs:1-26` (the "why this is not optional" module doc) and `:59-84` (`IndexerTls::new` — no pin, no allowlist). Enforcement that exists only *outside* the attested binary: `audit-target/zeronym/deploy.sh:115`, `audit-target/zeronym/hub/deploy/caution/assemble-caution.sh:126-131`, `audit-target/zeronym/deploy.env.example:23`. Claims bearing on it: `audit-target/zeronym/README.md:77`, `audit-target/zeronym/hub/src/lib.rs:29`, `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:128-132`, `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:73-76` and `:282`, and — stating the opposite and false — `audit-target/zeronym/hub/deploy/Containerfile:166-167` and `audit-target/zeronym/hub/deploy/README.md:48`
**Found by agent:** Local (two concurrent file audits: `hub/src/config.rs` and `hub/src/tls.rs`), merged into one issue by the Issue Validator on 2026-08-18
**In scope of audit?** Yes — operator-supplied configuration and environment (`ZIH_*`) is a declared trust boundary in `audit-context/AUDIT-INSTRUCTIONS.md`, priority area 5 is fail-closed discipline, and markdown/comment claims are in scope as security claims under ICTM

> **Note on the filename.** This file keeps its original name so the twenty
> cross-references elsewhere in `audit-state/` stay valid. Two phrases in that
> filename are **legacy artefacts and are wrong**: the hop is *not* "required in
> every document" (two build documents deny the TLS stack exists at all — see
> Description §5), and the absent state is *not* "unobservable" (it is measured
> into PCR0/PCR1 and served at `.manifest.run_command` — see Description §4). The
> title above is the corrected statement of the finding. This file also **merges**
> `hub-indexer-tls-is-optional-operator-chosen-and-unobservable.md`, filed
> concurrently by the `hub/src/tls.rs` audit, which is retained under `invalid/`
> for bookkeeping only.

## Description

The hub's outbound hop to the indexer is the channel by which every migration the
hub holds leaves the enclave on its way to the network. Whether that channel is
authenticated at all is decided by the *presence* of one environment variable,
and the absent case fails **open**.

**1. Unset means no authentication, and the process keeps running.**
`indexer_tls` is an `Option<String>` with no default (`config.rs:47-53`).
`Config::indexer_tls()` maps `None` to `Ok(None)` (`:100-108`), `main` emits a
`tracing::warn!` and continues (`main.rs:34-38`), and `ChainClient` carries a
complete, live plaintext branch for that case (`chain.rs:316-334`). "Plaintext"
understates it: with `tls: None` no certificate is requested, presented or
checked, so the hop has **no peer authentication of any kind**. Whoever is on the
path *is* the indexer, as far as the hub can tell.

The comment directly above says the opposite of what the code does
(`main.rs:30-32`): *"Refuse to run blind."* Nothing refuses. There is no
`Config::validate()` in the crate. Five lines further down, `main` demonstrates
the discipline it did not apply here, and applies it to a set of four
compile-time constants that no operator can change (`main.rs:41-45`,
`params.validate()?`). Verified for this audit: `BatchParams` is constructed only
via `Default::default()`, and `grep -rn "std::env\|env::var" hub/src` returns
exactly one hit (`nym_driver.rs:202`), so that startup invariant genuinely is not
operator-settable — the hub hard-fails at boot for constants and warn-continues
on the one operator-settable value that decides whether its entire egress is
authenticated.

The same struct applies the opposite discipline to its two booleans eight lines
away: `--nym` and `--http-submit` take an explicit `true`/`false`
(`config.rs:59-65`, `:93-97`) precisely *because* "an environment variable's mere
presence is a bad way to express a security-relevant choice". That care is spent
on the two flags whose worst-case misreading is a mixnet client that does not
start and an endpoint that 404s. It is not spent on the one variable whose
**absence** is the dangerous state. A unit test pins the optionality as intended
behaviour (`config.rs:142-144`, `fn tls_is_optional_but_a_bad_name_is_refused`,
first assertion `assert!(cfg.indexer_tls().expect("no tls configured").is_none())`).

**2. The harm is integrity, not confidentiality.** The confidentiality loss is
genuinely bounded — the batch is public in a mempool seconds later — and this
issue does not lead with it. What is unbounded is what an unauthenticated peer
can *do*:

- **Destroy every migration in the batch, silently.** A forged
  `SendResponse { error_code: 0 }` is enough. `classify_send_response`
  (`chain.rs:438-445`) returns `Accepted { txid: resp.error_message }` with **no
  check that the string is a txid, or is a hash, or is anything at all**;
  `best_of` ranks `Accepted` highest (`chain.rs:459-467`); `flush` counts it
  achieved and does **not** requeue it (`batcher.rs:365-390`). The entry was
  already removed from the queue by `drain_shuffled` (`batcher.rs:342`), the
  queue is RAM-only, and the shim answered the wallet `error_code 0` at mixnet
  hand-off (`shim/src/hub.rs:232-240`), keeping no copy. There is no copy
  anywhere and nothing retries.
- **Or, worse, capture and re-publish at a chosen moment.** The forged rejection
  is not the strongest move. An on-path party holds valid, signed transaction
  bytes: it can answer `error_code 0`, drop the broadcast, and publish that one
  transaction itself at any later instant. The migration *does* confirm, so
  nothing anywhere looks wrong, and it appears on chain **alone, at a time the
  attacker chose** — which is precisely the linkage the batch exists to prevent.
- **Drive the flush clock.** `tip_height` runs over the same hop and takes the
  max over endpoints (`chain.rs:155-175`), so a forged `GetLightdInfo` height
  moves the cadence. That is the lever `hub-tip-advance-unbounded-flush-clock.md`
  (confirmed, Medium) describes, here available without being the indexer.
- **Answer the wallets' lookups.** The hub relays every shim's `GetTransaction`
  to the same endpoints and returns whatever comes back; `chain.rs:212-266`
  never checks that the returned `RawTransaction` matches the requested
  `TxFilter.hash`.

**3. Even when it *is* set, nothing in the attested binary constrains the value.**
`IndexerTls::new(sni_name)` accepts any parseable name and verifies against the
compiled-in WebPKI roots (`tls.rs:59-84`). There is no compiled-in expected name,
no SPKI pin and no allowlist, and `ZIH_INDEXERS` is operator-supplied too. So
`ZIH_INDEXER_TLS=indexer.<operator's own domain>` plus `ZIH_INDEXERS=<operator's
own terminator>` is a fully TLS-"protected" hop that the operator terminates and
reads — and it looks *more* correct in a manifest review than deleting the line.
The TLS verification itself is correct and should be reported positively
(compiled-in `webpki-roots`, no `dangerous()`, no custom `ServerCertVerifier`, no
filesystem or environment root source anywhere in the crate); "correctly verified
against a name the adversary chose" is simply not a confidentiality property.

**4. Bounding facts — all of them verified, and they are why this is Medium and
not High.**

- The repository's own deploy path refuses the unset state **three times**:
  `deploy.env.example:23` ships `INDEXER_TLS=na.zec.rocks`, `deploy.sh:115` uses
  `: "${INDEXER_TLS:?set INDEXER_TLS for a hub}"` (verified by execution under
  `dash`: `:?` aborts on unset **and** on empty), and
  `assemble-caution.sh:126-131` refuses again with `exit 2`. Every worked example
  in every runbook passes `--indexer-tls`. **This finding must therefore be
  argued from the code, never from the deploy path.**
- An **empty** value fails closed. This one is derived from the two libraries'
  sources rather than executed (there is no Rust toolchain in the audit
  environment): clap 4.6.5 stores `env::var_os(name)` at `Arg::env()`
  (`clap_builder/src/builder/arg.rs`, `let value = env::var_os(&name);`), so a
  present-but-empty variable is applied as a value; `""` then reaches
  `ServerName::try_from("")` (`tls.rs:76`), and rustls-pki-types 1.15.1's
  DNS-name validator rejects the empty name (it is not a valid `DnsName` and does
  not parse as an `IpAddr`), so `main.rs:33`'s `?` exits the process. Nothing in
  this finding depends on that case; it is recorded so the *unset* case is not
  confused with it.
- The value **is** covered by the attestation. `unit.env` string values become
  `export KEY=<value>` lines in the generated `run.sh`
  (`caution-config/src/lib.rs:253-274`), `run.sh` is copied into the single EIF
  ramdisk, and that ramdisk is measured into PCR0/PCR1. It is additionally served
  to anyone at `.manifest.run_command` of every `/attestation` response. So the
  filed claim "no party outside the operator can tell whether it is on" is
  **struck**. What replaces it is narrower and still true: measurement
  **discloses** a value, it never **detects** a change (`deploy.sh:206-219`
  publishes the *deployed* tree as `--app-source`, so an edited manifest
  reproduces its own PCRs and `caution verify` prints PASSED), and **no document
  in zeronym tells anyone to look** — that omission is owned by the confirmed
  Medium `auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`.
- Against the **shipped** endpoint an accidental omission would not silently
  succeed. `deploy.env.example:22-23` pairs `66.241.124.200:443` with the
  certificate name `na.zec.rocks`, i.e. a TLS-terminating 443, and a TLS listener
  does not answer plain h2c — so every tip query and every flush would fail and
  the hub would be visibly broken (to the extent anything about an attested hub is
  visible). This is therefore not a quiet accident on the shipped configuration:
  it is a deliberate, deniable downgrade, or a genuine accident on a hub pointed
  at a plaintext endpoint by other means. Note the corollary, which cuts the other
  way: an on-path attacker who *does* answer h2c turns that visibly broken hub
  into a working one under its own control.

**5. Correction to the premise this file was originally titled on.** "Required in
practice in *every* document" is wrong. Two build documents state something
stronger and **false** — that the mechanism does not exist in the binary at all:
`hub/deploy/Containerfile:166-167` (*"The hub speaks plaintext HTTP/1.1 and has no
TLS stack in its dependency graph today"*) and `hub/deploy/README.md:48` (*"no TLS
stack"*, listed among the attested build's ingredients). Both are refuted by
`hub/Cargo.toml:95-102`, where `rustls`, `tokio-rustls`, `rustls-pki-types` and
`webpki-roots` are unconditional dependencies under a comment stating the exact
opposite. The reader of those two documents — the operator who configures the
image from them — is not merely under-warned about a variable; they are told the
mechanism behind it does not exist. (The `README.md` instance is filed separately
as `hub-deploy-readme-lists-no-tls-stack-among-the-attested-builds-ingredients.md`;
the `Containerfile` instance is Recommendation 3 of
`shim-containerfile-plants-a-system-ca-trust-store-in-the-enclave-that-tls-rs-says-cannot-exist.md`.)

## Attack Scenario and Steps

Two routes reach the unauthenticated state. Neither runs through `deploy.sh`.

**Route A — a hub that is not deployed by the repository's scripts.**
`README.md:70` invites it (*"Operators run the shim in front of their indexer,
and optionally a hub"*), and the code and docs explicitly support hubs outside an
enclave (`config.rs:50-51` "correct only for a test or a trusted local path";
`deploy.env.example:40` "Leave unset for a local hub"). `caution.hcl.tmpl` is a
*template*: an operator writing their own manifest, a container start, a systemd
unit or a `docker run` has no `assemble-caution.sh` in the path. Omitting
`ZIH_INDEXER_TLS` yields a running, healthy-looking hub — `/healthz` answers a
constant `ok` (`server.rs:450-452`) — with an unauthenticated egress. Note that
the neighbouring mistake fails loudly: omitting `ZIH_INDEXERS` is a clap
`required = true` error and the process never starts. Only the security-relevant
omission boots.

**Route B — a one-line edit between assembly and deployment.**
`assemble-caution.sh` renders `$DEST/caution.hcl`; `deploy.sh` then pushes that
same directory to `APP_SOURCE` (`deploy.sh:206-219`). Deleting the
`ZIH_INDEXER_TLS` line from the deployed copy — or, in the leg-3 form, changing
its value to a name the operator holds a certificate for — is re-published as the
app source, so the enclave reproduces its **own** PCRs and `caution verify`
prints PASSED. Nothing re-validates the manifest, and no zeronym document tells
any verifier to read `.manifest.run_command`.

Once in that state, the hub dials every `ZIH_INDEXERS` address over plain h2c
(`chain.rs:316-334`). The Nitro **parent host** is unavoidably on that path: the
enclave has no NIC, and every packet it sends is tunnelled to the parent over
vsock and forwarded by it (Caution's `run.sh.template`:
`socat TUN,tun-type=tap,…,tun-name=eth0 VSOCK-CONNECT:3:3`). So does any network
element between that host and the indexer. Any of them can now:

1. Read every `SendTransaction` body of every flush — the whole batch, seconds
   before it is public — plus every `GetTransaction` the hub relays for a shim.
2. Answer `SendResponse { error_code: 0 }` for chosen members and drop the
   broadcast, destroying them permanently (§2 above) while the wallets were told
   `error_code 0` at dispatch; or hold and re-publish a chosen member alone at a
   chosen instant, which confirms normally and defeats the batch.
3. Answer `GetLightdInfo` with a height of its choosing and move the flush
   cadence.

**Attack Requirements and Assumptions:**

- **No internet-side or wallet-side attacker can reach this.** The vulnerable
  configuration is chosen by whoever deploys the hub. The exploiting party is
  whoever is on the hub's egress path — for a Nitro deployment that is, first and
  unavoidably, the parent host, i.e. `AUDIT-INSTRUCTIONS.md` attacker 8 and
  exactly the party the enclave exists to exclude.
- **The repository's own path refuses it** (three checks, all verified). The
  honest framing is a fail-open *default of the attested binary*, whose only
  enforcement lives in two shell scripts that are neither measured, nor run at
  boot, nor part of what an auditor verifies.
- Leg 3 (no pin on the value) needs no deviation from the deploy path at all —
  only an operator who names a host they control.
- The state is disclosed in the attestation manifest, so it is detectable *by
  someone who thinks to look*; nothing tells anyone to look, and nothing in the
  running system refuses.

## Impact on Users

`hub/deploy/caution/caution.hcl.tmpl:10-17` states the deal the whole product
offers: attestation plus reproducibility together say *"the code you read is the
code that is holding your migration in plaintext, and it does nothing with it but
broadcast it."* Both legs of this finding falsify the second half — not by
changing the code, but by choosing who the code talks to.

- **Integrity (the headline).** Migrations that a wallet reported as sent are
  destroyed with no error, no retry and no diagnostic anywhere — the hub counts
  them achieved. Past `nExpiryHeight` the loss is permanent, and there is no
  confirmation tracking (designed, not built), so a user learns only by noticing
  much later that funds never moved. The blast radius is fleet-wide: the hub
  holds migrations from every shim.
- **Anonymity.** A party who both reads the batch and controls the tip can
  isolate a chosen migration into a batch of one, or hold one back and publish it
  alone later. That is the exact outcome the cadence, the shuffle, the mixnet and
  the enclave were all built to prevent.
- **Confidentiality (bounded, and stated as such).** A few seconds' lead time on
  data that becomes public anyway, plus the relayed `GetTransaction` lookups,
  which never appear on chain at all.
- **Assurance.** `README.md:77` tells users the attested enclaves are "the only
  things that ever see a migration in cleartext" and `hub/src/lib.rs:29` tells a
  code reader that `tls` "verifies that connection, which an enclave deployment
  requires". Neither is enforced by the artefact the attestation covers.

## Technical Details / Code Analysis

The field, in full (`hub/src/config.rs:47-53`):

```rust
    /// The DNS name the indexer's certificate must carry.
    ///
    /// Unset means PLAINTEXT h2c, which is correct only for a test or a trusted
    /// local path. A deployed enclave must set this: without it the enclave's
    /// parent host reads every batch in the clear moments before it is public.
    #[arg(long = "indexer-tls", env = "ZIH_INDEXER_TLS")]
    pub indexer_tls: Option<String>,
```

The accessor (`hub/src/config.rs:100-108`):

```rust
impl Config {
    /// The TLS verifier for indexer connections, if one is configured.
    pub fn indexer_tls(&self) -> Result<Option<IndexerTls>, BoxError> {
        match &self.indexer_tls {
            Some(name) => Ok(Some(IndexerTls::new(name)?)),
            None => Ok(None),
        }
    }
}
```

The whole of the startup enforcement (`hub/src/main.rs:30-39`):

```rust
    // Refuse to run blind. A hub without indexer TLS lets the enclave's parent
    // host read every batch in the clear moments before it is public, so this is
    // announced loudly rather than left to a config review.
    let tls = config.indexer_tls()?;
    if tls.is_none() {
        tracing::warn!(
            "no --indexer-tls: the hop to the indexer is PLAINTEXT and the host can read every batch"
        );
    }
    let chain = Arc::new(ChainClient::new(config.indexers.clone(), tls)?);
```

versus the invariant five lines below that *does* abort (`hub/src/main.rs:41-45`):

```rust
    // The expiry budget is asserted at startup, not trusted. A parameter change
    // that overspends it must fail here rather than be discovered in production
    // as a percentage of real traffic quietly expiring.
    let params = BatchParams::default();
    params.validate()?;
```

"Announced loudly" is also false in the deployment this component targets. The
warning is a `tracing` line on the enclave's console, and an attested enclave has
no console: Caution's own Terraform starts the enclave with `--debug-mode` and
installs the console-capture unit **only** under `debug_mode == "true"`
(`terraform/modules/aws/nitro-enclave/user-data.sh:166`, and the
`%{ if debug_mode == "true" }` block around `capture-enclave-console.sh`), while
`debug { enabled = true }` is documented by the platform as disabling attestation
verification. In the attested case the warning is discarded; in the debug case it
is delivered to the parent host, i.e. to the one party the plaintext benefits.

The live plaintext branch (`hub/src/chain.rs:316-334`):

```rust
        let authority = match &self.tls {
            Some(tls) => tls.authority().to_owned(),
            None => addr.to_string(),
        };

        let request = hyper::Request::builder()
            .method("POST")
            .uri(format!("http://{authority}{path}"))
            …

        let response = match &self.tls {
            Some(tls) => {
                let stream = tls.connect(addr, stream).await?;
                round_trip(stream, request).await?
            }
            None => round_trip(stream, request).await?,
        };
```

with the field's own comment stating an assumption about operator behaviour as
though it were an invariant (`chain.rs:126-128`): *"`None` means plaintext h2c …
A deployed enclave always sets this."*

The verdict a forged reply reaches (`hub/src/chain.rs:438-445`):

```rust
fn classify_send_response(resp: &SendResponse) -> Publish {
    if resp.error_code == 0 {
        return Publish::Accepted {
            txid: resp.error_message.clone(),
        };
    }
    classify_publish_error(&resp.error_message)
}
```

and what `flush` then does with it (`hub/src/batcher.rs:342`, `:365-390`):

```rust
    let batch = queue.drain_shuffled();
    …
    for (i, entry) in batch.into_iter().enumerate() {
        match outcomes.get(i) {
            Some(Publish::Accepted { .. }) | Some(Publish::AlreadyKnown) => achieved += 1,
            Some(Publish::Rejected { .. }) => rejected += 1,
            Some(Publish::Retryable { reason }) => { … unplaced.push(entry); }
            None => unplaced.push(entry),
        }
    }
    …
    let requeued = queue.requeue(unplaced);
```

Only `Retryable` survives. An `Accepted` — which any on-path party can produce by
answering `error_code = 0` — consumes the entry, and `flush`'s own comment
records why that is unrecoverable: *"the shim answered the wallet error_code 0
the moment the frame reached the mixnet and keeps no record, so once the entry
left this queue there is no other copy anywhere that anyone will retry."*

On leg 3, the entirety of what the attested binary knows about who it will talk
to (`hub/src/tls.rs:59-84`):

```rust
    pub fn new(sni_name: &str) -> Result<Self, BoxError> {
        install_crypto_provider();

        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![ALPN_H2.to_vec()];

        let server_name = ServerName::try_from(sni_name.to_owned())
            .map_err(|_| -> BoxError { format!("invalid indexer TLS name {sni_name:?}").into() })?;
        …
```

`sni_name` is whatever the environment said, and the addresses come from
`ZIH_INDEXERS`. Both are operator-supplied; neither is compared against anything.

Finally, the enforcement everyone relies on, in the two places it actually lives
— both outside the measured artefact, neither run at boot:

```sh
# deploy.sh:115
  : "${INDEXERS:?set INDEXERS for a hub}"; : "${INDEXER_TLS:?set INDEXER_TLS for a hub}"

# hub/deploy/caution/assemble-caution.sh:126-131
[ -n "$INDEXER_TLS" ] || {
	echo "error: --indexer-tls is required (the DNS name the indexer's cert carries)." >&2
	echo "       Without it the hop is plaintext and the parent host reads every batch." >&2
	exit 2
}
```

Degenerate-value behaviour, checked rather than assumed: an **IP literal** is
accepted (`ServerName::IpAddress`) and then fails at the handshake — fail-closed
with a confusing symptom, the hub-side instance of the trap filed for the shim in
`backend-tls-ip-literal-defeats-the-startup-validation-and-debug-formats-into-the-authority.md`
(the hub's `authority()` stores the original string, so that issue's
`Debug`-into-protocol half does not apply here) and, for the hub's own copy of
that gap, `hub-indexer-tls-drops-two-guards-its-sibling-backend-tls-carries.md`
(plausible, Low — a separate finding about the *set* value, not the unset one);
and `ZIH_INDEXERS` empty or
whitespace-only fails closed twice over (clap `required = true` plus
`SocketAddr` parsing, and `ChainClient::new`'s own `endpoints.is_empty()` check
at `chain.rs:132-136`).

## Recommendations

1. **Make the safe state the default and the unsafe one explicit, in the
   binary.** Either mark `--indexer-tls` `required = true`, or keep the `Option`
   and require an explicit opt-in — `--allow-plaintext-indexer` /
   `ZIH_INSECURE_PLAINTEXT_INDEXER=true` — before `indexer_tls()` may return
   `Ok(None)`, aborting startup otherwise exactly as `params.validate()?` already
   does one screen below. This is the same discipline `--nym` and
   `--http-submit` already use, applied to the variable that needs it most. A
   test-only convenience should cost a flag that says "insecure", not a missing
   variable. Update `config.rs:142-144` so the test pins the new behaviour.
2. Failing that, refuse `None` unless every `--indexer` address is loopback or
   RFC1918. That preserves the stated legitimate use ("a test or a trusted local
   path") and removes the deployed one.
3. **Correct the two comments and the two build documents that are false.**
   `main.rs:30-32` says "Refuse to run blind" and "announced loudly"; neither is
   true, and the second cannot be true in an enclave with no console.
   `chain.rs:126-128`'s "A deployed enclave always sets this" is an assumption,
   not an invariant. `hub/deploy/Containerfile:166-167` and
   `hub/deploy/README.md:48` state that the hub has **no TLS stack**, which
   `hub/Cargo.toml:95-102` refutes.

   *Added 2026-08-18 when `hub-deploy-readme-lists-no-tls-stack-among-the-attested-builds-ingredients.md`
   was merged into this issue (see the merge record below).* Two details the
   merged filing established, which the fixing commit should carry:
   (a) `hub/deploy/README.md:46-48` asserts "no TLS stack" of the **shim** as
   well — the sentence ends "is identical to the shim" — and that half is false
   too (`shim/Cargo.toml:71-79` takes `rustls`, `tokio-rustls`,
   `rustls-pki-types` and `rustls-acme` unconditionally, and the shim's own
   `deploy/README.md:299` records the build where they were linked in as
   "TLS on both hops … the binary grows 4.4 MB to 7.6 MB, which is the TLS
   stack"); (b) the sentence sits in this document's list of **determinism
   ingredients**, so the correct replacement is not a deletion but a positive
   statement: the hub links `rustls` with the `ring` provider and a compiled-in
   `webpki-roots` trust store, so a `webpki-roots` bump both moves the published
   hash and changes which CAs the enclave trusts. Provenance for the fix
   message: the phrase was written by `d5f4687` (2026-08-09), when the hub really
   had no TLS and spoke plaintext JSON-RPC; `a746496` (2026-08-09, the same day)
   added `hub/src/tls.rs` and the four dependencies, rewrote the bullet
   immediately above it in the same file, and left it standing.
4. **Give leg 3 something to check against.** Publish the expected
   `ZIH_INDEXER_TLS`/`ZIH_INDEXERS` alongside the expected measurements so a
   verifier reading `.manifest.run_command` has a reference; better, compile the
   expected name (or an SPKI pin) into the measured binary, so changing the peer
   changes what an auditor reproduces rather than only what they could have read.
5. Optionally, report the effective indexer-hop mode (authenticated under which
   name, or plaintext) on `GET /nym-status`. It discloses nothing an adversary
   does not already have — the same string is in the public manifest and the
   endpoint addresses are in the egress rules — and it converts "clone the
   operator's `app_sources` repo, or POST for an attestation, and know to read
   `run_command`" into one `curl` any shim operator or wallet author can run.
   Note this is a convenience, not the fix: recommendation 1 is the fix.
6. Add the manifest check to the auditor recipe at `README.md:71` — tracked in
   `auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`
   (confirmed, Medium), which owns that half and should not be double-counted
   here.

Cross-references: `hub-tip-advance-unbounded-flush-clock.md` (confirmed, Medium —
what a forged tip on this hop buys); `hub-chain-zaino-node-rejections-are-never-verdicts.md`
and `publish-verdict-strings-are-zcashds-vocabulary-only-…md` (confirmed, Medium —
the other ways a verdict destroys an entry);
`hub-lookup-fall-through-hands-every-wallets-txid-to-whichever-indexer-the-hub-is-pointed-at-…md`
(confirmed, Medium — the lookups this hop also carries);
`hub-chain-indexer-tls-hop-has-no-test-coverage-and-declares-scheme-http.md`
(plausible, Info — the same hop from the `chain.rs` side); and
`operators-runbook-attributes-the-hub-destination-to-the-binary-hash-and-egress-rules-neither-of-which-binds-it.md`
(confirmed, Low — the same "configuration, not the binary, decides where plaintext
goes" shape on the shim side).

## Validation Information

**Verdict: CONFIRMED. Severity: Medium** (both filings proposed Medium; that
grade survives, for different reasons than either gave). Validated 2026-08-18 by
the Issue Validator, which also **merged** the two concurrently filed copies of
this finding into this file.

### What was verified, directly against the target

| Claim | Verified at |
|---|---|
| `indexer_tls` is `Option<String>`, unset → `Ok(None)` | `config.rs:47-53`, `:100-108` |
| Unset produces a `warn!` and the process continues | `main.rs:30-39`; no `Config::validate` anywhere in `hub/src` |
| A live, complete plaintext branch consumes it | `chain.rs:316-334`; field comment `:126-128` |
| A unit test pins the optionality as intended | `config.rs:142-144` |
| The two booleans nearby take an explicit `true`/`false` for this exact reason | `config.rs:59-65`, `:93-97` |
| `params.validate()?` aborts startup for constants an operator cannot change | `main.rs:41-45`; `BatchParams` built only by `Default::default()`; one `env::var` in `hub/src` (`nym_driver.rs:202`) |
| `error_code == 0` ⇒ `Accepted { txid: error_message }`, no validation of the string | `chain.rs:438-445` |
| `Accepted` outranks everything and consumes the entry; only `Retryable` is requeued | `chain.rs:459-467`; `batcher.rs:342`, `:365-390` |
| The wallet was already told success at mixnet hand-off | `shim/src/hub.rs:232-240` |
| Relayed lookups are not checked against the requested hash | `chain.rs:212-266` |
| No pin/allowlist of any kind on the configured name | `tls.rs:59-84`; no `dangerous()`, no custom verifier, no filesystem/env root source in the crate |
| `deploy.sh:115`'s `${VAR:?}` aborts on unset **and** on empty | executed under `dash`: both cases exit 2 |
| `assemble-caution.sh:126-131` refuses an empty/absent value with `exit 2` | read directly |
| `deploy.env.example:23` ships a value; every worked runbook example passes `--indexer-tls` | `deploy.env.example:23`; `hub/deploy/caution/README.md:38`; `OPERATORS.md:61` |
| `deploy.sh` publishes the **deployed** tree as `--app-source` | `deploy.sh:206-219` |
| An attested enclave has no console; console capture exists only under `--debug-mode` | Caution `terraform/modules/aws/nitro-enclave/user-data.sh:166` and its `debug_mode` guard |
| Every enclave packet transits the parent host | Caution `src/enclave-builder/templates/run.sh.template` (`socat TUN … VSOCK-CONNECT:3:3`) |
| `unit.env` is measured and served at `.manifest.run_command` | `caution-config/src/lib.rs:253-274` → `run.sh` → single EIF ramdisk; PROGRESS item 6q, re-derived here from the same source |
| The two build documents deny the TLS stack exists | `hub/deploy/Containerfile:166-167`, `hub/deploy/README.md:48`, refuted by `hub/Cargo.toml:95-102` |

### What was struck or corrected from the two filings

1. **"Unobservable when absent" / "no party outside the operator can tell whether
   it is on" — STRUCK.** `unit.env` is measured into PCR0/PCR1 and the whole
   environment is served at `.manifest.run_command` of every `/attestation`
   response. The replacement statement is "measurement discloses, it does not
   detect, and no document tells anyone to look" — and the *documentation* half of
   that is owned by the confirmed Medium recipe issue, not by this one.
2. **"Required in practice in every document" — CORRECTED.** Two build documents
   say the opposite and are false. The filename is retained for cross-reference
   stability with this correction recorded at the top of the file.
3. **"The attestation chain is untouched / verification still passes with the
   variable removed" — REPLACED with the accurate mechanism.** Verification does
   not pass against a *previously published* tree; it passes because `deploy.sh`
   publishes whatever was deployed, so the modified enclave reproduces its own
   PCRs. Same practical outcome, different and checkable reason.
4. **The confidentiality framing was demoted and the integrity framing promoted**
   (this was filing 1's distinct contribution, and it is the part that carries the
   severity). One consequence neither filing had is added: an on-path party can
   *hold and later re-publish* a single transaction, which confirms normally and
   is a cleaner anonymity break than destroying it.
5. **"Global focus area 48 is unresolved" — REMOVED**; it was closed by the G10
   platform pass and the answer is folded in above.
6. **The `main.rs` warning's fate was sharpened rather than assumed**: verified
   from Caution's Terraform that console capture exists only in `--debug-mode`,
   i.e. the warning is discarded exactly when attestation is on.

### Why Medium — not High, not Low

- **Not High.** The unauthenticated state is not reachable through any documented
  path: three independent refusals stand between an operator and it, and against
  the shipped TLS endpoint an accidental omission breaks the hub loudly rather
  than working insecurely. Every downstream harm (tip manipulation, verdict-driven
  destruction, lookup exposure) is already separately graded, and the "nobody is
  told to check the manifest" half is owned at Medium elsewhere; stacking those
  severities here would count one harm three times, against the precedent set in
  PROGRESS item 7a.
- **Not Low.** This is the definition case for Medium in
  `docs/AUDIT-PROCESS.md`: *"a serious vulnerability that only exists if the user
  has configured the application in a specific, uncommon way."* The defect is in
  the attested binary — the artefact the entire reproducibility and attestation
  apparatus points at — and its only enforcement lives in two shell scripts that
  are not measured, do not run at boot, and are not what an auditor verifies. The
  dangerous state is selected by *omission* rather than by an explicitly named
  insecure flag, which is precisely the pattern
  `docs/AVOIDING-FALSE-POSITIVES.md` §7 identifies as a real issue rather than a
  configuration excuse. Leg 3 needs no deviation from the deploy path at all.
  And the code-side consequence is not a leak of already-public data: it is
  permanent, silent destruction of migrations that wallets have already reported
  as sent, plus a lever that isolates a chosen migration on chain.

### Merge record

`hub-indexer-tls-is-optional-operator-chosen-and-unobservable.md` (filed by the
`hub/src/tls.rs` local audit) is the same finding with a different emphasis. Its
distinct contribution — leg 3, that nothing in the attested binary constrains the
value when it *is* set — is carried here in Description §3, Route B, Impact and
Recommendation 4. That file has been moved to `invalid/` **for bookkeeping only**,
with a header stating that its substance was validated and found real and that it
must not be reported as a refuted claim.

**Second merge, 2026-08-18.**
`hub-deploy-readme-lists-no-tls-stack-among-the-attested-builds-ingredients.md`
(filed by the `hub/deploy/README.md` local audit) is the document-side half of
the same defect: it audits `hub/deploy/README.md:46-48`, the second of the two
build documents already named in this issue's Location line, verification table
and Recommendation 3. Every claim in it was re-verified and holds; its two
details that were not already here are folded into Recommendation 3 above. It is
under `invalid/` **for bookkeeping only**, with the same MERGED-NOT-REFUTED
header. The report must present this defect once, here.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

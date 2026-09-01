# The shim's configuration cannot express "diversion is required": an unset, empty or whitespace-only `ZIS_HUB_NYM` resolves to no-privacy forward-only mode and the process serves wallets normally instead of refusing to start

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/config.rs:50-77` (the two hub fields), `:199-213` (`HubSelection`), `:250-289` (`Config::hub_selection`), `:420-457` (the tests that pin the behaviour); consumers at `audit-target/zeronym/shim/src/main.rs:112-143`, `audit-target/zeronym/shim/src/nym.rs:135-136`, `:180-183`, `:305-309`, `audit-target/zeronym/shim/src/intercept.rs:117-124`, `audit-target/zeronym/shim/src/proxy.rs:643-653`; the design intent at `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:10-16` and `:242`; the claim it bears on at `audit-target/zeronym/README.md:26-28`
**Found by agent:** Local (file audit of `shim/src/config.rs`); validated 2026-08-18
**In scope of audit?** Yes — `audit-context/AUDIT-INSTRUCTIONS.md` names operator-supplied configuration and environment (`ZIS_*`) as an explicit trust boundary and the operator as the primary adversary; priority area #5 (fail-closed discipline)

> **Note on the filename.** This file keeps its original name so the ten
> cross-references elsewhere in `audit-state/` stay valid. The word "silently" in
> the filename is a legacy artefact and is **wrong** — see "What is not true"
> below. The title above is the corrected statement of the finding.

## Description

`Config::hub_selection` resolves the transport into a closed three-member set
(`config.rs:205-213`):

```rust
pub enum HubSelection {
    /// No hub: classify and log, forward everything. No privacy.
    ForwardOnly,
    /// The transitional clearnet path.
    Http(SocketAddr),
    /// The mixnet path, over one or more gateway-bound hub addresses.
    Nym(Vec<String>),
}
```

There is no fourth member and no fourth flag. Nothing in the crate lets an
operator say *"this shim is supposed to divert; if it cannot, refuse to serve."*
There is no `--require-diversion`, no `--strict`, no equivalent knob anywhere in
the shim. So `ForwardOnly` — the mode in which every Orchard-touching transaction
is handed straight to the operator's indexer — is not one of three options an
operator selects. It is the value the resolver produces whenever the other two are
not affirmatively and correctly present.

Two properties compound that:

1. **Degenerate values are indistinguishable from absence.** `hub_selection` trims
   every `--hub-nym` entry and drops the empty ones *before* deciding
   (`config.rs:262-267`), so `ZIS_HUB_NYM` unset, `ZIS_HUB_NYM=`,
   `ZIS_HUB_NYM=" "` and `ZIS_HUB_NYM=" , ,"` all yield `ForwardOnly`. Two unit
   tests pin exactly this as intended behaviour (`config.rs:420-446`).
2. **The process then reports healthy.** `MixnetStatus::is_healthy`
   (`nym.rs:305-309`) is `!configured || connected`, and `configured` is set from
   exactly one place — `nym_driver.rs:188`, inside the mixnet driver task, which
   `main.rs` spawns only on the `HubSelection::Nym` arm. A forward-only shim never
   sets it, so `/healthz` answers `200 ok` for the life of the process
   (`proxy.rs:643-653` documents this as deliberate).

Note the asymmetry with the sibling transport, which is the sharpest evidence that
this is an oversight rather than a decision. `ZIS_HUB=""` is a **hard startup
error**: clap 4.6.5 applies an environment variable that is present even when its
value is empty, so `""` reaches the `SocketAddr` parser and fails. The same
templating accident therefore refuses to boot on the clearnet transport and
degrades to no-privacy on the mixnet one — and the mixnet one is what
`deploy.env.example:18` ships and what `OPERATORS.md:20` tells operators to use.

The project's own adversarial review demanded the opposite discipline for the
analogous case. `hub/REVIEW.md` #11 requires the shim's last-resort direct
broadcast to be *"off by default, behind an explicitly named config flag"*,
reasoning that *"nearly every attack in this set converges on the same final step:
get the shim to broadcast directly. That is only possible because the shim fails
OPEN on privacy. Making it fail closed removes the whole class in one change."*
The shim has no direct-broadcast path at all, so #11 is satisfied by absence — but
`ForwardOnly` is reached the same way #11 was worried about: by omission rather
than by an explicitly named act.

### What is not true — four corrections that bound this finding

These were checked during validation and each one refutes part of the original
framing. They are recorded here so the report does not overstate the issue.

1. **The state is not undetectable.** `/nym-status` is served on the shim's
   wallet-facing listener, unauthenticated, to anyone
   (`proxy.rs:654`, `nym.rs:207-211`), and reports `diversion_configured: false`
   for a forward-only shim, permanently. `shim/deploy/caution/OPERATORS.md:415`
   documents that exact field as an alert condition: *"`false` on a shim you built
   with `--hub-nym` — it is forward-only and hiding nothing"*, and
   `smoke.sh:331-347` already implements the check. The signal is one `curl` away
   for any wallet author, monitor or passer-by. (Its one real weakness: the flag is
   mixnet-only, so a *clearnet-hop* shim also reports `false`. Since the clearnet
   hop is documented as legacy and non-functional against the current hub
   (`OPERATORS.md:18-21`), that ambiguity is not load-bearing today.)
2. **The attestation chain is not blind to it.** Coordinator open item 6q
   established from the Caution platform source that `unit.env` **is** measured
   into PCR0/PCR1, and `ZIS_HUB_NYM` is rendered into the manifest's env block
   (`assemble-caution.sh:352`, `caution.hcl.tmpl:161`). So a forward-only shim has
   different PCRs from a diverting one, and the whole environment is additionally
   served unauthenticated at `.manifest.run_command` of every `/attestation`
   response. Anyone can read whether a hub is configured, and which one.
3. **The deploy tooling does not select it silently.**
   `assemble-caution.sh:414` prints `"==> forward-only: no --hub, so migrations are
   forwarded to the operator's indexer (no privacy)."` on the operator's terminal,
   and the generated `PROVENANCE` file records `diversion: OFF (forward-only, no
   privacy)`. Moreover the assembler **hard-refuses** the two accidental routes a
   `deploy.sh` operator could take, because `deploy.env.example:63` ships
   `NYM_EGRESS` non-empty and `deploy.sh:125-127` passes it unconditionally:
   `assemble-caution.sh:189-192` exits 2 on `--nym-egress` without `--hub-nym`, and
   the structural check at `:226-235` (`case "$addr" in ?*.?*@?*)`) exits 2 on a
   whitespace-only address before the binary ever sees it.
4. **Forward-only is an intentional, documented product phase, not an oversight.**
   `shim/deploy/caution/OPERATORS.md:10-16` calls it *"Phase 1 … it classifies and
   logs but forwards everything"* and `:242` states *"Forward-only stays the
   default: no `--hub-nym`, no diversion."* The mode's existence is by design
   (`docs/AVOIDING-FALSE-POSITIVES.md` §6). **The defect is the absence of a way to
   opt out of it**, not its existence.

## Attack Scenario and Steps

**Path A — the deceptive operator.** Cheap, but detectable.

1. The operator deploys the real, attested, reproducible shim image in front of
   their indexer and advertises the endpoint as running zero-indexer.
2. They omit `HUB_NYM` and `NYM_EGRESS` (or invoke `assemble-caution.sh` by hand
   without `--hub-nym`, which is the interface `OPERATORS.md:77-87` documents).
   `hub_selection` returns `ForwardOnly`.
3. Every wallet pointed at the endpoint has each Orchard-touching transaction
   classified, logged, and then **handed to the operator's own indexer**
   (`intercept.rs:117-124`).
4. The operator joins the wallet's TCP source IP to the transaction bytes and,
   once it lands on chain, to the value moved — the exact linkage the product
   exists to prevent, permanent because the chain is permanent.
5. `/healthz` answers 200 throughout, and wallets, which per `README.md:68`
   "install nothing and change no setting", do not check anything.

**But this path is observable.** `/nym-status` reports `diversion_configured:
false` and `/attestation`'s `.manifest.run_command` shows no `ZIS_HUB_NYM`. What
is missing is not the signal but any documented instruction to read it: the
auditor recipe at `README.md:71` lists four steps (attestation, PCRs, reproduce,
Certificate Transparency) and none of them observes this state. So Path A works
only against a counterparty who never looks — which, absent a wallet-side check,
is every ordinary user.

**Path B — the configuration accident. This is the path the fix addresses.**

1. An operator runs the shim outside `deploy.sh` — compose, systemd, Kubernetes, a
   hand-written `caution.hcl`, or their own orchestration — with
   `ZIS_HUB_NYM=${HUB_NYM}` in an env file, and `HUB_NYM` renders empty. This is
   not hypothetical: it is the documented motivation for the filtering
   (`config.rs:253-261` — *"an existing clearnet deployment that templates the new
   variable in as empty would stop booting"*), and the hub's Nym address must be
   re-read and re-templated on every hub restart because a diskless enclave mints a
   new one (`OPERATORS.md:369-377`).
2. `config.rs:262-267` drops the empty entry; `hub_selection` returns
   `ForwardOnly`. **Startup succeeds.**
3. `main.rs:137-140` emits one `tracing::warn!` at process start. In an attested
   enclave that line goes to a console the parent host cannot read (coordinator
   open item 7), so in the deployment the product recommends, nobody sees it.
4. Every subsequent runtime signal says the component is working: `/healthz` is
   200, wallets get normal `SendResponse`s from the operator's indexer, and the
   per-transaction log line asserts `diverted_in_production=true` for a transaction
   it forwarded (filed separately as
   `forward-only-log-claims-migration-was-diverted.md`).

The only durable signal is `/nym-status`, which nothing in this path polls.

**Attack Requirements and Assumptions:**

- Path A requires only that the operator omit environment variables. No code
  modification is involved, so the *binary* is the audited one — but the PCRs and
  the disclosed manifest do change, so the deployment is not indistinguishable
  from a diverting one to anyone who reads them.
- Path B requires no attacker at all, but does require a deployment that bypasses
  `deploy.sh`/`assemble-caution.sh`, because the assembler hard-refuses the two
  routes a template-following operator could take.
- Neither path can be triggered *at runtime* by an outsider: `diversion` is decided
  once in `main.rs:112-143` and threaded immutably; no hub refusal, mixnet death,
  config reload or attacker input flips it (verified by the G1 global audit,
  coordinator open item 6r). Every reachable runtime degradation is toward
  fail-closed.

## Impact on Users

A wallet user of a forward-only endpoint receives none of the product's privacy
properties while the process, the operator's own liveness monitoring, and the
wallet's experience all report normal operation. `README.md:26-28` states the two
headline protections without condition:

> - **Broadcast contents.** An Orchard-touching transaction is hidden from the operator…
> - **Source IP.** The on-chain transaction carries no link to the wallet's IP…

Neither holds in `ForwardOnly`. The operator receives the full transaction bytes
and the wallet's source IP together, and because the transaction lands on the
public chain and pool crossings reveal value in cleartext, they can join
IP → transaction → amount, retrospectively and forever.

Under the ICTM methodology this engagement uses, a user being told they have a
property they do not have is itself the bug — and the same document that states
the protections unconditionally also says, five lines later, that operators "run
the shim in front of their indexer, **and optionally a hub**" (`README.md:70`).
That broader documentation gap is filed separately as
`readme-promises-protection-that-no-user-wallet-or-passerby-can-verify-is-switched-on.md`;
what belongs to *this* issue is narrower and mechanical: **the binary offers no
setting under which a hub-less start is an error**, so an operator who wants to
guarantee the property for their users cannot ask the software to enforce it.

## Technical Details / Code Analysis

The resolver, complete (`shim/src/config.rs:250-289`):

```rust
impl Config {
    /// Resolve the configured transport, rejecting anything ambiguous.
    pub fn hub_selection(&self) -> Result<HubSelection, ConfigError> {
        // Empty entries are dropped, not diagnosed. `ZIS_HUB_NYM=` reaches
        // clap as one EMPTY value rather than as no value at all, because with
        // a delimiter clap splits whatever the variable holds and an unset
        // variable is not the same thing as an empty one. Without this an
        // existing clearnet deployment that templates the new variable in as
        // empty would stop booting, either because both transports look set or
        // because "" looks like a malformed address. ...
        let addresses: Vec<&str> = self
            .hub_nym
            .iter()
            .map(|addr| addr.trim())
            .filter(|addr| !addr.is_empty())
            .collect();

        match (self.hub, addresses.is_empty()) {
            (Some(_), false) => Err(ConfigError::BothTransports),
            (Some(addr), true) => Ok(HubSelection::Http(addr)),
            (None, true) => Ok(HubSelection::ForwardOnly),
            (None, false) => { /* structural + duplicate checks, then Nym(..) */ }
        }
    }
}
```

Everything the resolver *does* check is correct and fails closed, which is worth
stating so the finding is not read more broadly than it is:

- both transports set → `Err(BothTransports)` → `main.rs:112` propagates → the
  process exits (`assemble-caution.sh:185-188` rejects the same combination one
  layer earlier);
- a malformed non-empty Nym entry → `Err(MalformedNymAddress)` → exit
  (`config.rs:276-278`), and `main.rs` then re-parses every surviving address
  through the SDK's authoritative `Recipient` parser, still at startup;
- a duplicate entry → `Err(DuplicateNymAddress)` → exit (`config.rs:279-281`);
- a malformed `ZIS_HUB` → clap `SocketAddr` parse error → exit.

The **only** unguarded cell in that matrix is "neither transport", and it is the
cell that turns the product's privacy off.

The two tests that pin the degenerate inputs as intended
(`shim/src/config.rs:420-446`):

```rust
    #[test]
    fn an_empty_hub_nym_is_the_same_as_an_unset_one() {
        let empty = parse(&["--hub-nym", ""]);
        assert_eq!(empty.hub_nym, vec![String::new()],
            "the field really does hold one empty entry");
        assert_eq!(empty.hub_selection().unwrap(), HubSelection::ForwardOnly);
        ...
        // Whitespace and stray separators are empty too.
        assert_eq!(parse(&["--hub-nym", " , ,"]).hub_selection().unwrap(),
            HubSelection::ForwardOnly);
    }
```

What forward-only actually does with a migration
(`shim/src/intercept.rs:117-124`):

```rust
117    if inspection.treat_as_migration() {
118        if let Some(diversion) = diversion {
119            return divert(&diversion, tx_data).await;
120        }
121        // Forward-only: no hub configured, so behave exactly like the merged
122        // proof of concept and forward the migration to the operator. No
123        // privacy, but no behaviour change until an operator sets `--hub`.
124    }
```

Why the health surface cannot show it (`shim/src/nym.rs:305-309`):

```rust
    /// Whether the shim can currently carry a migration: either diversion is not
    /// configured at all (forward-only, nothing to be down), or the client is up.
    pub fn is_healthy(&self) -> bool {
        !self.0.configured.load(Ordering::Relaxed) || self.0.connected.load(Ordering::Relaxed)
    }
```

`configured` is written from a single site, `shim/src/nym_driver.rs:188`, inside
`run_driver`, which `main.rs` spawns only on the `HubSelection::Nym` arm.
`main.rs:98-100` acknowledges the consequence — "on a forward-only or **clearnet**
shim nothing ever writes, and it honestly reports 'not configured'".

And the deploy wrapper contributes no guard of its own (`deploy.sh:113`):

```sh
[ -n "${HUB_NYM:-}" ] && set -- "$@" --hub-nym "$HUB_NYM"
```

`HUB_NYM` is the only shim input with no `: "${VAR:?…}"` guard, and this AND-OR
list does **not** abort under `set -eu` (verified by execution under `sh`, `dash`
and `bash`: a failing non-final command of an AND-OR list is exempt from `-e`).
The flag is simply omitted. That gap is filed separately as
`deploy-gates-the-hubs-mixnet-readiness-and-never-the-shims-so-a-no-privacy-shim-deploys-clean.md`;
it is noted here only because it is the mechanism by which an empty `HUB_NYM`
reaches `hub_selection` at all.

## Recommendations

1. **Add an explicit fail-closed mode and make it the documented deployment.** A
   `--require-diversion` / `ZIS_REQUIRE_DIVERSION=true` that turns
   `HubSelection::ForwardOnly` into a startup error is a few lines in
   `hub_selection` and removes the entire class. This is the shape
   `hub/REVIEW.md` #11 already asked for, applied to the fail-open that actually
   exists. Because the flag would live in `unit.env`, it would also be **measured
   into the PCRs and disclosed in `.manifest.run_command`** — turning "this
   operator promised to divert" into an attestable fact rather than a claim.
2. **Keep the empty-string accommodation, but make it separable.** The rationale
   at `config.rs:253-261` is sound for backwards compatibility, but "the operator
   set the variable and it was empty" and "the operator never set the variable"
   are distinguishable states. Reject the empty value outright when the require
   flag is set, and warn differently in the two cases otherwise.
3. **Report the transport, not a mixnet-only boolean.** Replace
   `diversion_configured: bool` with `"transport": "forward-only" | "clearnet" |
   "mixnet"`, set on every arm of `main.rs:113-143` rather than only inside the
   mixnet driver. That makes `FORWARD-ONLY` and `CLEARNET-HOP` distinguishable to
   anyone, which they are not today.
4. **Wire the check that already exists into the deploy path.** `smoke.sh:331-347`
   implements exactly this assertion and nothing in the repository invokes or even
   mentions it — `smoke.sh` is referenced by no runbook, no README and not by
   `deploy.sh`. Have `deploy.sh` run it after a shim deploy, and cite it in
   `OPERATORS.md`.
5. **Add "check that a hub is configured, and which one" to the auditor recipe at
   `README.md:71`**, naming `/nym-status` and `.manifest.run_command`. This is the
   cheapest of the five and closes Path A entirely for anyone who follows it.

Cross-references: `forward-only-log-claims-migration-was-diverted.md` (the
per-transaction log line asserts the opposite outcome);
`deploy-gates-the-hubs-mixnet-readiness-and-never-the-shims-so-a-no-privacy-shim-deploys-clean.md`
(the missing `deploy.sh` guard);
`readme-promises-protection-that-no-user-wallet-or-passerby-can-verify-is-switched-on.md`
(the user-facing verification gap);
`shim-config-hub-identity-is-unattested-unobservable-operator-configuration.md`
(the variant where a hub *is* configured and it is the operator's own);
`THREATMODEL.md` scenario `FORWARD-ONLY`.

## Validation Information

**Verdict: CONFIRMED as a real defect. Severity: Medium — downgraded from the
filed High.**

**What was verified true** (all read directly from the target):

| Claim | Verified at |
|---|---|
| `HubSelection` has exactly three members and no "require" flag | `config.rs:205-213`; no `require`/`strict` option anywhere in `shim/src/` |
| Unset, `""`, `" "` and `" , ,"` all resolve to `ForwardOnly` | `config.rs:262-272`; tests at `:420-446` |
| Every *other* cell of the matrix fails closed at startup | `config.rs:274-286`; `main.rs:112` |
| `is_healthy` keeps a forward-only shim at HTTP 200 forever | `nym.rs:305-309`; `proxy.rs:649-653` |
| `configured` is written only from the mixnet driver | `nym.rs:182-183`; single call site `nym_driver.rs:188`; spawned only on the `Nym` arm |
| Forward-only forwards the migration to the operator's indexer | `intercept.rs:117-131` |
| `ZIS_HUB=""` is a hard startup error (the asymmetry) | `config.rs:55-56`; clap 4.6.5 (`shim/Cargo.lock:1113-1114`) applies a present-but-empty env var to the `SocketAddr` parser |
| `deploy.sh:113` has no `:?` guard and does not abort under `set -eu` | executed under `sh`, `dash` and `bash`; all three continue with `--hub-nym` absent |
| `smoke.sh` implements the check and nothing invokes or mentions it | `smoke.sh:331-347`; repository-wide grep for `smoke.sh` returns only `smoke.sh` and `smoke-local.sh` themselves |
| `ForwardOnly` is not runtime-forceable | coordinator open item 6r (G1); `main.rs:112-143` decides once and threads immutably |

**What was verified FALSE, and struck from the finding.** Four claims in the filed
version do not survive; each is now stated as a bounding fact in the Description
rather than as support for the finding:

1. *"Undetectable by any wallet, user or passer-by."* False. `/nym-status` is
   unauthenticated on the wallet-facing listener and reports
   `diversion_configured: false`, and `shim/deploy/caution/OPERATORS.md:415`
   documents that value as an alert with the words *"it is forward-only and hiding
   nothing"*. The related claim that `/nym-status` "appears in the operator
   runbooks only as a way to detect a dead mixnet client" is therefore also false.
2. *"The attestation and reproducibility chain is untouched, which is what makes it
   attractive."* False, per open item 6q: `ZIS_HUB_NYM` is rendered into
   `unit.env` (`caution.hcl.tmpl:161`), `unit.env` is measured into PCR0/PCR1, and
   the environment is served unauthenticated at `.manifest.run_command` of every
   `/attestation` response. A forward-only shim's PCRs differ and its manifest
   visibly lacks the variable.
3. *"Silently selects no privacy."* False on the deploy path: `assemble-caution.sh:414`
   announces it on the terminal and `PROVENANCE` records it in the published tree.
   The two accidental routes a template-following operator could take are
   **hard-refused** with `exit 2` (`assemble-caution.sh:189-192` and `:226-235`),
   because `deploy.env.example:63` ships `NYM_EGRESS` non-empty and
   `deploy.sh:125-127` always passes it. The addendum filed by the
   `deploy.env.example` auditor was correct and is folded into the body.
4. The framing that forward-only is an accident. It is a **documented product
   phase**: `OPERATORS.md:10-16` ("Phase 1 is forward-only, so it adds no privacy
   yet") and `:242` ("Forward-only stays the default: no `--hub-nym`, no
   diversion"). `docs/AVOIDING-FALSE-POSITIVES.md` §6 applies to the mode's
   existence. It does **not** apply to the missing opt-out, which is what this
   issue is now about.

**One fact found during validation that strengthens the finding and was not in the
filed version.** The runbook's own worked *Deploy* command
(`shim/deploy/caution/OPERATORS.md:77-87`) contains neither `--hub-nym` nor
`--nym-egress`. An operator who copies that block verbatim — which is the
documented first deploy — brings up a **no-privacy shim serving live wallets
behind an unchanged public URL**, with diversion added later as a separate "Phase
2" step at `:226-242`. That is deliberate and announced to the operator, but it
means the forward-only state is not an exotic corner: it is the state of every
operator's first deployment, and the users of that endpoint are not told.

**Severity justification — Medium, and why not High or Low.**

*Impact when the state occurs:* total and permanent for the affected endpoint's
users — the operator gets the full transaction bytes plus the source IP, joinable
to the public chain forever. That is High impact.

*Likelihood:* this is what pulls the grade down, and `docs/AVOIDING-FALSE-POSITIVES.md`
§7 is the governing test — *"Will insecure configurations really be used in
practice without the user knowing about the insecurity?"* Unlike the sibling
`DEBUG=1` finding, the polarity here is **not** inverted: `deploy.env.example:18`
ships a populated `HUB_NYM`, so the shipped template selects the *secure* state,
and the assembler refuses the two accidental routes out of it. Reaching
`ForwardOnly` unintentionally therefore requires a deployment that bypasses the
project's own tooling. Reaching it intentionally is available to a deceptive
operator, but it is disclosed by two unauthenticated endpoints and by the attested
manifest, so it is not the invisible attack the filed version described. The
severity guide's Medium band is written for exactly this shape: *"a serious
vulnerability that only exists if the user has configured the application in a
specific, uncommon, way."*

*Why not High:* the three properties that would justify High — insecure by
default, undetectable, and attacker-triggerable — are all absent. The default is
secure, the state is disclosed three ways, and open item 6r establishes that no
outside party can force the mode at runtime.

*Why not Low:* the defect is real and the consequence is a complete loss of the
product's stated purpose for every user of an affected endpoint. A fail-closed
option is a few lines, it is what the project's own review #11 demanded for the
analogous case, and — because the flag would be measured into the PCRs — it would
convert an operator's privacy promise into an attestable fact. The sibling
transport already fails closed on the identical input, which makes this an
inconsistency in the crate's own discipline rather than a deliberate trade-off.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

# `deploy.sh` defaults to `DEBUG=1`, so the repository's one-command deploy ships the configuration that zeroes the attestation PCRs, opens SSH on the operator's host, and turns on per-request wallet-method logging

**Severity**: High
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/deploy.sh:52` (`DEBUG=${DEBUG:-1}`), `:128-134` (the branch that appends `--ssh-key … --debug`), `:206` and `:332` (the two places `DEBUG != 1` gates verifiability); `audit-target/zeronym/deploy.env.example:66` (`DEBUG=1`) and `:73` (`APP_SOURCE=` empty); the effects at `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:567-574`; the prohibitions at `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:82` and `:263`, `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:358-360`, and `audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:186-199`; the claims it defeats at `audit-target/zeronym/README.md:26`, `:33` and `:71`
**Found by agent:** Local (file audit of `deploy.sh`); validated 2026-08-18
**In scope of audit?** Yes — `deploy.sh` and `*/deploy/**` are explicitly in scope: "the reproducible-build and attestation chain **is** the trust model here, so a break in it is a security finding, not tooling noise."

## Description

`deploy.sh` is the repository's one-command deploy path. Its own header presents
it as *the* way to deploy (`deploy.sh:29-31`), and
`shim/deploy/caution/OPERATORS.md:129` sends operators to it ("`zeronym/deploy.sh`
automates this in the right order"). Its debug switch defaults to **on**
(`deploy.sh:52`):

```sh
DEBUG=${DEBUG:-1}
```

and the operator template it tells you to copy ships the same value
(`deploy.env.example:66`):

```
DEBUG=1                             # 1 = debug enclave (SSH console on); 0 = attested
```

An operator who does exactly what the script says — copy `deploy.env.example` to
`deploy.env`, fill in the names, run `./zeronym/deploy.sh` — gets a debug
enclave. Nothing else has to go wrong.

`--debug` does three things at once
(`shim/deploy/caution/assemble-caution.sh:567-573`):

1. flips `enabled = false` → `enabled = true` in the manifest's `debug` block,
   which **disables attestation**;
2. uncomments `RUST_LOG = "zis::proxy=debug,info"`, which turns on a per-request
   log line naming the gRPC method every wallet called;
3. writes the operator's SSH public key into `debug.ssh_keys`, opening port 22 on
   the Nitro **parent host**, from which
   `/var/log/nitro_enclaves/enclave-console.log` is readable.

Both operator runbooks forbid this in bold, in the imperative:

- `hub/deploy/caution/OPERATORS.md:82` and `:263`: **"Never pass `--debug`. Debug
  mode disables attestation."**
- `shim/deploy/caution/OPERATORS.md:358-360`: debug "**disables attestation**, so
  it is a diagnostic only, never the deployed config".
- `shim/deploy/caution/caution.hcl.tmpl:187-188`: "FALSE is the point of the
  exercise: debug mode disables attestation, and an unattested shim proves
  nothing that running it on a laptop would not."

So the shipped default of the deploy script is precisely the configuration the
project's own documentation says must never be deployed. That inversion is the
defect.

**This is default-insecure, not silent.** The operator is warned twice on their
own terminal, and an auditor who runs the documented verification procedure is
*not* fooled. Those calibrations are stated in full below and they are the reason
this is filed as High rather than Critical. What they do not do is give the
wallet user — the party who bears the loss — any signal at all.

## Attack Scenario and Steps

No exploitation step is required of an attacker. The insecure state is reached by
the operator following the repository's own happy path, and the system's
**primary adversary** — the indexer operator, who owns the Nitro parent host — is
handed the capabilities for free.

1. An indexer operator follows `deploy.env.example`'s instructions: copy it to
   `deploy.env`, set `COMPONENT`, `NAME`, `TLS_DOMAIN`, `DNS_DOMAIN`, `BACKEND`,
   `BACKEND_TLS`, `HUB_NYM`, export `VULTR_API_KEY`, run `./zeronym/deploy.sh`.
   They do not touch `DEBUG`, because it is already set to the value the file
   ships with and nothing requires them to make a choice.
2. `deploy.sh:128-130` appends `--ssh-key <their public key> --debug` to the
   assemble invocation.
3. `assemble-caution.sh:567-573` rewrites the rendered manifest as described
   above.
4. The enclave boots and serves wallets. From a wallet's point of view nothing is
   different: the same hostname, the same valid Let's Encrypt certificate, the
   same gRPC behaviour, the same `/healthz` `200 ok`.
5. The operator SSHes to the parent host and reads
   `/var/log/nitro_enclaves/*.log` — the procedure their own runbook gives at
   `shim/deploy/caution/OPERATORS.md:358-360`, and the one the project states it
   used to diagnose "every previous 'boots but never serves' bug". Everything the
   enclave prints is now theirs, including `intercept::log_verdict`'s per-migration
   `orchard_vb` (see the companion issue).

**Attack Requirements and Assumptions:**

- The attacker is the **primary adversary in this system's threat model**: the
  indexer operator who deploys the shim and owns the Nitro parent host. They need
  no special access — they simply do nothing.
- No wallet, and no user, can observe any of this. Nothing on the wallet-facing
  surface differs.
- **The friction is asymmetric and points the wrong way.** The insecure path costs
  zero effort: `deploy.sh:129` needs only `~/.ssh/id_ed25519.pub`, which almost
  every developer already has. The secure path is a hard stop until additional
  infrastructure exists — `deploy.sh:132` refuses to proceed unless `APP_SOURCE`
  is set, which means the operator must first create a **public git repository**,
  arrange **push credentials** for it (`deploy.sh:206-218`), and keep it in sync.
  `deploy.env.example` ships `APP_SOURCE=` empty (`:73`), so the template as
  distributed cannot be deployed attested by editing one value.

## Impact on Users

A wallet user connecting to a debug-deployed endpoint gets **none** of the
product's security properties, and cannot tell.

- **The attestation chain, which is the entire trust model, does not exist.**
  AWS zeroes all PCR values for an enclave launched with `--debug-mode`, and
  Caution documents `debug.enabled` as *"Allows reading enclave console output but
  disables attestation verification"* (both fetched during this audit; see the
  G10 addendum below). So there is nothing to compare, for anyone — not for a
  wallet author, not for a third party, not for the operator themselves.
  `README.md:71` tells auditors to *"fetch its attestation, check the PCRs against
  the AWS Nitro root, reproduce the build and compare hashes"*; on a debug
  deployment step 2 is vacuous and step 3 is impossible, because
  `deploy.sh:128-134` also passes no `--app-source` and `caution verify` then
  refuses outright (`assemble-caution.sh:439-443`: *"Cannot reproduce private code
  deployment"*).
- **The parent host gets the enclave's console, which is what converts the shim's
  in-enclave logging from inert to live.** Coordinator open item 7 established
  from vendor documentation that enclave console output reaches the parent
  **only** in debug mode. So this default is exactly what makes
  `shim/src/intercept.rs::log_verdict`'s INFO line — `orchard_vb`, `ironwood_vb`,
  `sapling_vb`, `expiry`, `inputs`, `outputs`, `tx_len` for **every** transaction
  the shim classifies — readable by the adversary. The operator already sees the
  wallet's source IP at the TCP layer. Joining that to a per-migration value
  balance is the exact link — IP → transaction → amount — that
  `README.md:54` calls "the attack" and that
  `audit-context/AUDIT-INSTRUCTIONS.md` names as the adversary's core goal.
  **This issue is the root defect; the log-discipline findings are its blast
  radius.**
- **Per-request method logging.** `RUST_LOG="zis::proxy=debug,info"` records which
  gRPC method each caller invoked. `caution.hcl.tmpl:178-182` describes that exact
  line as *"exactly the metadata this component exists to deny an operator"*.
- **SSH on the parent.** Beyond the console, this gives the operator (and anyone
  who later compromises that host, or compels it legally) a shell from which to
  observe packet sizes and timing on the enclave's interfaces and to restart the
  enclave at will — which for a hub destroys the RAM-only queue of migrations
  wallets were already told had succeeded, and rotates its Nym identity.

Note that the console log is a **persistent artefact on the operator's disk**, so
the exposure is not only to a deliberately hostile operator: it also reaches
anyone who breaches that host, anyone who obtains a backup, and any legal process
served on the operator. A well-meaning operator who takes the free path exposes
their own users retroactively.

**Detectability, stated precisely.** A third-party auditor who actually runs the
`README.md:71` procedure is **not** fooled into a false positive: `caution verify`
refuses for want of `app_sources`, and the PCRs are zero. There is, however, no
positive signal on any wallet-facing endpoint — `/healthz`, `/nym-status` and the
gRPC surface are byte-identical — and a wallet user has no mechanism at all. The
residual is that an endpoint nobody happens to audit is indistinguishable, to its
users, from an attested one.

## Technical Details / Code Analysis

**The default and the branch it selects** (`deploy.sh:51-53`):

```sh
DNS_TTL=${DNS_TTL:-300}
DEBUG=${DEBUG:-1}
SSH_PUBKEY_FILE=${SSH_PUBKEY_FILE:-$HOME/.ssh/id_ed25519.pub}
```

**The argument list it builds** (`deploy.sh:128-134`):

```sh
if [ "$DEBUG" = 1 ]; then
  [ -f "$SSH_PUBKEY_FILE" ] || die "SSH_PUBKEY_FILE not found: $SSH_PUBKEY_FILE"
  set -- "$@" --ssh-key "$(cat "$SSH_PUBKEY_FILE")" --debug
else
  [ -n "${APP_SOURCE:-}" ] || die "DEBUG=0 (attested) requires APP_SOURCE (public repo URL for caution verify)"
  set -- "$@" --app-source "$APP_SOURCE"
fi
```

The asymmetry is directly visible: the debug branch needs only a file that almost
every developer already has; the attested branch is a hard stop until a public
repository exists.

**What `--debug` does** (`shim/deploy/caution/assemble-caution.sh:559-574`):

```sh
# --debug: flip the enclave into debug mode and turn on per-request shim logging.
# This is a DIAGNOSTIC build, not a shippable one, for two reasons stated in the
# template: debug mode disables attestation (so nothing it runs is provable), and
# RUST_LOG=zis::proxy=debug logs the gRPC method each caller invokes, which is the
# exact metadata the shim exists to deny an operator. …
if [ "$DEBUG" = "true" ]; then
	sed -i.bak \
		-e 's|^      # RUST_LOG = "zis::proxy=debug,info"|      RUST_LOG = "zis::proxy=debug,info"|' \
		-e 's|^    enabled  = false|    enabled  = true|' \
		"$DEST/caution.hcl"
	rm -f "$DEST/caution.hcl.bak"
	echo "==> DEBUG build: attestation OFF, SSH console ON, per-request logging ON. Diagnostic only."
fi
```

The comment is an accurate description of a configuration the enclosing tool then
makes the default. (`assemble-caution.sh:114-118` additionally *requires*
`--ssh-key` whenever `--debug` is given, so debug and an open SSH console are
inseparable.)

**The verifiability gates**, both of which the default fails
(`deploy.sh:206` and `:332`):

```sh
if [ "$DEBUG" != 1 ] && [ -n "${APP_SOURCE:-}" ]; then
  …
  log "publishing the app-source to $APP_SOURCE_PUSH (tag $APP_SOURCE_TAG) ..."
```

```sh
verify     : $( [ "$DEBUG" != 1 ] && [ -n "${APP_SOURCE:-}" ] && printf '%s @ %s — run: caution verify' "$APP_SOURCE" "${APP_SOURCE_TAG:-}" || printf 'n/a (debug deploy, not attested; no app-source published)' )
```

**One point in the code's favour, checked and confirmed:** the comparison is
against the literal string `1`, so a mistyped value (`DEBUG=true`, `DEBUG=yes`,
`DEBUG=0 `) falls into the `else` branch and fails *toward* the attested path. The
only values that produce a debug enclave are a deliberate `1` — or the absence of
the variable entirely, which is the default under audit.

**The template's comment names only one of the three effects**
(`deploy.env.example:66`): it states the SSH console explicitly, attestation only
by implication ("0 = attested"), and does not mention the `RUST_LOG` effect at
all. `deploy.sh:22` compounds this: *"DEBUG=1 here is now about the SSH console
alone."* Read as a statement about what `DEBUG=1` *does*, that is false, and the
same sentence's earlier clause (*"--debug turns attestation OFF"*) shows the
author knew. The charitable reading is that it is a statement about *motivation*
(the enclave console is no longer needed to read a hub's Nym address), but it is
the second place a reader is pointed at the SSH console and away from the other
two effects.

**Vendor confirmation that debug removes attestation entirely, not merely weakens
it** (primary sources fetched during the G10/G12/G13 global audit):

- `docs.caution.co/guides/verify-an-app/`, call-out box **"Debug mode cannot be
  verified"**: *"AWS Nitro Enclaves **zero out PCR values** in debug mode. Remove
  the `debug` block from `caution.hcl` and redeploy before verifying a production
  app."* Prerequisites: *"You need … **A deployed Caution app running outside
  debug mode**."*
- `docs.caution.co/reference/caution-hcl/`, field `debug.enabled`: *"Enable debug
  mode. Allows reading enclave console output but disables attestation
  verification."*
- Caution's `terraform/modules/aws/nitro-enclave/user-data.sh` installs
  `capture-enclave-console.sh` as a systemd unit inside a
  `%{ if debug_mode == "true" ~}` block — i.e. the parent host actively captures
  the console to disk in debug mode.

## Recommendations

1. **Invert the default**: `DEBUG=${DEBUG:-0}`, and set `DEBUG=0` in
   `deploy.env.example`. This is the single-line fix and it removes the defect.
2. **Make debug an explicit, per-invocation act rather than a config-file value.**
   Require it on the command line (`./deploy.sh --debug deploy.env`) or via an
   obviously-dangerous variable name such as
   `I_UNDERSTAND_THIS_DISABLES_ATTESTATION=1`, so an operator cannot arrive at it
   by leaving a template field alone.
3. **Remove the friction asymmetry so the secure path is not the expensive one.**
   Allow `DEBUG=0` without `APP_SOURCE`, printing the "not independently
   verifiable" warning `assemble-caution.sh:439-443` already emits. An
   attested-but-unpublished enclave is strictly better than an unattested one, and
   today the script forces the operator to choose between *doing the repo work*
   and *turning attestation off*.
4. **List all three effects** in `deploy.env.example:66` and `deploy.sh:22`:
   attestation off, SSH console on, per-request gRPC-method logging on.
5. **Give the deployment an externally observable signal.** Surface `debug.enabled`
   (or simply the fact that the PCRs are zero) in what the endpoint publishes at
   `/attestation` or `/nym-status`, so a wallet, a monitor, or a passer-by can tell
   a diagnostic deployment from a real one without holding operator credentials.

## Validation Information

**Verdict: CONFIRMED. Severity: High.**

Every mechanical claim was re-verified against the target:

| Claim | Verified at |
|---|---|
| `DEBUG=${DEBUG:-1}` | `deploy.sh:52` — read directly |
| Template ships `DEBUG=1`, `APP_SOURCE=` empty | `deploy.env.example:66`, `:73` |
| `--debug` ⇒ `--ssh-key` required | `assemble-caution.sh:95`, `:114-118` |
| `--debug` flips `enabled = false` → `true` and uncomments `RUST_LOG` | `assemble-caution.sh:567-573` |
| Template's `debug { enabled = false }` and its rationale | `caution.hcl.tmpl:186-199` |
| Runbooks forbid `--debug` | `hub/…/OPERATORS.md:82`, `:263`; `shim/…/OPERATORS.md:358-360` |
| `RUST_LOG` line called "exactly the metadata this component exists to deny an operator" | `caution.hcl.tmpl:178-182` |
| `DEBUG=1` skips the app-source publish and renders `verify : n/a` | `deploy.sh:206`, `:332` |
| Console is readable on the parent in debug and *only* in debug | Coordinator open item 7 (AWS + Caution vendor docs); corroborated in-tree by `shim/deploy/README.md:289` ("neither readable on an attested enclave, which has no console") and by `shim/…/OPERATORS.md:358-360` describing the console as how every past boot bug was diagnosed |
| `deploy.sh` is a path operators are pointed at | `deploy.sh:29-31`; `shim/…/OPERATORS.md:129` |

**The `docs/AVOIDING-FALSE-POSITIVES.md` §7 counter-argument, stated and
answered.** §7 warns against grading configuration-dependent issues highly, and
gives as its canonical example: *"Debug mode information leakage (marked High →
should be Low) — Only with `DEBUG=true` … **Production uses `DEBUG=false` by
default**."* That example does not apply here, because the polarity is inverted:
production does **not** use `DEBUG=false` by default; `deploy.sh` and the shipped
template both select `DEBUG=1`. §7's own contrasting "Real Issue" line is exactly
this case — *"insecure protocols **enabled by default**"* — as is §4's
(*"Real vulnerability would be: same issues … **enabled by default**"*). The
related §7 carve-out for *"insecure flags that communicate their own security"*
also does not apply, because that carve-out is about a flag the user
affirmatively chooses (`--dangerously-skip-permissions`); here the user chooses
nothing and inherits the dangerous state by omission.

**Severity justification — High, and why not Critical or Medium.**

*Impact:* total. Every security property the product advertises is void
simultaneously: attestation (PCRs zeroed, so the check is not weakened but
vacuous), reproducibility (`caution verify` refuses), console isolation (the
parent captures the enclave's stdout to disk, delivering per-migration value
balances to the primary adversary), and request-metadata privacy (`zis::proxy`
per-request method logging). The affected population is every wallet user of
every endpoint deployed this way, and the harm is retrospective and permanent
because the chain is permanent and the console log persists.

*Likelihood:* high. It is the shipped default of the repository's one-command
deploy and of the config template operators are told to copy; the secure
alternative is gated behind creating a public git repository and provisioning
push credentials; and the audience is third-party indexer operators, onboarding
of whom began 2026-08-10.

*Why not Critical:* no funds are stolen or destroyed; the operator is warned twice
on their terminal (`assemble-caution.sh:573` and the `verify : n/a` banner at
`deploy.sh:332`); both runbooks forbid `--debug` in bold; the canonical operator
runbook `shim/deploy/caution/OPERATORS.md` documents the manual assemble path
with `--app-source` and never with `--debug`; and any third party who runs the
documented verification is definitively **not** fooled. This is a
default-insecure, loudly-announced configuration, not a silent backdoor.

*Why not Medium:* the two facts that would normally justify a downgrade — "the
secure setting is the default" and "the affected party can tell" — are both false
here. The insecure setting *is* the default, and the affected party (the wallet
user) has no signal whatsoever. Warnings addressed exclusively to the party who
benefits from ignoring them do not protect the party who is harmed.

**Corrections made during validation.** The pre-validation draft's framing was
sound; three points were tightened rather than changed:

1. The consequence of debug is stronger than "verify refuses": AWS zeroes all
   PCRs, so there is nothing for anyone to compare, ever. Stated in the impact
   section.
2. The "silent" adjective was removed throughout in favour of "default-insecure",
   and the operator-facing warnings are now stated in the Description rather than
   only in a mitigations paragraph, so the finding cannot be read as claiming a
   covert failure.
3. Added the persistence angle — the debug console is written to disk on the
   parent host (`capture-enclave-console.sh`), so the exposure survives the
   session and reaches breach, backup and legal-process adversaries, not only a
   deliberately hostile operator.

**Cross-references.** This issue is the **parent** of every log-discipline
finding: `log-verdict-logs-migration-value-balance-at-info.md` (confirmed, High)
and `hub-per-admission-info-log-is-a-real-time-per-entry-arrival-feed.md` are
exploitable *because of* this default and are inert without it (coordinator open
item 7). The report should present them in that order.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

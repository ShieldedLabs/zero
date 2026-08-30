# The hub's Nym address is the recipient's key, and nothing anchors it: the canonical copy is fetched over plain WebPKI on operator-controlled DNS and is never compared against the attestation the project already publishes

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/config.rs:65-77` (`ZIS_HUB_NYM` is a list), `:255-289` (`hub_selection`), `:292-309` (`is_nym_address`); `audit-target/zeronym/shim/src/nym.rs:595-690` (`NymHandle::submit`), `:733-784` (`each_target`); `audit-target/zeronym/shim/src/nym_driver.rs:608-624` (`send_frame`); `audit-target/zeronym/shim/src/wire.rs:28-34` (`SubmitV1` layout); `audit-target/zeronym/hub/src/server.rs:62-70`, `:469-479` (`GET /nym-address`); `audit-target/zeronym/deploy.sh:255-259` (the bare `curl`), `:283-318`; `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:131-145` (the handoff), `:186-189` (poll and alert), `:236-245` (the rotation runbook and "verify after"); `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:237-239` ("the authoritative copy"), `:312`, `:370-378`; `audit-target/zeronym/README.md:71`, `:77`, `:92`
**Found by agent:** Global with focus area of G8 (hub-as-adversary / shim-as-adversary), taken with G27; recorded as coordinator item 6z
**In scope of audit?** Yes

## Description

Every diverted migration in the system is delivered to one value: the Nym address in
`ZIS_HUB_NYM`. That address is not a name that gets resolved and then authenticated — it
**is** the recipient's public key material (`identity.encryption@gateway`). The Sphinx
payload is encrypted end to end to the encryption key inside it, which is a genuine
strength of the transport: no mix node and no gateway can read a submission. But it
means the transport's confidentiality is confidentiality *to whoever generated that
address*. Substituting the string substitutes the key, and there is no certificate, no
signature and no attestation over it.

The project knows this hop needs peer authentication: `OPEN-QUESTIONS.md` §3 records
that under STEVE "**the shim verifies the hub**, extracts its key, and derives a session
key. That is enough for privacy." STEVE is designed, not built (`README.md:92`), and its
absence is a stated residual that this issue does not re-report.

What this issue reports is the **interim substitute the deployed system actually runs
on**: the address is handed over out of band, and the only mechanism the documentation
offers for checking it is unauthenticated — even though the project already owns an
authenticated one and applies it two lines away.

1. **The documented authoritative source is plain WebPKI over operator-controlled
   DNS.** `shim/deploy/caution/OPERATORS.md:237-239` calls
   `https://<hub-domain>/nym-address` "the authoritative copy"; `deploy.sh:256` fetches
   it with a bare `curl -s ... "https://$TLS_DOMAIN$1"`. The presented leaf is **never**
   compared against `user_data.tls.certfp` in the hub's COSE-signed attestation — the
   binding coordinator item 6q established **does** exist and **does** work on this
   platform, and which `caution verify` performs when it is run.
2. **No shim runbook tells a shim operator to verify the hub at all.** Every
   `caution verify` instruction in `shim/deploy/caution/` targets the *shim's own*
   endpoint; the hub's `caution verify` line
   (`hub/deploy/caution/OPERATORS.md:100`) is a self-check for the hub operator.
3. **The alternative source is verbatim "a human message."**
   `hub/deploy/caution/OPERATORS.md:143-145`: *"Hand that string to each shim operator …
   There is no discovery mechanism — the handoff is a human message."*
4. **The value is required to change, on an operational schedule, with no key
   continuity.** A diskless hub mints a fresh identity on restart, so
   `shim/deploy/caution/OPERATORS.md:370-378` and `hub/deploy/caution/OPERATORS.md:186-189`
   instruct operators to poll `/nym-address` and alert on change, then re-bake. Nothing
   signs the new identity with the old one, so a shim operator has no way to distinguish
   a legitimate rotation from a substitution. The rotation runbook
   (`hub/deploy/caution/OPERATORS.md:236-245`) ends with "**`caution verify` … does NOT
   belong on the critical path — restore service first, verify after**", i.e. the entire
   fleet re-points at a fresh, unverified identity by design and the one available check
   is explicitly deferred.
5. **The attestation cannot rescue it.** The shim's manifest faithfully attests whatever
   `ZIS_HUB_NYM` was baked in; if the value was poisoned at source, the attestation is
   *valid* and proves only that the shim will send to that address. An auditor who
   performs the check that
   `auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`
   recommends obtains a base58 string and has nothing to compare it against except the
   same unauthenticated endpoint.

Consequence: a party who controls the hub's DNS name (or obtains a certificate for it),
and the hub operator themselves, can publish an address whose key they hold and thereby
receive **every migration on the network, in plaintext, at divert time, with per-shim
sender tags**, indefinitely, with an entirely honest shim operator and nothing
observable changing anywhere.

## Attack Scenario and Steps

**Variant A — substituted address (the primary case).**

1. The attacker obtains control of the hub's DNS zone or registrar, or a certificate for
   the name (which follows from DNS control via ACME) — or *is* the hub operator, who
   needs neither, since they author both the endpoint and the handoff message.
2. The attacker runs a stock `nym-sdk` client and serves its address `R_att` at
   `https://<hub-domain>/nym-address` (and/or sends it as the handoff message).
3. Shim operators re-assemble with `--hub-nym R_att` — which is routine, because the
   address is *supposed* to change on every hub restart, and a forced restart is itself
   cheap (see `hub-liveness-probe-reads-its-own-send-backlog-as-gateway-silence-so-any-stranger-can-drive-the-fresh-identity-fleet-kill.md`,
   confirmed). `Config::hub_selection` (`shim/src/config.rs:255-289`) shape-checks the
   string and boots.
4. Every divert now encrypts the `SubmitV1` frame to `R_att`'s key. `R_att` decodes it
   (`shim/src/wire.rs:28-58`, byte-identical in `hub/src/wire.rs`, and the wire format is
   published in-tree), obtaining the raw Zcash transaction, its arrival time and the
   shim's stable `AnonymousSenderTag`.
5. `R_att` re-frames each transaction and forwards it to the real hub from its own
   client, and relays `LookupV1`/`LookupReplyV1` in both directions. Everything downstream
   is unchanged: the real hub queues, batches and publishes; wallets get their txids;
   `/healthz` is 200; `/nym-status` reports `mixnet_connected: true`. The only cost is a
   small added latency, well inside the shim's 90 s lookup budget.
   A rogue that does not want to relay can broadcast each transaction itself and answer
   lookups from its own copy; the on-chain result is a smaller batch, which is
   indistinguishable from today's expected batch size of 0-1
   (`hub/src/batcher.rs:412-419` already warns about that on every honest flush).

**Variant B — appended address.** `ZIS_HUB_NYM` is a comma-separated list and a submit
goes to **every** entry (`shim/src/nym.rs:642`, `for target in 0..targets`), so handing
out `R_real,R_att` gives the attacker a silent copy with no relaying at all. This variant
is *mechanically* the same capability that
`shim-submits-every-migration-to-every-configured-hub-…` (confirmed High) owns for a
hostile shim operator; it is noted here only because an unanchored reference value lets
someone who is **not** the shim operator plant it. One caveat that keeps Variant A the
primary case: with two addresses, `each_target` (`shim/src/nym.rs:733-784`) sends lookups
to each in turn and only a **timeout** sweeps on (`:770-781`), so a silent second address
costs ~90 s on half of all lookups — degraded latency an operator might notice, whereas
Variant A has no such tell.

**Attack Requirements and Assumptions:**

- The attacker needs **one** of: control of the hub's DNS zone or registrar; a
  certificate for the hub's name; or to be the hub operator. It does **not** need the
  shim operator to be malicious, the enclave to be compromised, the mixnet to be
  compromised, or any code defect.
- A **spoofed handoff message alone is weaker than it looks** and is not the load-bearing
  vector: the shim runbook designates the `/nym-address` endpoint "the authoritative
  copy", so an operator who cross-checks there defeats a message-only spoof. The
  substitution has to reach the endpoint (or the operator has to skip the cross-check,
  which the rotation runbook's time pressure makes likelier).
- The opportunity is **recurring by design**: it exists at every hub restart, and
  `hub/deploy/caution/OPERATORS.md:244-245` explicitly moves verification off the
  critical path during exactly that window.
- What makes it hard to notice: there is no key continuity between an old and a new hub
  identity, so a legitimate rotation and a substitution are the same event to a shim
  operator; and both variants are indistinguishable from correct operation at every
  surface the system exposes.

## Impact on Users

If it happens, every user of every shim in the fleet loses the whole protection at once:
the attacker holds the plaintext of each Orchard-touching transaction, its exact length,
its arrival instant, and a stable per-shim label. Joined against the public chain that is
`IP → transaction → balance` for any user whose shim the attacker also observes, and
`operator → transaction → balance` for the rest. Nothing a wallet, a user, an auditor
following `README.md:71`, or a shim operator following their own runbook can see
distinguishes it from correct operation.

It also makes the reachability precondition of
`hub-unauthenticated-pre-publication-transaction-disclosure.md` real: the attacker holds
candidate txids for unpublished migrations, which per coordinator item 6z is otherwise
not reachable by any adversary other than the wallet itself.

`README.md:77` ("Green boxes are attested enclaves, the only things that ever see a
migration in cleartext") is stated unconditionally, and `README.md:71`'s auditor recipe
lists four checks, none of which is about the destination. The "Not protected" list at
`README.md:30-36` does not mention that the hub's identity is an unauthenticated
configured value; the closest disclosure is `README.md:92`'s "Designed, no code yet: the
STEVE handshake, the encrypt-to-hub-key layer", from which a reader must infer the
consequence themselves.

## Technical Details / Code Analysis

**The address is key material, and only its shape is checked.** `shim/src/config.rs:292-309`:

```rust
fn is_nym_address(addr: &str) -> bool {
    let Some((keys, gateway)) = addr.split_once('@') else { return false; };
    let Some((identity, encryption)) = keys.split_once('.') else { return false; };
    !gateway.is_empty() && !identity.is_empty() && !encryption.is_empty()
        && !gateway.contains('@') && !encryption.contains('.')
}
```

Its own doc comment says it is "deliberately shallow ... leaves the real parse (base58,
key lengths) to the SDK". Nothing above it establishes *whose* keys these are.
`Config::hub_selection` (`:255-289`) accepts any number of such entries, rejecting only
empty and duplicate ones.

**Confidentiality is to the key in the address, not to the hub.** The Sphinx payload is
encrypted to the recipient's encryption key (verified in the pinned `nym-sdk` tree by the
G15 pass: `common/nymsphinx/src/preparer/mod.rs:165-180`), so the mixnet leg is genuinely
private — against everyone except the holder of the key named in `ZIS_HUB_NYM`. Inside
that envelope the frame is cleartext; there is no application-layer AEAD, because that
is the unbuilt STEVE (`shim/src/wire.rs:28-34`):

```text
SubmitV1, exactly FRAME_BYTES:
  0    magic    4   b"ZNS1"
  4    nonce   16   request nonce, from OsRng
  20   tx_len   4   u32 big-endian
  24   tx       tx_len bytes
  ..   padding  zeros to FRAME_BYTES
```

**The provenance chain, end to end.** `deploy.sh:255-259`:

```sh
  hub_get() {
    _resp=$(curl -s --max-time 20 -w ' %{http_code}' "https://$TLS_DOMAIN$1") || true
    _code=${_resp##* }
    _body=${_resp% *}
  }
```

Default WebPKI verification of `$TLS_DOMAIN`, nothing else. The loop at `:283-308` checks
only the HTTP status, the *shape* of the body (`is_nym_address`, `:265-280`), and that
`/nym-status` reports `mixnet_connected == true`; on success `:316` prints the string to
stdout for a caller to bake into a shim. No attestation is fetched and `caution verify`
is never invoked on this path. The hub side of the handoff
(`hub/deploy/caution/OPERATORS.md:131-145`) is a bare `curl https://<hub-domain>/nym-address`
followed by the human-message sentence; the shim side
(`shim/deploy/caution/OPERATORS.md:237-239`, `:312`) designates that URL authoritative and
the config-table default.

**Why the attestation does not cover this, even after coordinator item 6q.** 6q
established that `unit.env` is measured into PCR0/PCR1 and that the whole environment is
served at `.manifest.run_command` of `/attestation`, and that `caution verify` genuinely
binds the enclave to its TLS leaf via `user_data.tls.certfp`. Both are true and both are
beside the point here:

- the shim's attestation proves the shim sends to `R_att` — which is precisely the harm,
  not a defence against it;
- `caution verify` against the **hub** *would* detect Variant A, because it compares the
  leaf of the connection that served `/attestation` against the COSE-signed `certfp`.
  It is never run by a shim operator, appears in no shim runbook, and is explicitly
  deferred in the one procedure during which every shim in the fleet adopts a new
  identity.

**What a fix looks like with today's parts.** `caution verify --attestation-url
https://<hub-domain>/attestation` first, pin the leaf, then read `/nym-address` over a
connection presenting that same leaf. Because the attested hub binary serves only the
address its own in-enclave Nym client minted (`hub/src/server.rs:62-70`, `:469-479`,
filled by the driver), that sequence *does* anchor the address to the attested enclave.
The machinery exists; nothing in the repository composes the two steps.

## Recommendations

In rough order of cost:

1. **Make the fetch attested.** Change `deploy.sh`'s `hub_get` and both OPERATORS
   runbooks so `/nym-address` is only ever read over a connection whose leaf has been
   compared against the hub's `/attestation` `user_data.tls.certfp`. Remove "verify
   after" from `hub/deploy/caution/OPERATORS.md:244-245`: the whole fleet is adopting a
   new key in that window, which is the one moment verification is load-bearing.
2. **Bind the Nym address to the attestation itself.** Have the platform place the hub's
   current Nym address in the COSE-signed `user_data` (alongside `tls.certfp`), the same
   way `caddy-certfp.sh` gets the leaf fingerprint there, so the address is signed by the
   Nitro key rather than merely served over TLS. A shim operator then verifies the
   address, not the endpoint that served it.
3. **Give the rotation key continuity.** Have the hub sign its new Nym address with the
   previous identity (or with a long-lived offline consortium key) and publish the
   signature at `/nym-address`, so a legitimate rotation is distinguishable from a
   substitution without re-running the whole attestation flow.
4. **Add a shim-side check on the list.** At minimum, log the configured addresses and
   their count at startup (today `shim/src/main.rs`'s mixnet arm logs neither), and
   refuse to start with more than one `--hub-nym` entry unless an explicit
   `--hub-nym-failover` flag is passed.
5. **Correct the claims.** `README.md:77` and `README.md:71` should state that the hub's
   identity is an unauthenticated configured value today, and what an auditor can and
   cannot conclude from an attestation that contains it.

## Validation Information

**Verdict: CONFIRMED. Severity: Medium — the filed grade is upheld and the case for High
argued inside the issue is decided against, for the reasons below.**

### Every mechanical and documentary claim re-verified against the target at HEAD

| Claim | Verified at |
|---|---|
| Only the shape of a `ZIS_HUB_NYM` entry is checked; any number of entries accepted | `shim/src/config.rs:292-309`, `:255-289` |
| A submit fans out to every entry, with no ack and no rotation | `shim/src/nym.rs:642` (`for target in 0..targets`), `:652` (ack receiver dropped at construction) |
| Lookups rotate a cursor and only a **timeout** sweeps to the next address | `shim/src/nym.rs:743-781` |
| A `LookupReply::Error` fails closed immediately with no sweep | `shim/src/hub.rs:264-266` |
| The frame carries the transaction with no application-layer AEAD | `shim/src/wire.rs:28-34` |
| Sphinx encrypts to the key **in the address**, so substituting the string substitutes the recipient | `nym-sdk` `common/nymsphinx/src/preparer/mod.rs:165-180` (established by the G15 pass) |
| `deploy.sh` fetches `/nym-address` with a bare `curl` over WebPKI and never touches the attestation | `deploy.sh:255-259`, `:283-318` |
| "the authoritative copy" | `shim/deploy/caution/OPERATORS.md:239` |
| "There is no discovery mechanism — the handoff is a human message" | `hub/deploy/caution/OPERATORS.md:144` and again at `:238` |
| "`caution verify` … does NOT belong on the critical path — restore service first, verify after" | `hub/deploy/caution/OPERATORS.md:244-245` |
| Poll-and-alert is the documented response to rotation; no key continuity anywhere | `shim/deploy/caution/OPERATORS.md:376`, `hub/deploy/caution/OPERATORS.md:186-189` |
| `certfp` appears **nowhere** in the target | grep: zero occurrences (also recorded under coordinator item 7k) |
| README states the cleartext claim unconditionally and lists four unrelated auditor checks | `README.md:77`, `:71`; the "Not protected" list at `:30-36` does not mention the destination |
| A stranger can force the rotation that triggers the fleet-wide re-handoff | `hub-liveness-probe-…-fresh-identity-fleet-kill.md` (confirmed Medium) |

### Why this is a real, separate finding and not a re-report of a stated residual

The audit instructions record "there is no authentication on the shim→hub channel today"
as a self-declared limitation, and `AUDIT-INSTRUCTIONS.md` says a stated residual must not
be re-reported. That rule was applied deliberately, and this issue survives it:

- The **residual** is that the channel has no cryptographic peer authentication (STEVE,
  encrypt-to-hub-key). This issue does not claim otherwise and does not ask for STEVE.
- The **finding** is about the *interim substitute the project chose instead*: a value
  distributed out of band whose only documented reference is unauthenticated, while an
  authenticated reference (`certfp`, verified by 6q to exist and work) is used two lines
  away for a different purpose and never composed with this one. That is a concrete
  deploy-script and runbook defect with a concrete fix, not a restatement of "STEVE is
  not built". Recommendation 1 costs a few lines of `deploy.sh` and two paragraphs of
  runbook.
- The audit instructions' own carve-out applies too: `README.md:77` claims more than the
  residual allows, and `README.md:30-36` omits it from "Not protected".

### Why Medium and not High — the case for High, decided

The filed text argued for High on impact (total defeat of the core guarantee, fleet-wide,
undetectable) plus a cheap vector (spoofing a base58 string in a chat message). The impact
half is correct. The likelihood half does not carry:

1. **The cheapest vector is defeated by a check the documentation already prescribes.**
   The shim runbook designates `https://<hub-domain>/nym-address` "the authoritative
   copy", so a spoofed handoff *message* fails against any operator who does the
   cross-check the same document tells them to do. What is left as the load-bearing
   precondition is control of the hub's DNS/PKI, or being the hub operator.
2. **DNS/registrar/certificate control over the hub's domain is a genuine position**, not
   a capability a stranger has. This audit has consistently held findings whose
   precondition is a configured-endpoint or infrastructure position at Medium rather than
   High — coordinator item 6p's bound, applied to both tip issues — and the same standard
   governs here.
3. **The hub-operator route is real but is the cheapening of an already-stated residual.**
   The audit's own threat model records W9 ("the hub sees every diverted transaction in
   cleartext, and this is total ... compromise of the hub, or a legal order served on
   whoever runs it, exposes every migration"). Substitution makes that outcome reachable
   without touching the enclave, which is a genuine and unowned observation — it is why
   this issue is confirmed rather than merged away — but it does not create a new class
   of victim for that party.
4. **Nothing here is reachable by an anonymous party.** Compare the confirmed High it is
   most often confused with, whose attacker is adversary #1 (the shim's own operator, a
   party whose hostility is the product's founding premise) and needs only one comma in a
   config file.

Medium is therefore the honest grade: maximal impact, real and recurring opportunity,
undetectable by any documented procedure — held below High by a precondition that is a
real infrastructure position, and by the fact that the underlying absence of peer
authentication is disclosed by the project.

### Anti-double-counting — checked against all four neighbours

- **`shim-submits-every-migration-to-every-configured-hub-…` (confirmed High)** — a
  *different mechanism* (append at the shim, by the shim's own operator) and a different
  victim set (that endpoint's users). This issue is *substitute at the source*, by
  someone who is not the shim operator, affecting every shim at once. Neither fix
  addresses the other: exact whole-list equality against a published value does nothing
  if the published value is the attacker's, and anchoring the published value does
  nothing about an operator appending an extra entry. Variant B is described here only to
  show the reference-value defect also enables the *other* issue's capability from
  outside; the append capability itself stays credited there.
- **`auditor-recipe-omits-…` (confirmed Medium)** — owns "no document tells anyone to
  look". This issue owns the orthogonal half: *even when someone looks, the comparison
  terminates in an unanchored value*. That constraint is already recorded in the recipe
  issue's validation as a constraint on its Recommendation 1; this file is where the
  constraint's own fix (Recommendations 1-3 above) lives. The recipe issue's severity is
  not moved by this one.
- **`operators-runbook-attributes-the-hub-destination-to-…` (confirmed Low)** — owns two
  operator-facing sentences that falsely assure the reader the destination is bound by
  the binary hash and egress rules. Documentation-only, no independent attack path. This
  issue does not restate those lines.
- **`attested-tls-binding-is-verified-once-by-hand-if-ever-…` (confirmed Medium)** — owns
  the certfp binding being *time-of-check-only*. This issue is the case where the binding
  is **never applied at all** to a different value. Different artefact, different fix.
- **`hub-nym-driver-automatic-fresh-identity-permanently-invalidates-every-shim.md`
  (confirmed)** — owns the *availability* consequence of the same mandatory rotation;
  this is its *authenticity* consequence. The forced-rotation primitive
  (`hub-liveness-probe-…-fleet-kill.md`, confirmed Medium) is cited here as the thing that
  manufactures the handoff window, not re-counted.

### Corrections applied against the filed text

- **The "cheapest vector" argument was struck.** The filed text leaned on spoofing the
  human message; the shim runbook's own "authoritative copy" instruction defeats that in
  isolation, and saying otherwise would overstate the finding. The load-bearing
  preconditions are now stated as DNS/PKI control or the hub operator.
- **"`SubmitV1` is plaintext" was made precise.** As written it could be read as
  contradicting a positive the report must state — that the mixnet leg is end-to-end
  encrypted and no mix node or gateway can read a submission (G15). The frame is
  cleartext *inside* the Sphinx envelope, i.e. readable only by the holder of the key
  named in `ZIS_HUB_NYM`, which is exactly why substituting that string is the whole
  attack.
- **Variant ordering was inverted.** The filed text led with the appended-address variant,
  which is mechanically the confirmed High's capability and carries a ~90 s lookup-latency
  tell on half of all lookups. Substitution is the variant this issue actually owns and is
  now primary.
- **The claim that a silent rogue "need not send a single packet to stay invisible" was
  softened** to name the latency cost, verified at `shim/src/nym.rs:743-781` and
  `shim/src/nym.rs:48-71` (`REQUEST_TIMEOUT = 90 s`, overridable via
  `ZIS_LOOKUP_TIMEOUT_SECS` at `shim/src/config.rs:115-116`, and documented at
  `shim/deploy/caution/OPERATORS.md:316` as *multiplying* by the number of addresses).
- **The fix was re-grounded in parts that exist.** The filed Recommendation 1 asked the
  platform to add the address to `user_data`; validation established that a
  certfp-pinned fetch of `/nym-address` from the attested hub achieves the same binding
  with today's artefacts, so that is now Recommendation 1 and the platform change is
  Recommendation 2.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).

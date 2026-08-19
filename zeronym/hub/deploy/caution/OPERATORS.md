# Running the zero-indexer-hub (operator guide)

> **Status, 2026-08-17.** The live pair is **hub-6 + shim-9, in clearnet mode**
> (`--http-submit`; the shim reaches the hub over HTTPS, not the mixnet). It is
> attested, publicly verifiable, and fast: lookups 1.6–2.5 s, a diverted
> migration retrievable from the hub's queue in ~1 s. Nym mode is built, tested,
> and works end to end — but every hub run inside a Nitro enclave answers a
> lookup in 30–90 s, and a debug console traced that to the enclave's networking
> (a `socat` TAP-over-vsock relay that adds latency per frame; the nym-sdk's rate
> controller correctly throttles to ~8 packets/s in response). Caution confirmed
> the diagnosis and v2 enclaves with directly attached NICs are their fix. Until
> then: clearnet. See "Clearnet mode" below for exactly what that keeps and gives
> up. This block is the thing to update when Nym comes back.

The hub is where diverted migrations land. Shims divert Orchard-touching
`SendTransaction`s to it — over the **Nym mixnet** in Nym mode, or over HTTPS in
clearnet mode; the hub holds them in a short-lived queue, then broadcasts them
to the Zcash network through indexers you configure.

**Read this first: of the two components, this is the one that sees plaintext.**
The shim's enclave hides migrations *from the indexer operator*. The hub is where
that content arrives in the clear. It is therefore the component whose
attestation matters most — an unattested hub proves nothing that running the
binary on a laptop would not, while being trusted with exactly the data the
system exists to protect.

**One hub, today.** The current DEPLOYMENT runs a single hub, which is why there
is no hub directory and a single address baked into every shim.

**But the target model is REPLICATION, not failover** (Taylor Hornby's rule,
2026-08-17, recorded in `PRODUCTION.md`). Multiple hubs are fine for availability
provided every hub receives IDENTICAL information: every shim submits every
migration to EVERY hub, unconditionally. Zcash handles two hubs broadcasting the
same batch fine. Lookups may pick a hub at random and retry elsewhere, because
every hub can answer.

What must never be built is STATEFUL ROUTING -- any scheme where a shim chooses
its hub based on which one looks healthy. `REVIEW.md` #6 reached this
independently and states the sharper version of the attack: preferring a primary
creates an ISOLATION ORACLE, because a brief, targeted outage of the primary
during one migration's retry isolates THAT migration. Taylor's rule states the
broader one: an attacker who can make Hub1 look down to Shim1 and Hub2 look down
to Shim2 has partitioned the anonymity set, without touching either hub. Both
land on the same instruction -- submit to all hubs, always, never conditioned on
health. That is why the section below is titled as
the single-hub degraded case rather than as a failover design: failover is the
wrong shape, not merely an unbuilt one.

## What the hub does, precisely

- **Receives** submissions over its own Nym mixnet client. It cannot reply to a
  sender who exposed their address instead of sending anonymously, and drops such
  messages rather than queueing them.
- **Queues** them in RAM, bounded by an explicit byte budget (64 MiB), which is
  what bounds this enclave's memory against someone who simply keeps submitting.
- **Flushes** the batch on a cadence, broadcasting every member to **every**
  configured indexer — a migration that entered only one mempool is one outage
  away from never being mined.
- **Answers lookups** so a wallet can see its own just-diverted transaction
  before it is mined.

It deliberately does **not** validate transactions. There is no consensus check,
no signature or proof verification, no fee or double-spend check: the hub
broadcasts what it is given and lets the network decide. It also broadcasts
transactions it could not parse, by design.

The queue is deliberately **not persisted**. An enclave is diskless, and
persisting it would mean writing plaintext migrations somewhere the host could
read them — the one thing this component exists to prevent. A restart drops the
queue; see "Known failure modes".

## Prerequisites

Same platform prerequisites as the shim (`shim/deploy/caution/OPERATORS.md`): a
Caution account with a FIDO2 authenticator, the `caution` CLI, a push key, and a
DNS name you control. Additionally:

- **Indexers to broadcast through**, each as a literal `IPv4:port` speaking gRPC
  over TLS, plus the DNS name their certificates carry.
- **Capacity and credit.** Fully-managed deploys refuse below a credit minimum,
  and the enforced figure has disagreed with the dashboard's stated one. It fails
  *before* building, so nothing is wasted, but budget for it.

## Deploy

Create the public repo for the assembled tree first (see "Verify"). Then pick
the mode. **As of 2026-08-17 the deployed pair uses clearnet mode.**

**Clearnet mode** (current):

```bash
sh zeronym/hub/deploy/caution/assemble-caution.sh \
  --name        <enclave-name> \
  --indexers    <ipv4>:<port>[,<ipv4>:<port>...] \
  --indexer-tls <name-on-indexer-cert> \
  --tls-domain  <hub-domain> \
  --app-source  https://github.com/ShieldedLabs/zero-hub \
  --http-submit
```

`--http-submit` opens `POST /` for shims to submit over HTTPS. There is no
mixnet client, no `--nym-egress`, and `/nym-status` will (correctly) report
`mixnet_connected: false`. Shims are assembled with `--hub <ip>:443 --hub-tls
<hub-domain>` — the hub's IP is only known after deploy, so the hub goes first.

**Nym mode** (built and tested; re-enable when the enclave network path
supports it — see the status block at the top):

```bash
sh zeronym/hub/deploy/caution/assemble-caution.sh \
  --name        <enclave-name> \
  --indexers    <ipv4>:<port>[,<ipv4>:<port>...] \
  --indexer-tls <name-on-indexer-cert> \
  --tls-domain  <hub-domain> \
  --app-source  https://github.com/ShieldedLabs/zero-hub \
  --nym --ack-wait-ms 15000 \
  --nym-egress  92.39.63.14/32:443:tcp \
  --nym-egress  172.104.178.252/32:443:tcp \
  --nym-egress  0.0.0.0/0:9000:tcp \
  --nym-egress  1.1.1.1/32:53:udp
```

Or drive either mode with `deploy.sh` and a `deploy.env` (see
`deploy.env.example`), which also publishes the app-source and polls the hub's
address for you.

Notes that apply to both:

- `--indexers` are literal IPv4 addresses. The enclave resolves **no DNS** for
  them (there is no port 53 rule for that path), so a poisoned answer has nothing
  to poison.
- **`--indexer-tls` is not optional in practice.** Without it the broadcast hop
  is plaintext and the enclave's parent host reads every batch in the clear
  moments before it is public — which would undo most of the reason this runs in
  an enclave.
- The two `443` egress rules in Nym mode are **two of the SDK's three built-in
  nym-api endpoints** (`validator.nymtech.net`, `cdn1.media-platform.net`). The
  SDK shuffles them and an enclave allowlist DROPS blocked packets, so each
  blocked attempt costs a 30 s timeout; with only one allowed, ~2/3 of client
  builds started on a dead endpoint. The third (Fastly) is deliberately excluded:
  shared CDN edges. Both IPs are DNS snapshots — re-derive on every redeploy.
- `--nym-gateway <identity>` pins the entry gateway. It **works** (verified
  against four public gateways, 2026-08-14) and the hub's address embeds its
  gateway, so pinning keeps the address stable across rebuilds. It needs a
  matching `--nym-egress <gateway-ip>/32` rule; a mismatch fails closed with no
  console. Gateway choice is NOT a throughput lever (measured).
- `--ack-wait-ms 15000` raises the SDK's ack-wait before retransmission. It is
  harmless and slightly helpful; it is NOT the fix for enclave slowness (the
  enclave hub never retransmits — the debug console showed zero).
- **Never pass `--debug` on a hub wallets point at.** Debug mode disables
  attestation. A debug hub is a diagnostic; `zeronym-hub-dbg` exists for exactly
  that and is not in `zero-hub`.

Then, from the directory it creates:

```bash
caution login --qr --username <name>
caution apps create           # prints the app id
  # -> create DNS: <hub-domain>  CNAME  <app-id>.apps.caution.sh   (BEFORE the push)
git push caution main         # builds, boots, health-checks (~20 min cold)
```

DNS ordering, the ACME race, and the certificate budget behave exactly as
described in the shim guide — read that section; it applies here unchanged.

## Verify

```bash
caution verify --attestation-url https://<hub-domain>/attestation
```

Pass `--attestation-url` explicitly: a fully-managed app writes no
`.caution/deployment.json`, so verify has nothing to infer from, and the error it
prints suggests `caution init`, which would wrongly provision an AWS stack.

Expect `✓ Attestation verification PASSED` with all three PCRs reproducing and
the TLS certificate binding verified.

**Do it the way an outsider would, or it proves less than you think.** `caution
verify` builds from the directory you run it in. Run from your own assembled
tree it proves "the enclave matches my laptop", which is only as trustworthy as
your laptop. The claim that matters is that a **fresh clone of the public repo**
reproduces the live enclave:

```bash
git clone https://github.com/ShieldedLabs/zero-hub && cd zero-hub
caution verify --attestation-url https://<hub-domain>/attestation
```

That requires the public repo's `main` to be at the EXACT assembled commit the
enclave was built from — the manifest names it, and verify rebuilds "at the
manifest commit". Each assemble is a fresh git history, so the push is a
force-push, and it must land BEFORE the enclave push: a verifier who arrives in
the gap gets a PCR mismatch indistinguishable from a compromised enclave. This
step was skipped by hand once (2026-08-17) and caught only by checking; put it in
the same script as the enclave push, or use `deploy.sh`, which does it.

**Measured, 2026-08-17, from fresh public clones with the build cache cleared:**
hub-6 (`zero-hub` @ `a1e703a8`) and shim-9 (`zero-shim` @ `cc70e9af`) both
`PASSED` — all three PCRs and the TLS binding — against the live enclaves. Both
halves of the deployed pair are independently verifiable. Note PCR2 was
identical across every build all week (`21b9efbc…`) while PCR0/1 tracked the
application: PCR2 does not identify the code, which is why all three are
required.

**Require all three PCRs. Do not accept a PCR2 match alone.** Advice circulated,
while PCR0/1 were failing for an unrelated reason, that "PCR2 is the application
layer and the check that matters". That is wrong on this platform: measured
2026-08-14, the attested hub and the attested shim — two entirely different
binaries — produced **byte-identical PCR2** (`21b9efbc…`), while PCR0/PCR1
differed per application (`218d1f64…` hub, `accb679a…` shim). PCR2 does not
distinguish the application, so an attestation accepted on PCR2 alone would prove
only that *some* Caution enclave is running, not that it is running the reviewed
hub — which for the component holding plaintext migrations is the whole point.
(Empirical observation; not confirmed with Caution which layer each index
measures.)

**Re-verified independently 2026-08-18, straight from the signed attestations of
two live apps** (`POST /attestation` with a nonce, COSE payload decoded, no
`caution verify` in the loop), and it turned up two things this section did not
record:

| PCR | shim-10 | hub-6 | Reading |
|---|---|---|---|
| 0 | `ed131501…` | `608621de…` | differs per application |
| 1 | `ed131501…` | `608621de…` | differs per application, **and equals PCR0** |
| 2 | `21b9efbc…` | `21b9efbc…` | **identical** — the shared base, does not identify code |
| 3 | `6eab08ea…` | `597eaa50…` | differs, but this is the **IAM role** under standard Nitro |
| 4 | `af50754c…` | `8c59e320…` | differs, but this is the **instance ID** — changes every redeploy |

1. **PCR0 and PCR1 are byte-identical within each enclave.** So "all three PCRs"
   is really ONE distinct code-binding value, reported twice, plus a constant.
   The guidance to require all three stays correct — it includes the values that
   do track the application — but it should not be read as three independent
   confirmations, and nobody should quote it as such to an auditor.
2. **PCR3 and PCR4 differ too, and must NOT be read as code measurements.** Under
   AWS's documented layout they are the parent instance's IAM role and instance
   ID. PCR4 in particular changes on every redeploy, so it can never belong in a
   reproducible expectation.

Note also that AWS documents PCR2 as *the application* measurement. We observe
the exact opposite, and PCR1 (documented as kernel + bootstrap, and therefore
expected to be IDENTICAL across two enclaves on one platform) instead tracks the
application. Caution's EIF layout is therefore not the standard one, which makes
the question below worth asking rather than assuming.

The 2026-08-14 PCR2 value (`21b9efbc…`) reproduced exactly on 2026-08-18 across a
redeploy, which is what a stable base layer should do.

Publish the assembled tree to the `--app-source` repo — `main`, plus a tag on the
deployed commit, since the manifest pins branch **and** commit and a branch tip
moves. Caution's own git remote is push-only, so this published repo is the only
route an auditor has to what you deployed. `zeronym/deploy.sh` does this
automatically on an attested deploy.

## Handing your address to shims

The hub's Nym address is what every shim is built against. Read it with:

```bash
curl https://<hub-domain>/nym-address
```

That endpoint exists because an attested enclave has **no SSH**: without it the
address could be read *or* the binary proved, never both. It returns `503` with an
explicit message until the mixnet client has connected, so an empty answer is
never mistaken for a valid address.

Hand that string to each shim operator; they bake it in with `--hub-nym`. There is
no discovery mechanism — the handoff is a human message, which is a deliberate
simplification of the one-hub design.

`/healthz` answers `200 ok` for liveness. Nothing exposes queue depth, batch size,
or counts: those would be an oracle for the anonymity-set size.

### Is the hub actually reachable? Check `/nym-status`, not the other two

```
curl https://<hub-domain>/nym-status
{"mixnet_connected":true,"address_published":true,"client_deaths":0,"consecutive_rebuild_failures":0}
```

**`/nym-address` and `/healthz` cannot tell you this, by design.** The address is
deliberately KEPT when the mixnet client dies — shims are baked against it, and it
comes back on rebuild — so `/nym-address` answers 200 whether or not the hub can
currently receive anything, and `/healthz` only says the process is alive.

That combination is not hypothetical. On 2026-08-14 an attested hub answered both
with 200 for hours while carrying no mixnet traffic at all; a shim and hub run
locally against the same public mixnet round-tripped a lookup in 5.6 s, which is
what identified the deployed hub rather than the network.

| field | alert when |
|---|---|
| `mixnet_connected` | `false` — **the hub is receiving nothing**; every shim's diverts are failing closed |
| `address_published` | `false` after startup — the client has never connected |
| `client_deaths` | climbing: gateway churn |
| `consecutive_rebuild_failures` | growing — it is down and not recovering; at 60 it takes a NEW identity and every shim needs re-pointing |

Poll `mixnet_connected`. It is the single field that says whether the system is
carrying migrations.

## Clearnet mode: a documented, temporary step down (2026-08-17)

The hub can run WITHOUT its mixnet client, accepting submissions over clearnet
HTTPS at `POST /` (`--http-submit` on assemble → `ZIH_HTTP_SUBMIT=true`). As of
2026-08-17 the deployed pair (hub-6, shim-9) runs this way. Know exactly what it
keeps and what it gives up, because it is easy to mistake for "no privacy":

| Property | Nym mode | Clearnet mode |
|---|---|---|
| Migrations batched, held, published on the 20-block cadence | ✅ | ✅ |
| Operator's indexer never sees the migration | ✅ | ✅ |
| Hub attested; publicly reproducible from `zero-hub` | ✅ | ✅ |
| Hub sees migration plaintext (it always must; it broadcasts) | ✅ | ✅ |
| Wallet lookups | 30–90 s from an enclave | sub-second |
| **Shim ↔ hub link unlinkable** | ✅ | ❌ **the hub sees which shim sent each migration, and when** |

That last row is the mixnet's specific contribution. With one shim and one hub
the link is trivially known regardless, so today the loss is near zero; it grows
with the fleet, which is exactly when Nym must return.

**Why:** every hub inside a Nitro enclave answered lookups in 30–90 s, and a
debug console showed the cause directly — the nym-sdk's send-rate controller
throttles the enclave hub to ~8 packets/s because its buffer to the gateway
websocket fills with cover traffic alone, where a laptop hub sits at 50/s. Not
our code, config, CPU, gateway, or the mixnet (each measured); it is the rate the
enclave's egress path drains one TCP socket, and it is being raised with Caution
(`CAUTION_QUESTIONS.md`).

**Re-enable path:** assemble the hub with `--nym` (and the egress rules) instead
of `--http-submit`; assemble shims with `--hub-nym <address>` instead of
`--hub <ip:port> --hub-tls <name>`. Nothing else changes; both binaries already
contain both paths.

## Known failure modes

**1. A restart changes your address and silently breaks every shim.** The identity
lives in RAM. A client *reconnect* keeps it — that was fixed deliberately, and
matters because the SDK reconnects often — but a **process restart** mints a new
one. Every shim then fails its diverts closed until re-pointed, and nothing tells
them. After 60 consecutive failed rebuilds the hub also deliberately takes a fresh
identity to get back onto the mixnet at all, logging loudly.

- **Detect:** poll `/nym-address` and alert on change.
- **Recover:** send the new address to every shim operator; each re-assembles and
  redeploys. Plan for this being slow, and prefer not restarting the hub.

**2. A restart drops queued migrations that wallets were already told succeeded.**
Shims answer the wallet as soon as a migration is dispatched onto the mixnet, and
your queue is RAM-only until the next flush. A restart in that window loses those
migrations with no error reaching anyone. Inherent to the diskless design.
Consequence to communicate: **wallets must resend when a transaction never
confirms.** Resends are safe — the queue deduplicates on payload hash.

**3. Mixnet bandwidth exhaustion.** Nym's free tier meters volume and your client
emits continuous cover traffic. When it runs out, reception stops. No ticketbook
(paid credential) mechanism is wired up yet.

**4. Anyone who knows your address can submit.** There is no submitter ACL. The
64 MiB queue budget is the only thing bounding an abusive submitter.

## If the hub goes down: the single-hub recovery runbook

**Not a failover design, and deliberately not one.** With one hub deployed there
is nothing to fail over TO, so this is a recovery procedure. When a second hub
exists it must be REPLICATED to, not failed over to (see "One hub, today" above):
shims submit to every hub always, so losing one costs redundancy, not service,
and no shim ever decides which hub to use.

There is **no standby today**. Recovery is a manual, multi-party operation, and it
is worth reading before you need it rather than during.

**What breaks, and how it looks.** The hub's identity lives in RAM, so a *process
restart* — crash, redeploy, enclave replacement — comes back with a **new Nym
address**, while every shim is baked with the old one. Then:

- **Submits are silently lost.** A shim answers its wallet the moment a migration
  is handed to the mixnet; a frame addressed to an address nobody is listening at
  is simply undeliverable, and no error reaches anyone.
- **Lookups fail closed** (`UNAVAILABLE`), which is the visible symptom.

A client *reconnect* is different and safe: the address survives it deliberately,
and that has been observed holding for ~13 hours across gateway churn.

**Detect it:** poll `/nym-status` for `mixnet_connected`, and watch `/nym-address`
for a *changed* value. Do not infer health from `/nym-address` returning 200 — it
answers with the last address published even after the client is gone, which is
precisely how a dead hub went unnoticed for hours on 2026-08-14.

**Recover:**

1. Redeploy the hub (destroy → create → CNAME → push; managed apps are
   immutable, so the app id and CNAME both change). **~25 min**, measured twice:
   the push alone is 20–23 min, of which **15–17 min is the builder downloading
   dependencies** — remote, and nothing local speeds it up. Note the CNAME step
   sits in the middle and blocks, so if DNS is someone else's job, wake them
   first.
2. Confirm `mixnet_connected: true` — do not skip this; a hub can boot, serve TLS,
   and answer `/healthz` while receiving nothing.
3. Read the new address from `/nym-address`.
4. **Send it to every shim operator.** There is no discovery mechanism; the
   handoff is a human message.
5. Each shim operator re-assembles with the new `--hub-nym` and redeploys
   (**~25 min each**, and their app id and CNAME change too). These can run in
   parallel across operators; nothing serialises them once they have the address.

Budget **well over an hour** end to end, during which migrations are failing.
`caution verify` is a further ~7 min but does NOT belong on the critical path —
restore service first, verify after.

**Why it cannot currently be better, and which half of that is real.** A standby
cannot be pre-baked into shims because a diskless hub has no address until it
runs. Nor can the identity be supplied by config to keep the address stable: that
would put the hub's Nym private key where the host operator can read it, and
anyone holding it can impersonate the hub and **receive migrations**. Both of
those still hold.

What no longer holds is the objection that a running standby is *hot*, "so both
hubs broadcast and the batch splits across two moments". Under the replication
rule that is the intended state, not a defect. Two hubs each holding every
migration each publish a full-size batch; the transaction appears twice, which
on-chain is a known txid and harmless. The cost is a second enclave holding
plaintext and one migration appearing in two batches at two moments -- accepted
deliberately, because the alternative that avoids it is health-based hub
selection, and that hands an attacker the ability to partition the anonymity set.

**What would fix it** (not built): shims fetch the current hub SET at runtime from
a published endpoint, accepting it only if signed by a key baked into their
audited config. That keeps the property the baking exists for -- an operator
cannot silently repoint a shim at a hub they control -- while reducing recovery to
a poll interval.

Note the unit is the SET, not an address. Every shim must converge on the SAME
set: two shims running different hub sets partitions the anonymity set exactly as
health-based selection would, just more slowly. So the signed document has to be
the whole membership list with a version, and a shim must refuse to act on a set
older than one it has already seen. Until that exists, treat hub restarts as
expensive and rare.

## Operating rules

- **Never `--debug`.** It disables attestation.
- **Never raise `RUST_LOG` to debug** on a deployed enclave. The hub logs only
  counts and dispositions, never a txid or transaction body, precisely because in
  an enclave the log reaches the parent host. Raising the level would leak exactly
  what this component protects.
- **`ZIH_HTTP_SUBMIT` stays off.** The clearnet `POST /` submit path is disabled
  by default; with the mixnet carrying submissions nothing legitimate posts there,
  while the enclave accepts inbound from anywhere. Disabled, it returns the same
  `404` as any unknown path, so a scanner cannot tell it exists.
- **Certificates**: diskless means every restart is a fresh ACME order, against 5
  production issuances per name per week. Iterate on throwaway names.
- **Watch Certificate Transparency** for your hub domain.

## Config reference

| env var | meaning | you point it at |
|---|---|---|
| `ZIH_LISTEN` | listen address inside the enclave | `0.0.0.0:8083` (the port Caution's proxy forwards to) |
| `ZIH_INDEXERS` | comma-separated literal `IPv4:port` list to broadcast through | your indexers; every batch member goes to all of them |
| `ZIH_INDEXER_TLS` | DNS name the indexer certificates must carry | **required in practice** — without it the hop is plaintext |
| `ZIH_NYM` | receive submissions over the mixnet | `true` |
| `ZIH_NYM_GATEWAY` | pin the entry gateway by IDENTITY key (single value; the address must stay stable) | **leave unset for now** — untested against public Nym |
| `ZIH_HTTP_SUBMIT` | accept clearnet `POST /` submissions | **leave off** (default `false`) |
| `RUST_LOG` | log level | leave at the default `info`; never `debug` on a deployed enclave |

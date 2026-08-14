# Running the zero-indexer-hub (operator guide)

The hub is where diverted migrations land. Shims divert Orchard-touching
`SendTransaction`s to it over the **Nym mixnet**; the hub holds them in a
short-lived queue, then broadcasts them to the Zcash network through indexers you
configure.

**Read this first: of the two components, this is the one that sees plaintext.**
The shim's enclave hides migrations *from the indexer operator*. The hub is where
that content arrives in the clear. It is therefore the component whose
attestation matters most — an unattested hub proves nothing that running the
binary on a laptop would not, while being trusted with exactly the data the
system exists to protect.

**One hub.** The current design assumes a single hub operator. That is why there
is no hub directory, no failover, and a single address baked into every shim.

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

Create the public repo for the assembled tree first (see "Verify"), then:

```bash
sh zeronym/hub/deploy/caution/assemble-caution.sh \
  --name        <enclave-name> \
  --indexers    <ipv4>:<port>[,<ipv4>:<port>...] \
  --indexer-tls <name-on-indexer-cert> \
  --tls-domain  <hub-domain> \
  --app-source  <public-git-url> \
  --nym \
  --nym-egress  92.39.63.14/32:443:tcp \
  --nym-egress  0.0.0.0/0:9000:tcp \
  --nym-egress  1.1.1.1/32:53:udp
```

- `--indexers` are literal IPv4 addresses. The enclave resolves **no DNS** for
  them (there is no port 53 rule for that path), so a poisoned answer has nothing
  to poison.
- **`--indexer-tls` is not optional in practice.** Without it the broadcast hop
  is plaintext and the enclave's parent host reads every batch in the clear
  moments before it is public — which would undo most of the reason this runs in
  an enclave.
- `--nym` turns on mixnet reception; the `--nym-egress` rules are the enclave's
  entire allowlist for reaching the mixnet (nym-api, a gateway, a DNS resolver).
- **Do NOT use `--nym-gateway` yet.** It pins the entry gateway by IDENTITY key
  while the egress rule needs its IP ADDRESS; a mismatch fails closed with no
  console. Untested against public Nym.
- **Never pass `--debug`.** Debug mode disables attestation. See the warning at
  the top of this file.

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
the TLS certificate binding verified. Measured on the first attested hub
(2026-08-14): all of PCR0/1/2 matched, and the manifest correctly recorded the
published app source and commit.

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

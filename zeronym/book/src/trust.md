# Trust: TEE, STEVE, and the quorum

Zeronym's migration-privacy guarantee does not rest on one mechanism. It rests on a chain of them, and the honest way to document the system is to walk that chain end to end. This chapter is the canonical deep-dive into how trust is established, verified, and bounded on the migration path:

- the **Nym 5-hop mixnet** that unlinks a shim from the hub, so the hub cannot tell which operator or region a migration came from;
- **AWS Nitro attestation** over a **reproducible StageX build**, which makes the exact software running in each enclave checkable rather than merely asserted;
- **STEVE**, Caution's one-way enclave handshake, by which the shim verifies the hub before it ever hands over a migration;
- the **keymaker / locksmith M-of-N quorum**, a governance mechanism (separate from STEVE) that persists keys across cold boots and upgrades and hands every hub the same shared key;
- the **Auditor Role**, which lets any independent third party confirm that a public endpoint is really running the reviewed software, without trusting the operator.

Related chapters own the pieces this one deliberately does not repeat. [The architecture](./architecture.md) owns the two trust-plane diagrams and the three nested encryption layers on the migration path. [Honest limits](./limits.md) states plainly that everything below trusts AWS and the hardware, not mathematics. [The roadmap](./roadmap.md) covers PIR (V3) as the step that eventually removes that hardware trust root. This chapter explains the mechanisms; those chapters place them in the wider story.

**What is trusted, and what is not.** The near-term system trusts the AWS Nitro platform and its hardware root of trust, and it trusts the people who review the open source and reproduce its build hash. It does **not** trust the operator running the shim, the host running the hub (Caution at launch), the Nym client or mixnet, or the full nodes the hub broadcasts through. Those parties see only ciphertext on the migration path, and the attestation chain is what lets a wallet (or an auditor acting on its behalf) check that this is actually so.

---

## The Nym transport (shim to hub)

Nym is used in the near-term system for exactly one hop: **shim to hub**. Wallets do not speak Nym; a naive wallet reaches [the shim](./shim.md) over ordinary TLS, and all query and non-migration traffic flows from the shim to the operator's backing lwd over the local network. Only an isolated migration is routed over Nym, and only from the shim's side to the hub's. This is the same boundary STEVE observes (below): both Nym and STEVE live only on the shim-to-hub channel, never on the wallet-to-shim path.

**The mixnet.** Nym is a 5-hop mixnet. The encrypted migration is wrapped in a **Sphinx** packet and relayed through independent mix nodes, each of which peels one layer and forwards, so no single node sees both the source and the destination. **Cover traffic** (steady dummy packets that are indistinguishable from real ones) is what makes the shim-to-hub flow unlinkable in time and volume: the hub cannot tell which region or operator a given migration came from, and a network observer cannot pick the real migration out of the cover. This is the property the migration threat model leans on to break the "IP X was active at time T" to "on-chain migration at time T" correlation for naive wallets.

**The proxy pair.** Per `deploy/caution-zaino/NYM.md`, the transport is built from the nym-sdk `TcpProxy` binaries: **`nym-proxy-client`** on the shim side and **`nym-proxy-server`** fronting the hub. The shim opens a Nym tunnel to the hub's Nym address, and the encrypted migration rides inside it. To the shim the tunnel is just a local TCP endpoint the Nym client exposes.

**The Nym client is untrusted.** This is a deliberate and load-bearing design choice. Because the migration is already encrypted to the hub's enclave key before it is handed to the tunnel (the inner layer in [the architecture](./architecture.md)'s three-layer stack), the Nym client, the mix nodes, and the parent host all see only ciphertext. `nym-proxy-client` can therefore run as an ordinary sidecar (in-enclave on managed Caution only because we do not control the parent there, parent-side on BYOC), and `nym-proxy-server` can run parent-side at the hub. A compromised Nym path yields nothing but ciphertext and traffic timing that the mixnet has already obscured. Whether `nym-proxy-server` may run parent-side on managed Caution or must run in-enclave is an open item for Caution (see [open questions](./open-questions.md)).

**Credentials and egress.** Nym mainnet uses ticketbook ecash credentials, so the Nym client needs Nyx-RPC egress (`rpc.nymtech.net:443`) to obtain them. This is a real operational dependency, not just an IP route.

**Validated, not assumed.** The transport is not a paper design. In the V2 rehearsal (2026-07-30), nym-proxy built from `nymtech/nym` carried real `CompactTxStreamer` gRPC over the live Nym **mainnet** mixnet against a live testnet node, end to end. Throughput is roughly 10x slower than clearnet (unary calls ~9 to 10 seconds, `GetBlockRange` ~19 blocks per second, latency-bound, with the first one or two calls warming up before steady state). That is fine for migrations, which are explicitly not time-sensitive, and it directly de-risks the shim-to-hub tunnel and the enclave attestation that ride on top of it.

---

## TEE attestation (AWS Nitro)

Both Zeronym binaries, the shim and [the hub](./hub.md), run inside **AWS Nitro enclaves**. The enclave is what makes operator-blindness real and checkable rather than a promise: a non-enclave shim would let the operator read migration contents, and a hacked adjacent node could peer at outbound transactions. Nitro provides hardware memory isolation (the parent instance and host cannot inspect enclave memory) and a hardware root of trust that signs a statement about the software running inside.

**Reproducible build, root hash.** Both binaries are **static-musl** and built reproducibly with **StageX** (`SOURCE_DATE_EPOCH=1`), following the existing `deploy/caution-zaino/combined/` Containerfile pattern. A reproducible build means anyone with the source produces a bit-identical image and therefore the same measurement hash. That hash is the anchor of the whole trust chain: it is what an attestation carries and what an auditor recomputes.

**In-enclave keygen and NSM binding.** Each enclave generates its key **at boot, inside the enclave** (the private key never leaves), and binds the corresponding **public key** into the attestation produced by the **Nitro Security Module (NSM)**. The attestation is a COSE_Sign1 document, signed up to the AWS Nitro root, that carries the enclave's **PCRs** (platform configuration registers measuring the loaded software, that is, the StageX root hash) alongside the bound public key. Verifying it proves two things at once: the software is exactly the reviewed build, and the public key was born inside that software rather than handed in by the host.

The shim binds its **TLS public key** into the attestation (see [the shim](./shim.md) for the ACME cert model that makes the drop-in work), so an auditor can check that the certificate a wallet sees is keyed to an enclave-born key. The hub binds its **hub public key**, the key that shims encrypt migrations to. Attestation binding is achievable three ways on Caution's platform (confirmed at the V2 sync): via the STEVE handshake, via the pubkey injected through `metadata.json` into `user_data` (which implies a persisted key), or via a new runtime `arbitrary_data` field Caution would add. The specific mechanism, and how the `/attestation` endpoint is delivered for a zero-ingress service, are open items for Caution (see [open questions](./open-questions.md)).

The two unknowns Zooko originally flagged, exactly what can be attested and how memory peeking is prevented, are substantially answered by this substrate: Nitro's hardware memory isolation prevents the host from reading enclave state, and Caution's attestation binding ties the enclave-born key to the measured software.

---

## STEVE (the corrected canonical facts)

**STEVE** stands for **"Secure Transport Encryption Via Enclave"**. It is a Distrust protocol, integrated into Caution, documented at the Distrust blog (`distrust.co/blog/steve.html`) with source at `git.distrust.co/public/steve`. It is a **second encryption layer that terminates inside the enclave**, designed for exactly the situation Zeronym has: when an outer transport layer terminates outside the enclave, STEVE gives you a channel whose plaintext exists only within the TCB.

**Scope.** STEVE is used **only on the shim-to-hub channel**, carrying the request/response `SubmitMigration` exchange. It is **not** used on the wallet-to-shim path (naive wallets speak plain TLS, which terminates inside the shim enclave on its own). In [the architecture](./architecture.md)'s three-layer stack, STEVE is the **middle** layer: it wraps the payload that is already encrypted to the hub key (inner) and is itself carried inside the Nym Sphinx packet (outer), and its AES-256-GCM plaintext is only ever exposed inside the hub enclave.

**One-way handshake.** STEVE's handshake is **one-way: the client (the shim) verifies the enclave (the hub)**, not the reverse. Concretely:

1. The client checks the hub's **attestation and PCRs against the AWS Nitro root**.
2. It extracts the enclave's **Ed25519** identity key from the attested material.
3. It sends an ephemeral **X25519** public key.
4. It receives the enclave's ephemeral key plus an **Ed25519 signature** over that ephemeral key.
5. It **verifies the signature** against the attested Ed25519 identity key.
6. Both sides derive a session key by **X25519 ECDH followed by HKDF-SHA256**.
7. Payloads are then **CBOR encoded and encrypted with AES-256-GCM**.

STEVE runs as a **reverse proxy on `:8080`**. On the hub side this is the STEVE server that decrypts inbound migrations inside the enclave; on the shim side the STEVE client performs the verification above before any migration is sent. The **Rust SDK is still in development** (the JS SDK ships today), which is why implementing the handshake directly from standard primitives is on the table as a fallback (see [open questions](./open-questions.md)).

Because the handshake is the shim verifying the hub's attestation and extracting its bound key, **the STEVE handshake is itself an act of auditing**: it performs the same enclave-verification the Auditor Role performs (below), just automatically and per session, before the shim trusts a hub with a migration.

**Two open STEVE items** are carried to Caution rather than settled here: the exact **wire form over Nym** (does a STEVE session carry gRPC / h2, or a raw framed byte stream we frame ourselves), and **mutual vs one-way** (one-way is sufficient for privacy; making the hub also verify the shim's attestation would additionally gate abuse). Both live in [open questions](./open-questions.md).

**STEVE is not the quorum.** A common confusion worth stating flatly: the keymaker / locksmith quorum described next is a **separate** Caution mechanism, not part of STEVE. STEVE is a per-session handshake that verifies an enclave and derives a transport key. The quorum is a governance and persistence mechanism for long-lived keys. They solve different problems.

---

## The keymaker quorum (separate from STEVE)

Enclaves are diskless and ephemeral: a key generated in-enclave at boot is, by default, lost on every restart, and a software upgrade changes the measurement so a KMS-seal-to-PCR scheme would refuse to unseal the old key. Zeronym needs long-lived keys anyway (a stable TLS key and address for the shim, and one hub key that every hub instance shares), so it uses a **keymaker / locksmith M-of-N quorum**.

**What it is.** The quorum is an M-of-N key-custody mechanism spread across **3 to 4 organizations**: the proposed consortium of Caution, Nym, Shielded Labs, and the Zcash Foundation. It reconstitutes a key inside a fresh, attested enclave across both **cold boots and software upgrades**, which is strictly better than sealing to PCRs (that breaks on upgrade). The private key material is only ever reassembled inside an attested enclave; no single org holds it.

**What it persists.**

- For **the shim**: the TLS keypair. Persisting it across boots and upgrades gives the shim's public URL a **stable key and address**, so an operator's endpoint does not churn its identity on every restart, and the ACME cert can renew against a stable key.
- For **the hub**: **a single shared hub key across all hub instances**. This is the decision that makes failover clean. A shim encrypts a migration to "the hub key," and **any** attested hub instance can decrypt, dedup, and publish it. Per-hub keys would force the shim to re-encrypt on failover and would strand a migration whose hub died mid-flight. With one shared key provisioned to every attested hub by the quorum, running two or more hubs with shim failover (see [the hub](./hub.md)) costs nothing in key management: a rare double-publish is a harmless on-chain duplicate, deduped by txid.

**Governance trajectory.** The quorum is also the long-term **trust-distribution** goal: the consortium collectively attests to the key state and software integrity, so no single party (not even Caution) unilaterally controls the hub key. For launch this is staged: a single trusted entity (Caution) stands up the hub, with the multi-org quorum to follow. The consortium's several organizations are also the natural operators of the standby hubs, which is where decentralization of the hub itself begins.

---

## The Auditor Role

The drop-in model creates a specific verification problem: a wallet connects to a familiar public URL (say `zec.rocks:443`) and needs to know that URL is really fronted by the reviewed shim enclave, not by an operator who quietly kept the plaintext. The **Auditor Role** is how any independent third party answers that, **without trusting the operator**, and then passes the assurance on. In practice a wallet developer acts as the trust proxy: audit once, and every user of that wallet inherits the result.

The steps for a public lwd endpoint:

1. **Fetch the endpoint's TLS public key and certificate** directly over HTTPS.
2. **Load its attestation.** POST a nonce to `/attestation` and receive a COSE_Sign1 document. This proves the private key was **generated inside the enclave** and carries the **root hash of the software** running there.
3. **Verify the PCRs against the AWS Nitro root.** Confirm the attestation chains to the genuine Nitro hardware root of trust and that the measured software matches expectations.
4. **Reproduce the build.** Run the reproducible StageX build from source, obtain the hash, and confirm it equals the attested root hash. This is what ties "the reviewed source" to "the software actually running."
5. **Check Certificate Transparency.** Confirm that **no other currently-valid certificate exists** for the domain. Without this step an operator could present the attested, enclave-born cert to auditors while serving a different, non-enclave shadow cert to real users and MITM them. All Let's Encrypt certificates are CT-logged, and the shim obtains its cert via in-enclave ACME precisely so this check is meaningful (see [the shim](./shim.md)). CT closes the cert-substitution gap that the public-URL drop-in would otherwise leave open.

Concrete probes against a running enclave (the node IP here is an ephemeral testnet enclave):

```
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"nonce":"00112233445566778899aabbccddeeff"}' https://<node-ip>/attestation
grpcurl -plaintext \
  -import-path <zaino>/packages/zaino-proto/lightwallet-protocol/walletrpc \
  -proto service.proto <node-ip>:8137 \
  cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
```

The hub is audited the same way, and, as noted above, **the shim's STEVE handshake is the shim performing exactly this audit of the hub** on every session before it trusts a hub with a migration. Independent auditors run the same steps out of band; reproducibility is what lets the consortium and third parties confirm that the running hub is the reviewed hub.

---

## What this trusts, and what it does not

| Party | Trusted? | Why |
|---|---|---|
| AWS Nitro manufacturer + platform | Yes (the trust root) | Hardware root of trust signs the attestation; memory isolation blocks host peeking |
| Reviewers who reproduce the build hash | Yes | The reproducible StageX hash is only meaningful if someone recomputes it |
| Operator running the shim | No | Sees only TLS that terminates in the enclave; migrations never reach its backing lwd (see [the shim](./shim.md)) |
| Hub host (Caution at launch) | No | Sees only STEVE ciphertext; cleartext exists only inside the attested hub enclave |
| Nym client + mixnet | No | Payload is already encrypted to the hub key before it enters the tunnel |
| Full nodes the hub broadcasts through | No | Receive only the final, wallet-signed, batched transactions, unlinked from any source IP |

The honest consequence, developed fully in [honest limits](./limits.md): the resulting privacy trusts **AWS and the hardware, not mathematics**. That is a deliberate near-term choice. The hardware-independent alternative, which removes this trust root entirely, is PIR, and it is complementary to the TEE rather than a replacement (distinct failure modes). That is the V3 story, covered in [the roadmap](./roadmap.md).

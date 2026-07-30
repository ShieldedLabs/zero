# Toward a Verifiable Zcash Node: Running Zebra and Zaino in a TEE

*A joint post by Shielded Labs and Caution. Draft for review by Mark Henderson and Anton Livaja. Not for publication until both approve.*

---

When you use a Zcash light wallet, your wallet talks to a server. That server,
running `lightwalletd` or `zaino`, sees which transparent addresses you look up
and which transactions you fetch. Shielded Zcash hides an enormous amount, but
the light-client server has historically been a party you simply have to trust
not to log your queries.

We wanted to know: what would it take to remove that trust entirely? Not with a
promise, but with a proof, cryptographic evidence that the server is running
exactly the code you can read, on hardware that its own operator cannot see
into.

This post is about a proof of concept we built together to answer that: a full
Zcash validator and light-wallet indexer, running inside a Trusted Execution
Environment, from a build that anyone can reproduce and verify byte for byte.

## The idea

Two ingredients make this possible.

The first is **confidential computing**. AWS Nitro Enclaves carve a slice of a
machine into an isolated environment with no persistent storage, no interactive
access, and no network except a single narrow channel. Even the operator of the
parent machine cannot read enclave memory. Crucially, the enclave can produce an
**attestation**: a signed measurement, rooted in AWS's hardware, of exactly what
image is running.

The second is **reproducible builds**. An attestation is only as useful as your
ability to know what it measures. If the build is reproducible, anyone can take
the public source, rebuild it, compute the expected measurement, and confirm the
running enclave matches. Caution's platform is built on
[StageX](https://codeberg.org/stagex/stagex), a fully source-bootstrapped,
deterministic toolchain, precisely so that this check is possible down to the
kernel.

Put them together and you get something new for Zcash: a light-wallet service
whose operator cannot see your queries, and whose integrity you do not have to
take on faith.

## What we built

Rather than shield only the indexer, we put the whole stack in one enclave:

- **Zebra**, the Zcash Foundation's full-node validator, providing
  consensus-validated chain data.
- **Zaino**, the light-wallet indexer, serving the `CompactTxStreamer` gRPC API
  that wallets speak.

Both run inside a single Nitro enclave, built from a single reproducible image.
A small init process supervises the two and wires the indexer to the validator
over loopback, a link that never leaves enclave memory.

Co-locating them is not just tidy. It means **one attestation covers the entire
path** from consensus to wallet. And because the indexer talks to the validator
inside the same sealed environment, there is no network hop to protect: the data
path a wallet depends on is verifiable end to end.

## The interesting engineering

A few things made this harder, and more interesting, than a normal deployment.

**No disk.** An enclave boots from a RAM image and keeps nothing. A light-wallet
indexer handles this beautifully: Zaino has an ephemeral mode that keeps only the
recent block window and the mempool in memory, tens of megabytes, and sources
everything else from the validator on demand. We measured it serving live
mainnet compact blocks at well under 100 MB of RAM.

The validator is the harder half. A fully-synced Zcash mainnet state is around
280 GB. With no disk, that has to live in RAM, which is the one genuine cost of
this design today, and the place where confidential computing platforms are
still maturing.

**Two C++ libraries under musl.** Building Zebra and Zaino as fully static
binaries meant compiling RocksDB and `libzcash_script` against musl libc, from
source, deterministically. Caution had already blazed most of this trail for
Zebra; combining it with Zaino in one image took some careful linker work, but
it built, and it built **reproducibly**: two independent from-scratch builds
produced byte-identical binaries.

**Real bugs, found by really deploying.** The first live deploy built, attested,
and booted, and then sat there not serving. Rather than guess, we reproduced the
exact failure locally and found two concrete bugs: the supervisor was invoking
the validator with a misplaced config flag, so the validator never started; and
a stale build could fail on missing CA certificates. Both were a few lines to
fix. We also turned up, and reported, a genuine platform bug in the enclave
image assembler and a handful of rough edges, exactly the kind of feedback that
makes a young platform better.

## Where it stands

A **testnet node is live right now**, and every layer is proven end to end:

- The combined image builds **reproducibly**, identical binaries across
  independent builds.
- Caution's platform **builds it, attests it, and boots it**. The running
  enclave returns a real signed attestation document to a nonce challenge.
- The co-located supervisor **starts both processes** as one unit inside the
  enclave.
- The enclave **serves real wallet queries** over the public internet,
  `GetLightdInfo`, `GetLatestBlock`, and compact-block `GetBlockRange`, while
  the validator syncs the chain inside the box.

To our knowledge this is the first attested, reproducibly-built Zcash node of
any kind.

What we have not yet run is the same stack against **mainnet**, because
mainnet's ~280 GB of chain state needs either persistent enclave disk or a
large-memory enclave, and testnet's ~15-20 GB fits in RAM today while mainnet's
does not. Both of those are on Caution's near-term roadmap (**persistent enclave
storage** and **large-memory / bring-your-own-compute enclaves**), and when they
land the same reproducible image becomes a fully verifiable Zcash *mainnet*
node, with its state seeded from a synced snapshot to skip the initial sync.

## Why it matters

Privacy infrastructure asks users to trust operators. This work is a concrete
step toward not having to. A verifiable, operator-blind light-wallet service
means your wallet queries are protected not by policy but by hardware and math,
and the roadmap from here points at removing even the on-demand queries to the
validator, so that nothing about your activity leaves the enclave at all.

It is also a template. The pattern, reproducible build plus attested enclave plus
a diskless indexer, generalizes well beyond Zcash to any service where users
would rather verify than trust.

We are releasing the build contexts and the reproducibility checks publicly.
We would love for others to rebuild the image, check the hashes, and hold us to
the standard this whole approach is about: don't trust, verify.

---

*Shielded Labs builds enterprise Zcash infrastructure. Caution provides
verifiable confidential-compute deployment on AWS Nitro Enclaves. This was a
joint effort; the build contexts live in Shielded Labs' public repository.*

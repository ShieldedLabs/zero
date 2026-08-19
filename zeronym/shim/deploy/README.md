# Reproducible build: zero-indexer-shim

A StageX build that turns `zeronym/shim` into one static-musl binary, designed
so that two independent builds of the same commit produce **byte-identical**
output.

Sibling of `deploy/caution-zaino/combined/`, which does the same job for zebrad
and zainod. Same ingredients, far less machinery, because the shim has no
rocksdb and no libzcash_script.

> **Where the sibling recipe lives.** Every reference below to
> `deploy/caution-zaino/` or `.github/workflows/caution-z3-reproduce.yml` points
> at branch `claude/lightwalletd-zaino-anton-livaja-522841`. Those files are not
> on `main` and not on this branch, so `git show` them from that ref rather than
> looking for them beside this file.

## Why reproducibility is the whole point

The Zeronym trust model is detection-based, and it hands the auditor one job:
rebuild from source, get the same hash, and check that hash against the one
bound into the enclave attestation.

Without that chain, an attestation proves only that *some* binary is running
inside a genuine enclave. It says nothing about *which* binary. The design would
collapse into trusting whoever compiled it, which is precisely the party the
architecture refuses to trust. So the deliverable here is not a Dockerfile that
builds. It is a hash anyone can independently recompute.

## Files

| File | What it does |
|---|---|
| `assemble.sh` | Builds the throwaway build context out of `git archive HEAD`. Never touches the working tree. |
| `Containerfile` | The build itself. StageX bases pinned by digest, static musl, export stage, busybox runtime. |
| `build.sh` | One deterministic build: extracts the binary and packages the OCI image, printing both hashes. |
| `reproduce.sh` | The proof: two cold builds, compared against each other **and** against `EXPECTED_SHA256`, then assert the vendored subtrees are still clean. |
| `EXPECTED_SHA256` | The published binary hash, in one machine-readable place. `reproduce.sh` and CI both read it; re-baselining is an explicit, reviewable edit. |

`.github/workflows/zeronym-shim-reproduce.yml` runs `reproduce.sh` itself (not a
copy of its logic) on a native x86_64 runner, which is the
independent-second-machine half of the claim. Running the script rather than
re-implementing it is deliberate: CI then exercises the exact command auditors
are told to run, and inherits the `EXPECTED_SHA256` comparison. A job that only
checked build 1 against build 2 would go **green** on the one failure it exists
to catch, since two builds on a diverging machine agree with each other
perfectly well.

All four scripts must be run from inside a checkout: each starts with
`git rev-parse --show-toplevel` and locates everything from there. The
directory you are in within the checkout does not matter.

## Platform, stated plainly

**This builds `linux/amd64`, targeting `x86_64-unknown-linux-musl`. There is no
arm64 variant and there should not be one.**

Every StageX image pinned here is published for amd64 only (verified with
`docker manifest inspect`: each returns a single-platform OCI index). A "native
arm64" build would therefore require substitute base images, which would throw
away the pinned-by-digest base that the entire reproducibility argument rests
on. A reproducible build on a non-reproducible base is not a reproducible build.
amd64 is also the AWS Nitro enclave target, so the demonstrated hash is the hash
that *would* be bound into an attestation. That binding does not exist yet: see
"What is proven, and what is not" below, which is explicit that nothing here has
been near an enclave.

On an arm64 Mac this runs under Rosetta. The sibling recipe's comments warn that
"arm64 via emulation is far too slow", but that warning was written about two
Rust workspaces with rocksdb; the shim is a single small binary whose only C
dependency is `secp256k1-sys`. **Measured here: 78 s to fetch, 96 s to compile
all 276 crates, under three minutes end to end for a cold build.** The warning
does not transfer.

One trap worth recording, because it cost an earlier measurement a factor of
twenty: do not point `CARGO_TARGET_DIR` at a macOS bind mount. An exploratory
`docker run -v $OUT:/out` build of this same crate took 34 minutes, because
every rlib was written through VirtioFS. Building on the container's own
filesystem, which is what this Containerfile does, is the whole difference.

## Determinism ingredients

In the recipe, ours to set:

- **Every image pinned by sha256 digest, not by tag.** Three of them, and the
  third is the one people forget:

  | Image | Digest | Role |
  |---|---|---|
  | `stagex/pallet-rust:1.96.0` | `sha256:abe9b95c…73dc` | builder base; **this is the toolchain pin** |
  | `stagex/core-busybox:1.38.0` | `sha256:e4a30addc…c181` | runtime base |
  | `docker/dockerfile:1.26.0` | `sha256:ecfaec9ed…fc32` | the BuildKit **frontend**, which parses this Containerfile into LLB |

  The frontend matters as much as the bases. It decides how every `COPY`, `RUN`,
  `--network=none` and export directive becomes build graph, so the floating
  `docker/dockerfile:1` tag the sibling recipes use would let a Docker Hub
  release change layer construction, and therefore the OCI manifest digest this
  document publishes as a constant. `1` is a moving pointer, checked rather than
  assumed: on 2026-07-31 it resolved to `sha256:87999aa3…`, a different image
  from `1.26.0`. Bump any of these three deliberately, and re-baseline the
  hashes in the same change.
- `SOURCE_DATE_EPOCH=1`.
- `-C codegen-units=1` (parallel codegen is a nondeterminism source).
- `-C target-feature=+crt-static`, fully static musl, no dynamic loader.
- `-C link-arg=-Wl,--build-id=none`.
- `CARGO_INCREMENTAL=0`.
- Fixed `WORKDIR` and `CARGO_HOME`. rustc embeds absolute paths, and this recipe
  pins them rather than using `--remap-path-prefix`. **A rebuild at a different
  path produces a different hash.** That is the single most likely way for an
  auditor to conclude "it does not reproduce" when it does.
- `cargo fetch --locked` then `cargo build --frozen`, so the committed
  `Cargo.lock` is authoritative and any drift is a hard failure.
- **Network exception, mixnet-driver build only.** The clearnet build
  (`--build-arg CARGO_FEATURES=`) can run the compile phase `--network=none`; the
  mixnet build (the default `CARGO_FEATURES=mixnet-driver`) cannot, because
  `nym-network-defaults`'s build.rs shells out to `cargo metadata` over the whole
  nym workspace, resolving git deps (e.g. `nymtech/smoltcp`) that are not in this
  crate's lockfile and so were never `cargo fetch`ed. The compile RUN keeps the
  network on for it. Determinism is unaffected: every version is pinned (this
  crate's `--frozen` lock, and nym's own committed lock at the pinned tag for that
  transitive resolution), so the network only fetches content already addressed by
  rev/hash — demonstrated by two independent cold builds producing the identical
  hash. A fully-offline mixnet build (pre-warm nym's workspace metadata cache in
  the fetch phase, then `CARGO_NET_OFFLINE` for the compile) is the follow-up.
- **No BuildKit cache mounts.** `docker build --no-cache` does not clear cache
  mounts, so a cache-mounted recipe cannot honestly claim a cold-build proof.
- Context built with `git archive`, which stamps every file's mtime with the
  commit timestamp, so mtimes are identical on every machine. **That includes
  the Containerfile itself**, which is the whole definition of the build and
  therefore the file most worth pinning to a commit; every caller builds
  `-f "$CTX/zeronym/shim/deploy/Containerfile"`, the archived copy, never the
  working-tree one. Without that, an auditor at the recorded commit with a
  locally edited recipe would build something else while believing they had
  rebuilt that commit, and nothing would say otherwise.
- `umask 022` in `assemble.sh`, plus `tar -xp`, so context file modes are the
  committed ones rather than the invoking user's. Nothing from the context
  currently reaches a shipped layer, so this cannot move either published hash
  today; it is what keeps that true if a `COPY <context path>` is ever added to
  the runtime stage, at which point the OCI digest would otherwise become
  umask-dependent, and same-host repeats would keep agreeing while other
  machines diverged.
- `assemble.sh` is **POSIX sh**, `set -eu`, no pipelines. Callers invoke it as
  `sh assemble.sh`, which bypasses the shebang, and on Debian and Ubuntu
  `/bin/sh` is dash, which has no `-o pipefail`. A `set -euo pipefail` here
  aborts with `set: Illegal option -o pipefail` before the script body runs, on
  the most likely third-party host. Each `git archive` therefore writes a tar
  file whose exit status `set -e` genuinely checks, instead of being masked by a
  downstream `tar -x` that succeeds happily on empty input.

For image packaging (a separate, weaker claim about the OCI digest rather than
the binary): `--output type=oci,rewrite-timestamp=true,force-compression=true`
with `SOURCE_DATE_EPOCH=1` exported into the shell. Same flags as
`zcash/zallet utils/build.sh`. Caution's backend adds these automatically at
deploy time; `build.sh` exists to reproduce it locally.

The toolchain pin is the **pallet-rust digest**. Neither the shim nor the repo
root carries a `rust-toolchain.toml`, and none should be added: a channel that
differs from the image would make rustup download a toolchain, which needs
network and destroys determinism.

## The build context

`assemble.sh` produces a partial mirror of the zero repo:

```
zeronym/shim/                 the crate
zebra/Cargo.toml              workspace root that zebra-chain inherits from
zebra/zebra-chain/            the vendored Zcash parser (the path dep)
zebra/zebra-test/             optional dep of zebra-chain, manifest only
zaino/Cargo.toml              workspace root that zaino-proto inherits from
zaino/packages/zaino-proto/   the CompactTxStreamer codegen (the path dep)
```

Total 8.5 MB. Keeping the repo's own layout is what makes the shim's
`../../zebra/zebra-chain` and `../../zaino/packages/zaino-proto` path
dependencies resolve unchanged inside the image. **No manifest is edited
anywhere**, which is both a hard rule (vendored subtrees are read-only) and the
right answer.

Three non-obvious facts, each established empirically rather than assumed:

- **`zebra-test` must be present and is never compiled.** It is an optional
  dependency of zebra-chain that only surfaces under dev-dependencies, but cargo
  must load its manifest to resolve the graph. Removing it fails at
  *resolution*, not compile: `failed to get zebra-test as a dependency of
  package zebra-chain`. The error looks unrelated to what you deleted.
- **The other workspace members are not needed.** zebra lists twelve members and
  zaino nine; cargo reads those roots only for inheritance here and does not
  require the absent ones to exist.
- **`orchard/` is not needed.** zebra carries a `[zero]` patch
  `orchard = { path = "../orchard" }`, but that patch belongs to zebra's
  workspace, not the shim's. The shim is its own workspace and resolves orchard
  from crates.io per its lockfile. (Whether the shim's parser and the node's
  parser *should* come from the same orchard is a real open question, and a
  separate one. It does not affect reproducibility; the lockfile pins it either
  way.)

## Two traps worth naming

**protoc must stay absent from the image.** `zaino-proto`'s `build.rs`
regenerates its committed `src/proto/*.rs` whenever protoc is reachable.
`default-features = false` in the shim's manifest removes the
`which::which("protoc")` branch, but the `PROTOC` env-var branch of
`protoc_available()` is *not* feature-gated. So the recipe deliberately does not
copy `stagex/user-protobuf` (which the sibling zaino stage does) and never sets
`PROTOC`. Two independent locks: nothing to find, nothing to regenerate. Do not
add the protobuf pallet back while copy-pasting from the sibling recipe. Inside
a container this would only alter a throwaway copy, but it silently changes what
gets compiled, and therefore the hash.

The `default-features = false` lock has been tested where it actually gets
stressed, which is the *host*, not the image: this development machine has
`protoc` on `PATH` at `/opt/homebrew/bin/protoc`, and a full `cargo test
--locked` of the shim still leaves `git status --porcelain zebra/ zaino/`
empty. If the `which` branch were live, that build would have rewritten
`zaino/packages/zaino-proto/src/proto/*.rs` in the vendored subtree. So the
feature lock holds on its own, and the absent-protoc image is the second,
independent one.

**No cache mounts, ever, in this file.** See above. The sibling
`overlay/Containerfile` uses them and is correspondingly unsuitable for proving
anything; `combined/Containerfile` does not, which is why the reproduce workflow
targets it. This recipe follows `combined`.

## What was subtracted from the sibling recipe

All of it is rocksdb and libzcash_script fallout that the shim's graph does not
need. `zcash_script 0.4.5` is the pure-Rust reimplementation, and
`secp256k1-sys` (plain C, no C++) is the only crate in the graph that compiles
native code.

- `stagex/pallet-clang`, `stagex/user-protobuf`, `stagex/user-abseil-cpp`.
  `pallet-rust` alone already ships clang, cc, ar, mold, ld.lld, headers and
  `libc.a`.
- `CXXSTDLIB`, `CXXFLAGS`, `ROCKSDB_USE_PKG_CONFIG`.
- The `libc++.a` / `libc++abi.a` / `libzstd.a` / `libz.a` link-args, the
  `--whole-archive` bracket, `--allow-multiple-definition`, `-ldl`, `-lm`, and
  the `/usr/lib/libstdc++.a` `INPUT()` shim.
- The `zebra-release` build barrier, which exists to stop two heavy workspaces
  hitting peak codegen simultaneously. One binary needs no barrier.

If a future dependency breaks the link, restore the sibling's flags before
debugging anything else. Matching the proven recipe is worth more than a short
flag list.

## Usage

```sh
# one deterministic build: binary + OCI image, prints both hashes
sh zeronym/shim/deploy/build.sh

# the proof: two cold builds, compared to each other AND to EXPECTED_SHA256,
# then assert the vendored trees are clean
sh zeronym/shim/deploy/reproduce.sh
```

Run these from anywhere inside the checkout. Artifacts land outside it
(`../zero-indexer-shim-build/`) so that `git status --porcelain zebra/ zaino/`
stays a clean signal. Override with `OUT` and `CTX`.

Changing the recipe on purpose means re-baselining: confirm the new hash twice
with `EXPECTED= sh zeronym/shim/deploy/reproduce.sh` (an empty `EXPECTED` skips
the published-hash comparison for exactly that run), then update
`EXPECTED_SHA256` and the recorded hashes below in the same commit. Those two
must never drift apart.

**Changing the compiled source means re-baselining too**, and that is the more
common case: any edit under `zeronym/shim/src/`, to `Cargo.toml` or
`Cargo.lock`, or a subtree pull touching `zebra/zebra-chain` or
`zaino/packages/zaino-proto`, moves the binary and therefore the published hash.
Both 2026-08-01 re-baselines were of exactly that kind (two classifier rulings in
one day), and both are written up below. Two traps the first one exposed, both
worth knowing before you start.
Because `assemble.sh` archives `HEAD`, a source change that is still in the
working tree is invisible to every script here, and they will cheerfully rebuild
and confirm the **old** hash. And a hash measured from anything other than
committed source is a hash of a tree that may never exist as a commit. **Commit
first, then measure.** The post-mortem below is what happens when that order is
reversed, and the second re-baseline explains what to do when the two cannot be
ordered that way because the hash and the source have to land in one commit.

## Recorded hashes

Do not populate this section by hand, and never with a plausible-looking
placeholder. A wrong hash in an audit document is worse than a missing one. The
machine-readable copy is `deploy/EXPECTED_SHA256`; the two must move together.

| | binary sha256 | what it was built from |
|---|---|---|
| **current**, zaino-proto 0.4.0 Bytes payloads | `77fa2dc49fab22f1cbf7059d8bba819e7b4ebb45d9de36619bd977245cfded0f` | the zaino 0.8.0 subtree pull bumps the path-dependency `zaino-proto` to 0.4.0, which serves `RawTransaction.data` as prost `Bytes` instead of `Vec<u8>`; the shim's two `RawTransaction` construction sites gain `.into()` (`Vec` to `Bytes` is a zero-copy move). The classifier predicate and every route are unchanged: the hash moved because the compiled proto crate and those two sites did. `zeronym-shim-reproduce` reports SELF-CONSISTENT across two cold builds on the x86_64 runner, measured from the tree that vendors zaino 0.8.0. |
| superseded, padded clearnet submissions + deadlines on every hop | `b91fa275ffbb9676b9ef07df25a88fff2bf697c9134336fdadc17355c9fe23b5` | four compiled changes. (1) Clearnet submissions are now the fixed-size `SubmitV1` frame the mixnet path already used, instead of the bare transaction: an unpadded body's LENGTH tracked the payload, and since payload sizes become public once published, length plus arrival time re-identified what timing alone could not. (2) `HubClient::submit` gained a deadline covering connect, TLS handshake, request and body -- it had none, so a hub that accepted a connection and went quiet held the WALLET open indefinitely. (3) `forward()` bounds time-to-response-HEADERS (not the body, so streaming still works), closing the stalled-but-alive upstream that answers PINGs and never replies. (4) Body reads are time-bounded and fail closed. Two independent cold builds on this host agree. CI's native double-build is the cross-machine check. |
| superseded, inbound-liveness reroll + gated `/nym-diag` | `2009f9b37404ceba8846c0157d3fadb169f0595afc04fb889efb17f22c3a22c9` | two compiled changes, both answers to the same measured failure. (1) The driver now **rebuilds its mixnet client when nothing is arriving inbound**: a 60 s probe to the shim's own address, and two consecutive silent rounds tear the client down and build a fresh one, which rerolls the entry gateway and the gateway registration together. Measured 2026-08-14 across four deployed shims on identical config: two answered `GetTransaction` and two never did, one of them broken three minutes after boot and still broken hours later, because the SDK reports a death only when it gives up on its gateway and a gateway that accepts sends is never given up on — so `client_deaths` stayed 0, no rebuild was ever requested, and an immutable enclave has no restart to fall back on. (2) A **gated `/nym-diag`** (closed unless `ZIS_DIAG`, and closed it takes the same pass-through arm as any unknown path, so it is indistinguishable from a build without it) reports whether inbound replies arrive at all, plus the entry gateway the SDK chose — neither readable on an attested enclave, which has no console. `zeronym-shim-reproduce` reports SELF-CONSISTENT across two cold builds on the x86_64 runner with `zebra/` and `zaino/` clean. |
| superseded, `/nym-status` + 90 s lookup budget + send-to-all | `49b85803c4c80441f4776320ca1328197eaf3e779b2a93ab9f537320c304bed7` | the shim publishes its own mixnet-client health (`/nym-status`, `/healthz`), without which an attested shim whose client has died is indistinguishable from a working one — dispatch-only submit answers the wallet before the mixnet is involved. Also: the lookup budget rises 25 s → 90 s (`ZIS_LOOKUP_TIMEOUT_SECS`), and a submit now goes to EVERY `--hub-nym` address rather than one, because without an awaited ack the shim cannot discover that the address it picked is down. `zeronym-shim-reproduce` reports SELF-CONSISTENT across two cold builds with `zebra/` and `zaino/` clean. |
| superseded, best-effort submit + gateway pinning + gated Caution relay | `6985d67c5e6e09cfdbb5b35d0cf87fb7540493e9a310f619a0c1fa16e13b66e5` | four compiled changes land together. (1) `NymHandle::submit` is **dispatch-only**: it answers as soon as the migration is handed to the mixnet instead of awaiting the hub's ack, which is a full round trip and — since neither side runs a validator — only ever meant "the hub queued it". (2) The mixnet client can **pin its entry gateway**, a rotating list on the shim (`ZIS_NYM_GATEWAY`), which is the lever against the gateway backpressure that caps the send rate. (3) The `/attestation`→bootproofd relay is **gated** behind `ZIS_CAUTION_ATTESTATION` with the internal port no longer hardcoded. (4) Code-review fixes, including the hub's fresh-identity gateway-pin deadlock. `zeronym-shim-reproduce` reports SELF-CONSISTENT across two cold builds on the x86_64 runner with `zebra/` and `zaino/` clean. One host confirmed twice, not cross-machine. |
| superseded, parse-critical crates pinned to the hub's | `f1f58af730a725d116f555e1ccfdf4ce61190db661f970119d4d7a7e5b8aebcd` | `orchard` 0.15.4 to 0.15.5 and `halo2_proofs` 0.3.4 to 0.3.5, matching the hub's lockfile per `hub/REVIEW.md`'s rule that the two crates agree on what parses a transaction. Both sides compute txids and the shim's L4 verification fails closed on a mismatch, so a skew would surface as a wallet seeing `not_found` for its own migration. No source change: the hash moved because the compiled dependency stack did. `zeronym-shim-reproduce` reports SELF-CONSISTENT across two cold builds on the x86_64 runner with `zebra/` and `zaino/` clean. NOT yet built on a second architecture, so this is one host confirmed twice, not cross-machine; an arm64 local build would complete the claim the rows below make. |
| superseded, Caution control-plane paths | `8b5ec3fa2365153d78d8d16de911bc1283633bd7dcb057ac36d7b5870f782b6d` | `route_for` owns `/.well-known/caution/health` and `/attestation` instead of proxying them to the indexer, so compiled code changed. ONE build on one host (arm64 under Rosetta). Superseded before `zeronym-shim-reproduce` was ever dispatched on it, so like `418ce662` below it was never CI-confirmed — the value above is the first shim hash since `51ccefed` to be machine-checked at all. |
| superseded, mixnet driver embedded (nym-sdk) | `418ce662de99108a0335b155f6086f52141bbeecc2cc129c6989608abcb9f2f4` | built `--features mixnet-driver` (the deploy default now): links `nym-sdk` so the shim can divert over the Nym mixnet (`--hub-nym`), via the vendored `nym-upgrade-mode-check` `[patch]` and `rand` pinned to 0.9.2. Two independent cold builds agree. See the network exception under Determinism ingredients: the compile RUN keeps the network on for `nym-network-defaults`'s build.rs. Superseded without ever being CI-confirmed. |
| superseded, zebra v25 stack | `dde2ccccaa99b93ba1ef58b1f046366fb99ed7b0e85e3be7da4581569cf510df` | merged main's zebra v25 update: `zebra-chain` 11.2.0 to 11.3.0, which bumped `zcash_primitives` 0.29 to 0.30 (and `zcash_keys`, `zcash_proofs`, `zcash_transparent`). The classifier is unchanged and all 70 tests pass; the hash moved because the compiled dependency stack did, not the predicate or the recipe. |
| superseded, GetTransaction interception | `51ccefed3eda14a55261b06ad3779f3e8c57e1d9c2915ebf3353981ac0b43d5d` | added the `GetTransaction` interception path (`Route::GetTransaction`, `intercept::get_transaction`, `diverted_txid`, `grpc_unary`) plus the divert path and its config, so compiled code changed. Cross-machine confirmed: a native x86_64 CI runner and a local arm64 build under Rosetta agree. |
| superseded, hub-hop ALPN fix | `3e9e1cec7a74f55d66f1bbe7eb4d29534302a38d59310579db5fa0ea711a360c` | the hub hop now negotiates `http/1.1` instead of `h2` (`BackendTls::new_http1`). Found in production: the hub's ALPN-honouring Caddy agreed to h2 and waited for a preface our HTTP/1.1 client never sends, so every diverted migration failed closed as "hub unreachable" over a valid TLS session. |
| superseded, stateless shim (hub-served GetTransaction) | `f498f8224071187220aaffa5408f07ca80c88a44c4fdffee16bf65ad7315ba5d` | removed `DivertState` and route every `GetTransaction` to the hub's new `POST /transaction` (`HubClient::get_transaction`), so the shim holds no per-migration state. Two cold builds agree; `zebra/` and `zaino/` clean. |
| superseded, TLS on both hops | `cd72daf30956fbdbeb76d9e55c723aad7d9d928d09213c37fed8a66d55b3b5a7` | rustls (`ring`) linked in and wired into the serving path: ACME-terminated wallet TLS, WebPKI-verified backend TLS. The binary grows 4.4 MB to 7.6 MB, which is the TLS stack. |
| superseded, commit `c161012ff2` | `4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a` | `is_orchard_touching(tx) := tx has at least one Orchard action`. Zooko's second ruling: every Orchard-touching transaction is diverted, whatever `orchard_value_balance` says. |
| superseded, commit `2243adbdce` | `6257764933df4e2a907f2a0d7d371d42172d5b8350ee5916610c18731bda649f` | the first 2026-08-01 predicate, `is_orchard_exit(tx) := orchard_value_balance > 0`. |
| superseded, recorded to 2026-07-31 | `a9c19f2c3c878da0e2048ff05c075e017a960b3c81c43b631be53f424462ce05` | the pre-2026-08-01 classifier, with the `V6` and `ironwood_value_balance < 0` conjuncts still in the predicate. |

> **Table drift, recorded rather than quietly fixed.** Between the
> `inbound-liveness reroll` row and the current one, `EXPECTED_SHA256` was
> re-baselined to `4f60e630...` without a row being added here, so this table
> named a stale binary as current for that period. `EXPECTED_SHA256` is the
> machine-readable source of truth and was correct throughout; this table is
> prose and was not. If the two ever disagree, believe the file.


The hash moved because the **predicate** moved, not because the recipe did: no
base digest, no flag and no script changed between `62577649…` and `4143ce5f…`.

> **How the current row was measured, and how the chicken-and-egg was resolved.**
> `assemble.sh` archives `git archive HEAD`, so a commit-pinned measurement
> cannot exist until the commit does; but the rule one line above says
> `EXPECTED_SHA256` and the source must land in the **same** commit, and this
> document is part of that commit. Committing first would mean knowingly
> committing a stale hash.
>
> It was resolved in two steps rather than by picking one horn. `4143ce5f…` was
> first measured from a context assembled at `HEAD` and overlaid with the working
> tree, carrying exactly the compiled file set the commit would contain, and was
> recorded as **provisional**. The source then landed as commit `c161012ff2`, and
> `reproduce.sh` was re-run against the committed tree with no overlay. Two cold
> builds agreed with each other and with the published value:
>
> ```
> build 1:  4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
> build 2:  4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
> expected: 4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
> ```
>
> So the row is **no longer provisional**: it describes a real commit, measured
> the ordinary way. The prose you are reading was written after that build, which
> is safe only because no document is a compiled input. The `include_bytes!` calls
> in `src/` are test fixtures inside `#[cfg(test)]` modules and never reach the
> release binary, so editing this file cannot move the hash it reports.
>
> What makes this different from the post-mortem case immediately below, where
> the same overlay shortcut produced a hash that described no commit at all: that
> measurement was taken while `src/` could still change, and it did. This one was
> taken after the last change to compiled code, and then confirmed from the commit
> itself.
>
> **Cross-machine agreement is now also confirmed, so no claim is outstanding.**
> The builds above ran under Rosetta on one arm64 Mac, which controls for time,
> PID and tmpdir and for nothing else.
> `.github/workflows/zeronym-shim-reproduce.yml` then ran the same script on a
> **native x86_64** runner, on different hardware, a different kernel and a
> different BuildKit, and reached `4143ce5f…` again:
> [run 30720417584](https://github.com/ShieldedLabs/zero/actions/runs/30720417584).
> Since the two hosts differ in architecture and in emulation, the agreement also
> rules out CPU-feature-detection-dependent codegen, which is the failure mode
> digest pinning alone would not catch.

> **The hash changed on 2026-08-01, and it changed for a source reason.** The
> classifier predicate was rewritten: the turnstile test became
> `orchard_value_balance > 0` alone, with both the `tx.version == V6` and the
> `ironwood_value_balance < 0` conjuncts dropped. `src/classify.rs`,
> `src/intercept.rs` and `src/lib.rs` all changed, so the compiled binary
> changed with them. (That predicate has since been superseded by the widening
> described above. This paragraph explains the `62577649…` row, and is history
> from here on.)
>
> This is the tripwire doing its job. A recipe whose published hash survived a
> change to the compiled predicate would be the alarming outcome, not this. The
> old value is kept in this document on purpose: an auditor who reproduces
> `a9c19f2c…` has built the **oldest** classifier, and should be able to learn
> that here rather than conclude the recipe is broken. The `62577649…` value is
> confirmed on two machines: see "Cross-machine confirmation, 2026-08-01" below.
> The sections after it are history, and are labelled with the hash each
> attests.

> **Post-mortem: the first published hash for this predicate was wrong, and CI
> caught it.** `EXPECTED_SHA256` briefly recorded
> `c6b7738f6ac2f6f2e6cb58c5b63d4c40b7e4903d057c5a44adcf7b0e01fe1a6a`, a value
> that never corresponded to any commit. It was measured before the predicate
> change had landed, from a context that `assemble.sh` had built from `HEAD` and
> that was then overlaid with the working tree, because `assemble.sh` archives
> `HEAD` by design and would otherwise have compiled the old classifier. The
> overlay was recorded at the time as "byte for byte what the commit will
> record". That was true when written and false by the time the commit was made:
> a later adversarial-review pass changed `src/proxy.rs` in **compiled code**
> (the near-miss path normalization went from `strip_suffix('/')` to
> `trim_end_matches('/')`, so a path with two or more trailing slashes can no
> longer fall out of `InterceptNearMiss` into `PassThrough`, a one-token change
> in the fail-safe direction), along with doc-comment edits to
> `src/classify.rs`.
>
> The first CI run against the commit therefore failed, comparing the real
> binary against a stale expected value. **That failure was the system working.**
> A published hash had drifted from the committed source, and the tripwire
> detected it on its first live test. A green run there would have been the
> alarming outcome.
>
> **The rule this establishes:** a hash is only ever measured from *committed*
> source, or at minimum after the last change to compiled code. Measuring during
> a review pass that can still touch `src/` produces a number describing a tree
> that exists on exactly one machine. The corrected value,
> `6257764933df4e2a907f2a0d7d371d42172d5b8350ee5916610c18731bda649f`, was
> measured from commit `2243adbdce` on two independent machines and is recorded
> in the table above.

> **A second, older provenance defect, found while re-baselining.** The commit
> this section used to record, `f94d194d09`, **is not an ancestor of `HEAD`**
> (`git merge-base --is-ancestor f94d194d09 HEAD` returns false). The branch was
> rebased under it. That is exactly the failure the previous status note warned
> about, and it happened. The lesson is not "record the SHA more carefully", it
> is that a provenance line pointing at a rewritable branch commit is worth less
> than the content hashes above. Both are recorded now.

Source: `zeronym/shim` at zero commit `ad20158cde` **plus the working-tree
classifier change that lands with this document**, content-pinned by the table
above. Target `x86_64-unknown-linux-musl`, built `linux/amd64` under Rosetta on
an arm64 Mac (Docker 29.5.3, `desktop-linux` builder, 16 vCPU).

```
binary sha256:  4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
size:           4393048 bytes
ELF:            x86-64 static-PIE, 35 sections, no INTERP, no NOTE segment
                (hence no build-id), no DT_NEEDED

OCI manifest:   sha256:e95a8de686a08686a1640b628727b575d4002d4e117fa810c83a1b5a390db070
OCI tar sha256: 23bbce863d957e9b1becea9ef766c86b2f019a5580407b096cc0caecf662fa51
```

4393048 bytes is 320 more than the 4392728 this document records for `a9c19f2c…`
and `c6b7738f…`, which is the sort of difference a handful of changed log strings
and one simplified predicate makes. No size was ever recorded for `62577649…`,
so the delta against the immediately preceding binary is unknown and is not being
guessed at here. Size is weak evidence in either direction anyway: the first
re-baseline changed the predicate and the size did not move by a single byte. The
string checks below are what actually distinguish the binaries.

**The two OCI values are weaker claims than the binary hash, and were measured
differently.** The binary was built twice from cold with `--no-cache`. The
runtime image was packaged once, from a build that reused the second cold
build's cached builder layer, so it attests packaging determinism not at all and
is recorded only so the numbers exist. What it does attest, checked rather than
assumed, is routing: the runtime layer was unpacked out of the OCI tar and its
`/zero-indexer-shim` hashes to `4143ce5f…`, byte-identical to the exported
binary, so `COPY --from=export` still ships what it claims.

### Re-baseline, 2026-08-01 (second): the predicate widened

> Measured, not pending. This section was written **before** the build, with the
> procedure and the string assertions stated in advance so they could not be
> retrofitted to whatever came out. The results are recorded underneath each one,
> and the advance text is left standing so the two can be compared.

**Why the hash moved.** Zooko ruled a second time and widened the turnstile
predicate. It was `is_orchard_exit(tx) := orchard_value_balance > 0`. It is now

```text
is_orchard_touching(tx) := tx has at least one Orchard action
```

implemented as `tx.orchard_shielded_data().is_some()`, because an Orchard bundle
is an `AtLeastOne` and so presence and a non-zero action count are the same fact.
`orchard_value_balance` is demoted to evidence and gates nothing. Orchard only:
a transaction carrying only Ironwood actions still passes through, deliberately,
and no Ironwood arm may be added. `src/classify.rs`, `src/intercept.rs` and
`src/lib.rs` changed, so compiled code changed and the binary changed with it.
Nothing about the *recipe* changed: not a base digest, not a flag, not a script.
That matters for reading the result, because it isolates the source edit as the
only moving part. The crate's own `README.md` records the ruling and its
rationale.

**The procedure written in advance, and the one step that could not be
followed.** The advance text called for this order, and said the order was the
whole point:

1. Commit the source change. Do not measure first. `assemble.sh` archives
   `git archive HEAD`, so a hash measured from a working tree describes a tree
   that may never exist as a commit, and the post-mortem above is what happened
   the one time that rule was bent.
2. `EXPECTED= sh zeronym/shim/deploy/reproduce.sh` from the commit. The empty
   `EXPECTED` skips the comparison against the now-stale published value for
   exactly that run, and the script still builds twice from cold and compares the
   two builds against each other.
3. Write the resulting value into `deploy/EXPECTED_SHA256` and into the table
   under "Recorded hashes", moving `62577649…` down to the superseded row, in
   **the same commit**. Those two must never drift apart.
4. Re-run the string check below against the new binary, and record the result
   here.

Steps 2, 3 and 4 were followed. **Step 1 could not be followed in that position**,
and that is a defect in the advance text rather than in the execution: as written,
steps 1 and 3 contradict each other whenever the published hash is itself part of
the commit. The only commit that satisfies step 3 is one that already carries a
value nobody has measured yet. So the measurement was taken from a working-tree
overlay, and most of this section is the provenance that makes that legible
instead of hand-waved.

**The order was wrong, not the requirement.** Step 1 exists to guarantee the hash
describes a real commit, and that guarantee was obtained by doing step 1 *after*
steps 2 to 4 rather than before them. The source landed as `c161012ff2` carrying
the provisional value, and `reproduce.sh` was then re-run against that commit with
no overlay, no `EXPECTED=` override, and therefore a live comparison against the
published file:

```text
build 1:  4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
build 2:  4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
expected: 4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
SELF-CONSISTENT: two cold builds on this host agree.
MATCHES PUBLISHED: 4143ce5f...
zebra/ and zaino/ clean
zero-indexer-shim: REPRODUCES
```

That is the whole overlay argument discharged empirically. Everything below about
which files are and are not build inputs was the *prediction*; this run is the
check, and the prediction held. **So the advance text should be corrected for next
time**: the rule is not "commit first", which is impossible, but *measure from an
overlay, commit, then re-measure from the commit and require the published value
to match*. Three steps, no contradiction, and the failure mode that produced the
post-mortem below cannot survive it, because a hash describing no commit fails the
re-measurement by construction.

**What was measured, and from exactly what.** `HEAD` was `ad20158cde`. At build
time `git status --porcelain zeronym/shim` showed eleven modified tracked files
and two untracked fixtures, of which the compiled set is exactly three:

```
 M zeronym/shim/src/classify.rs
 M zeronym/shim/src/intercept.rs
 M zeronym/shim/src/lib.rs
```

`Cargo.toml` and `Cargo.lock` were byte-identical to `HEAD`, `zebra/` and
`zaino/` were clean, and `deploy/Containerfile` was byte-identical to `HEAD`'s
(asserted with `cmp`, not eyeballed). The other eight modified files are
`README.md`, `demo.sh`, `deploy/README.md`, `examples/shim_demo.rs` and four
files under `tests/`; the two untracked files are `tests/fixtures/*.bin`. **None
of those is a build input.** The Containerfile compiles `cargo build --release
--frozen --target ${TARGET_ARCH} --bin zero-indexer-shim`, which builds the lib
and that one binary: `tests/`, `examples/` and `deploy/` are never compiled, the
crate has no `build.rs`, and `grep -rn 'include_str!\|include!' src/` finds
nothing, so no Markdown file reaches the compiler. The only `include_bytes!` uses
are the four fixtures, all inside `#[cfg(test)] mod tests`, which is why the two
untracked `.bin` files cannot affect a release build.

Three files changed **after** the build, and they are named here rather than
left for someone to notice: `deploy/EXPECTED_SHA256` (which now holds
`4143ce5f…`), this `deploy/README.md`, and the crate `README.md`. All three are
in the non-compiled set above, so the context that produced the measurement
carries their pre-edit contents and the binary is unaffected. That is not an
assumption: the first re-baseline measured builds with four different revisions
of `deploy/` in the context and the binary hash did not move, and the
compiled-input digest recorded below covers `src/`, `Cargo.toml` and
`Cargo.lock` precisely so that this claim can be checked rather than believed.

**The overlay, verbatim, so it can be re-run rather than trusted.** It is the
block recorded under the first re-baseline with one change, marked below:

```sh
cd "$(git rev-parse --show-toplevel)"
CTX=$(mktemp -d)/ctx
sh zeronym/shim/deploy/assemble.sh "$CTX"

# Refuse if a vendored tree is dirty: that is a build input this overlay does
# not carry, so the context would correspond to no possible commit.
test -z "$(git status --porcelain -- zebra/ zaino/)" || exit 1

# CHANGED from the first re-baseline, which instead REFUSED when any untracked
# file existed under the crate. Untracked-but-not-ignored files are carried too,
# so the context is exactly what `git add -A zeronym/shim` would record. The old
# refusal would have blocked this pass outright over two test fixtures.
STAGE=$(mktemp -d)
{ git ls-files -z -- zeronym/shim
  git ls-files -z --others --exclude-standard -- zeronym/shim; } > "$STAGE/list"
tar --null -T "$STAGE/list" -cf "$STAGE/shim-wt.tar"
rm -rf "$CTX/zeronym/shim"
umask 022 && tar -xpf "$STAGE/shim-wt.tar" -C "$CTX"

# The recipe must still be HEAD's. Assert it rather than trust it.
git show HEAD:zeronym/shim/deploy/Containerfile | cmp - "$CTX/zeronym/shim/deploy/Containerfile"

SOURCE_DATE_EPOCH=1 docker build -f "$CTX/zeronym/shim/deploy/Containerfile" "$CTX" \
  --platform linux/amd64 --target export --no-cache --output "type=local,dest=$OUT"
sha256sum < "$OUT/zero-indexer-shim"
```

**Two cold builds, both `--no-cache`, from that one context**, byte-identical to
each other (`cmp` reports no difference):

```
build 1:  4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
build 2:  4143ce5fdffe396adf9937bb975971c850e6b43305a5d5ce3e36deaca3540b5a
size:     4393048 bytes
```

**The measurement did not move underneath itself**, which is the specific
failure the post-mortem above records. A digest of the compiled input set
(`src/**` plus `Cargo.toml` and `Cargo.lock`) was taken before build 1 and again
after build 2, and it is unchanged: `9cb6922a9a475d401ba4d1f6e749714aefe0973da28f972e11e9d336a035e87c`.
That is the mechanical form of the rule the post-mortem established, and it is
also the check a reviewer should re-run: if `find zeronym/shim/src -type f | sort
| xargs shasum -a 256` plus the two manifests no longer digests to that value,
then `4143ce5f…` describes source that no longer exists and must be re-measured.
`git status --porcelain zebra/ zaino/` was empty before and after.

**The string check, run exactly as stated in advance.** A hash that changes is
not by itself evidence that it changed for the stated reason. The advance text
required, and `strings` on `4143ce5f…` found:

| Required | Found |
|---|---|
| one occurrence of `MIGRATION detected: the transaction carries Orchard actions, so it is diverted whatever its Orchard value balance` | 1 |
| one occurrence of `passthrough: SendTransaction carries no Orchard actions` | 1 |
| **zero** of `an Orchard exit, value LEAVING the Orchard pool` | 0 |
| **zero** of `moved no value out of Orchard` | 0 |

The last two are what the `62577649…` binary contains, as recorded under the
first re-baseline, so the two binaries are distinguishable by inspection and not
only by digest. The older `value leaving Orchard and entering Ironwood`, which
belonged to `a9c19f2c…`, is also absent (0). Those counts were measured on this
binary; the claims about what `62577649…` contains are carried over from the
first re-baseline's own measurement and were not re-measured here, since
rebuilding the superseded predicate to re-count a string it was already recorded
as containing would cost another cold build for nothing.

**The usual not-for-a-stupid-reason checks:**

- **Not a stub.** 4393048 bytes, `ELF 64-bit LSB pie executable, x86-64, version
  1 (SYSV), static-pie linked, not stripped`. `e_shnum` 35, ten program headers,
  no `PT_INTERP`, no `PT_NOTE` (hence no build-id), zero `DT_NEEDED` entries.
  The same shape every previous pass recorded.
- **No host paths leak in.** `strings` finds zero occurrences of `/Users/mark`,
  `claude-501`, `scratchpad`, the worktree name `wonderful-villani`, or the
  context directory name `ctx-widened`.
- **The shipped bytes are the audited bytes.** The runtime image's layer was
  unpacked out of the OCI tar and its `/zero-indexer-shim` hashes to
  `4143ce5f…`, `cmp`-identical to the exported binary.

**Timings, and an honest note about them.** Build 1 took 2475 s wall clock and
build 2 took 891 s, about 56 minutes for the pair, with `cargo fetch` at 11.6 s
and 8.3 s. Cargo's own compile timer reported 40m58s and 14m41s, against the
1m34s to 3m16s this recipe records everywhere else. That is host contention, not
a recipe regression: `uptime` reported load averages above 29 for the whole of
build 1 and 5.07 by the time build 2 finished, and the two builds produced
identical bytes despite a 2.8x spread in how long they took. Note the difference
from the outlier recorded under the first re-baseline, where wall clock blew out
to 33m while cargo's internal timer stayed at 2m15s: there the container was
fine and the host was thrashing around it, here the container was itself
CPU-starved throughout. Neither is a property of the build.

**What is still owed, and it is the part that matters.** Half of what this
section originally listed as owed has since been paid, and the half that remains
is the more interesting one.

*Paid.* The overlay question is closed. `reproduce.sh` was re-run against commit
`c161012ff2` with no overlay and a live comparison, and it matched. So the
overlay was faithful, nothing reached the compiler that this section claims did
not, and `4143ce5f…` describes a real commit.

*Also paid.* Cross-machine agreement, which was the last thing owed.
`.github/workflows/zeronym-shim-reproduce.yml` ran `reproduce.sh` on a **native
x86_64** runner and reached `4143ce5f…`, matching `EXPECTED_SHA256`:
[run 30720417584](https://github.com/ShieldedLabs/zero/actions/runs/30720417584).
The two hosts differ in architecture, in emulation (Rosetta versus native),
in kernel and in BuildKit build, so the agreement covers more than a rerun on
similar hardware would: it rules out CPU-feature-detection-dependent codegen,
which digest pinning alone does not address. `4143ce5f…` is therefore settled on
the same footing as `62577649…` and `a9c19f2c…` before it.

**Nothing about the build is outstanding.** What remains untested is the enclave
half of the chain, which is a different claim entirely and is recorded under
"What is proven, and what is not": no PCR has been computed from this binary and
no attestation document exists, so the hash-to-attestation binding that motivates
the whole exercise is designed rather than demonstrated.

**One consequence to expect before the commit lands.** `EXPECTED_SHA256` now
holds `4143ce5f…` while `HEAD` still compiles `62577649…`, so running
`reproduce.sh` in this working tree **fails right now**, and that is the correct
reading rather than a regression. It is strictly more useful than the state this
replaced: with the old value the script passed while describing code that had
already been rewritten, which is the "confirmed for the wrong tree" case named
under "What is proven, and what is not". A red run here means "you have not
committed yet", and it goes green with the commit.

### Re-baseline, 2026-08-01 (first): the classifier predicate changed

> Superseded. This section attests `62577649…` and the predicate
> `orchard_value_balance > 0`, which the widening above replaces. The recipe
> facts in it (control build, mtime and context-path independence, the
> five-cold-build pass) all still hold.

**Why the hash moved.** The turnstile predicate became
`is_orchard_exit(tx) := orchard_value_balance > 0`, dropping the `V6` and
`ironwood_value_balance < 0` conjuncts. Compiled code changed, so the binary
changed. Nothing about the *recipe* changed: not a base digest, not a flag, not
a script. That matters for reading the result, because it isolates the source
edit as the only moving part.

**Control build first.** Before touching anything, one cold `reproduce.sh` build
ran against a pristine `git archive HEAD` context at `22a92f8fe6` and produced

```
a9c19f2c3c878da0e2048ff05c075e017a960b3c81c43b631be53f424462ce05   (the old hash, unchanged)
```

This is the load-bearing control, and it establishes two things at once. This
host still reproduces the published hash, so the pipeline and the machine are
sound and any later difference is attributable to the source edit rather than to
drift. And `HEAD` genuinely still compiles the old classifier, which is what the
provenance caveat above asserts. It also settles a real question raised by the
history: `zebra/Cargo.toml` lost its `[patch.crates-io] orchard = { path =
"../orchard" }` block between the previously recorded commit and `HEAD`, and
`zebra/Cargo.toml` **is** a context input. The hash did not move, which
empirically confirms `assemble.sh`'s claim that zebra's workspace patch does not
reach the shim's own workspace.

That `reproduce.sh` invocation was stopped after its first cold build rather than
run to completion. Build 1 had already answered the only question being asked of
it, and the second build would have re-proven same-host self-consistency of a
hash that three prior sessions and a CI run already attest. So this pass contains
**one** control build, not two.

**How the uncommitted source got into the context**, in full, because a
procedure that only one session can run is not a reproduction. `assemble.sh` is
used unmodified, then the crate directory is replaced with the working-tree copy
of the same tracked file set:

```sh
cd "$(git rev-parse --show-toplevel)"
CTX=$(mktemp -d)/ctx
sh zeronym/shim/deploy/assemble.sh "$CTX"

# Two refusals, because either condition means the context corresponds to no
# possible commit and its hash would be meaningless. A dirty vendored tree is a
# build input this overlay does not carry; an untracked file under the crate
# would land in the commit but not in `git ls-files` below.
test -z "$(git status --porcelain -- zebra/ zaino/)" || exit 1
test -z "$(git ls-files --others --exclude-standard -- zeronym/shim)" || exit 1

# Same file set `git archive` would take, current contents, tracked files only.
STAGE=$(mktemp -d)
git ls-files -z -- zeronym/shim | tar --null -T - -cf "$STAGE/shim-wt.tar"
rm -rf "$CTX/zeronym/shim"
umask 022 && tar -xpf "$STAGE/shim-wt.tar" -C "$CTX"

# The recipe must still be HEAD's. deploy/ is unmodified here, so these are
# byte-identical; assert it rather than trust it.
git show HEAD:zeronym/shim/deploy/Containerfile | cmp - "$CTX/zeronym/shim/deploy/Containerfile"

# Artifacts outside the checkout, so `git status` stays a clean signal.
OUT=$(mktemp -d)
SOURCE_DATE_EPOCH=1 docker build -f "$CTX/zeronym/shim/deploy/Containerfile" "$CTX" \
  --platform linux/amd64 --target export --no-cache --output "type=local,dest=$OUT"
sha256sum < "$OUT/zero-indexer-shim"
```

That block was then executed **verbatim, as extracted from this file**, from a
third working directory and with a third context path (`mktemp -d` under
`/var/folders/…`), and printed `c6b7738f…`. The only adaptation was
`sha256sum` to `shasum -a 256`, because the host is macOS; the block is written
for the Linux audience the rest of this document addresses. Compile 2m06s,
`cargo fetch` 41.6s.

This is a stopgap for a deliberately uncommitted change, not a supported mode.
It is not wired into any script, because a context that can silently include
working-tree state is exactly the hole `assemble.sh` was written to close. After
the commit lands, `reproduce.sh` is the only correct path and this block is
history.

**Five cold builds of the new source in total**, all `--no-cache`: the verbatim
run just described, plus the four below, from two more separately assembled
contexts, varying the two dimensions a single host lets you vary here:

| | build A | build B |
|---|---|---|
| Host context path | `…/scratchpad/ctx-newA` | `…/scratchpad/b/deeply/nested/differently/sized/host/path/for/context-b/ctx` |
| Context file mtimes | working-tree mtimes (wall clock, all different) | every entry forced to `2000-01-01 00:00` |
| Targets built | `export`, then `runtime` (OCI) | `export`, then `runtime` (OCI) |

The mtime axis is the one worth explaining. `git archive` stamps every file with
the commit timestamp, so a committed context has uniform mtimes that these
working-tree contexts do not have, and the commit that eventually lands will
stamp its own. Forcing build B's entire context to a fixed, wildly different
timestamp tests that the compiler output does not depend on any of that. It does
not:

```
binary sha256:  c6b7738f6ac2f6f2e6cb58c5b63d4c40b7e4903d057c5a44adcf7b0e01fe1a6a   (A == B)
OCI tar sha256: 2c97d8f4f7a8cb82284d2b500c4ca3ae5c7da00b5b3a9973471dfd9cc5b3df14   (A == B)
OCI manifest:   sha256:33c0f4f12bdeb9b47f77d5cea7e479c2bddc6ef7067e790072115221d0bb9460   (A == B)
```

`cmp` reports no difference between the two exported binaries, or between the
two OCI tars.

**It is the new classifier, checked rather than assumed.** A hash that changes is
not by itself evidence that it changed for the stated reason, so, *as measured at
the time on the first-predicate binary*:

- That binary contains the then-new operator-visible log strings (`an Orchard
  exit, value LEAVING the Orchard pool`, and `moved no value out of Orchard`),
  one occurrence each.
- It contains **zero** occurrences of the older `value leaving Orchard and
  entering Ironwood`, which the control binary from `HEAD` at the time does
  contain. The two binaries are distinguishable by inspection, not only by
  digest.

> Both of those strings are **gone from the source** as of the widening. They are
> what an auditor should expect to find in `62577649…` and to find zero of in
> `4143ce5f…`, which was measured and holds: see the string-check table under
> "Re-baseline, 2026-08-01 (second)". Do not read this pair of bullets as a claim
> about the current `src/`.

**The usual not-for-a-stupid-reason checks, re-run on the new binary:**

- **Not a stub.** 4392728 bytes, `ELF 64-bit LSB pie executable, x86-64,
  static-pie linked`, `e_shnum` 35, no `INTERP` segment, no `NOTE` segment
  (hence no build-id), zero `DT_NEEDED` entries. Loaded from the OCI tar and
  run: `--version` prints `zero-indexer-shim 0.1.0` and `--help` prints the real
  `--listen` / `--backend` flag set with their `ZIS_*` env bindings.
- **No host paths leak in.** `strings` finds zero occurrences of `/Users/mark`,
  `claude-501`, `scratchpad`, the worktree name, or any of the context directory
  names. The three very different context paths above are the live test of it.
- **The shipped bytes are the audited bytes.** The runtime image's last layer was
  unpacked out of the OCI tar and its `/zero-indexer-shim` hashes to
  `c6b7738f…`, the same value as the exported binary, so `COPY --from=export`
  still routes what it claims.
- **Vendored subtrees untouched.** `git status --porcelain zebra/ zaino/` is
  empty after every build.

**Timings, and an honest note about them.** Host wall clock for the A/B pass:
export A 3m02s, OCI A 6m02s, export B 33m10s, OCI B 3m43s, 45m57s in total. The
export B outlier is machine contention, not a property of the recipe (host load
average was above 30 for most of it, and Spotlight was indexing concurrently);
cargo's own timer inside that same step reported 2m15s, in line with every other
build. `cargo fetch` took 41.6s, 46s and 58s across the pass. Cargo's
self-reported compile times were 2m06s, 2m08s, 2m13s, 2m15s and 3m16s, so the
standing "under three minutes to compile" claim above still holds wherever the
host is not oversubscribed. Do not read the 33m figure as a recipe regression;
read it as a reminder that wall clock on a shared desktop measures the desktop.

### Independent re-verification, 2026-07-31

> Everything from here to the end of the hardening pass attests the **superseded**
> binary hash `a9c19f2c…`, built from the pre-2026-08-01 classifier. The recipe
> facts in these passes (frontend pin, POSIX rewrite, umask, archived recipe,
> cross-machine agreement) all still hold; the hash they landed on, and the test
> count recorded alongside it, are historical. Cross-machine agreement has since
> been re-measured on the **current** source and holds for `62577649…` (see
> "What is proven, and what is not"). `c6b7738f…` was never built anywhere but
> this host, because it was only ever a working-tree overlay.

A second pass rebuilt everything from scratch, deliberately varying every
dimension a single host lets you vary:

| | build A | build B |
|---|---|---|
| BuildKit instance | a **fresh `docker-container` builder** (`docker buildx create`), empty build cache, its own image store, re-pulled both StageX bases by digest [1] | Docker Desktop's `desktop-linux` builder |
| Host context path | `…/verify/ctxA` | `…/verify/deeply/nested/differently/sized/host/path/for/context-b/ctx` |
| Working directory | `…/verify` | `<repo>/zeronym/shim/src` |
| Invocation | `docker buildx build --no-cache` directly | the documented `build.sh --no-cache` |

Each of A and B builds two targets (`export` and `runtime`), and `--no-cache`
invalidates the builder stage for each, so the pass contains **four independent
cold compiles** of all 276 crates. All four produced the same bytes:

```
binary sha256:  a9c19f2c3c878da0e2048ff05c075e017a960b3c81c43b631be53f424462ce05  (A == B == then-recorded)
OCI tar sha256: 8a19102ed78277f54cb97a43dad7725d8dcf98c1392c7ae811dfb10fc449b651  (A == B == then-recorded)
OCI manifest:   sha256:c657f0c87fc879e941455ecf2750eb47ca6833c398af142e078d8308b8c9db2a  (A == B == then-recorded)
```

`cmp` reports no differences between the two exported binaries or the two OCI
tars. Three further checks, so that "it reproduces" is not being satisfied for a
stupid reason:

- **It is not a stub.** 4392728 bytes, `ELF 64-bit LSB pie executable, x86-64,
  static-pie linked`, 35 section headers, no INTERP, no build-id, no
  `DT_NEEDED`. It executes: `--version` prints `zero-indexer-shim 0.1.0` and
  `--help` prints the real flag set.
- **No host paths leak in.** `strings` finds zero occurrences of any host path
  fragment. The only embedded absolute paths are the in-image ones the recipe
  pins (`/usr/src/app/…`, `/usr/local/cargo/registry/…`). This is what makes the
  *host* context path free while the *in-image* paths stay load-bearing, and the
  two wildly different context paths above are the test of it.
- **The shipped bytes are the audited bytes.** Unpacking the runtime image's
  layers yields a `/zero-indexer-shim` whose sha256 is the same `a9c19f2c…`, so
  the `COPY --from=export` routing does what it claims.

Blocker (b) re-confirmed directly rather than inferred: `protoc` is absent from
`pallet-rust` (`command -v protoc` fails, no protoc binary on disk) and `PROTOC`
is unset, and both builds logged the `unused import: BlockId` warning from
`zaino-proto`'s committed `src/proto/utils.rs`, which is the generated source
compiling verbatim. Vendored subtrees were empty in `git status --porcelain`
after every build.

Timings for this pass: build A 16m28s for the export target, but 14 of those
minutes were the fresh builder pulling 2.5 GB of StageX bases over a slow link;
its actual compile was 2m51s, and its second (OCI) target took 3m12s total.
Build B, with images already local, took 14m25s for both targets while
contending with A for bandwidth. Compile time alone is consistently 1m34s to
2m51s.

[1] **Caveat on build A's builder independence.** The fresh `docker-container`
builder was torn down afterwards and its log was not kept, so `docker buildx ls`
no longer shows it and that particular detail now rests on the prior session's
word rather than on retained evidence. The artifacts and the two distinct
context directories were kept and do match, so the reproducibility conclusion
stands on its own; it is the *builder-was-fresh* claim specifically that is no
longer independently checkable. Keep the build log next time, as was done for
`build1.log` and `reproduce.log` in the pass below.

### Third-party-hardening pass, 2026-07-31

A determinism review found three things that would have broken reproduction for
a third party on a clean machine, none of which a same-host repeat could ever
surface. All three are fixed, and **fixing them did not change any hash**, which
is itself the useful result: the recipe was already deterministic, it was the
*path to running it* that was broken.

| Was | Now |
|---|---|
| `assemble.sh` carried `set -euo pipefail` under a `#!/usr/bin/env bash` shebang, but every caller and every documented command invoked it as `sh assemble.sh`, which bypasses the shebang. On Debian and Ubuntu `/bin/sh` is dash, which has no `pipefail`, so the script died on line 2 with `set: Illegal option -o pipefail` before doing anything. CI used `bash` and stayed green while the published recipe was broken for exactly the audience it exists to serve. | POSIX `#!/bin/sh` and `set -eu`, no pipelines. Each `git archive` writes a tar whose exit status `set -e` actually checks. Verified in an `ubuntu:24.04` container and a `debian:bookworm-slim` container: `dash -n` clean on all three scripts, and a full `sh assemble.sh` run to exit 0. The old two-line construction was re-run in the same container to confirm it still fails with `exit=2`. |
| `# syntax=docker/dockerfile:1`, the only image reference in the build not pinned by digest, and the one that compiles this file into LLB. A frontend release can change layer construction and therefore the published OCI manifest digest. Measured on 2026-07-31: `docker/dockerfile:1` resolved to `sha256:87999aa3…`, a genuinely different image from `1.26.0`, so the tag does float. | `# syntax=docker/dockerfile:1.26.0@sha256:ecfaec9e…`, pinned like everything else and listed in the determinism ingredients above so it gets bumped deliberately. Both frontends happen to produce identical artifacts for this file, which is a fact about two versions rather than a property to rely on. |
| The Containerfile was the one file in the context taken from the **working tree** (`cp` at the end of `assemble.sh`) rather than from `git archive HEAD`, while a commit-pinned copy sat unused inside the context. An auditor at the recorded commit with a locally modified recipe would build something else and nothing would say so. Its wall-clock mtime was the giveaway: two `assemble.sh` runs two seconds apart differed in exactly that one file. | The `cp` is gone. Every caller builds `-f "$CTX/zeronym/shim/deploy/Containerfile"`, the archived copy. `assemble.sh` warns if the working tree has drifted from HEAD, and warns much more loudly if `deploy/` is not committed at all. |

Four smaller items from the same review, also fixed:

- **The CI job could go green on the exact failure it exists to catch.** It
  compared build 1 to build 2 and never to the published hash, so a runner that
  deterministically produced *some other* hash passed. The published value now
  lives in `EXPECTED_SHA256`, `reproduce.sh` asserts against it, and the
  workflow runs `reproduce.sh` itself rather than a copy of its logic (which
  also means CI finally exercises the documented command, `--platform
  linux/amd64` included, instead of a hand-rolled near-miss). The verdict logic
  was unit-tested under dash across all five branches; the case that used to
  pass, "agrees with itself, differs from published", now exits 1.
- **The CI `paths:` filter omitted both vendored path dependencies**, so the
  routine operation in this repo, a subtree pull, could change the binary and
  stale the published hash without ever running the job. `zebra/Cargo.toml`,
  `zebra/zebra-chain/**`, `zebra/zebra-test/**`, `zaino/Cargo.toml` and
  `zaino/packages/zaino-proto/**` are now in the filter, with cross-references
  in both files so the list and `assemble.sh` do not drift apart.
- **Context file modes depended on the invoking user's umask**, because
  `git archive | tar -x` masks recorded modes. Fixed with `umask 022` and
  `tar -xp`. Measured: two assemblies under umask 077 and umask 022 now produce
  identical modes across all 476 entries. This moves no hash today and is
  purely a trap removed for later.
- **`36 sections` was wrong**; `e_shnum` is 35.

Everything was then rebuilt: **five more cold builds**, from four separately
assembled contexts, one via `build.sh`, two via `reproduce.sh`, two via
`build.sh --no-cache` on the finished files. The frontend pin, the POSIX
rewrite, the umask change and the archived-recipe switch are all
**hash-neutral**:

```
binary sha256:  a9c19f2c3c878da0e2048ff05c075e017a960b3c81c43b631be53f424462ce05  (unchanged)
OCI tar sha256: 8a19102ed78277f54cb97a43dad7725d8dcf98c1392c7ae811dfb10fc449b651  (unchanged)
OCI manifest:   sha256:c657f0c87fc879e941455ecf2750eb47ca6833c398af142e078d8308b8c9db2a  (unchanged)
```

`cmp` reports no difference between the first and last binary, or between the
first and last OCI tar. The binary runs: launched from the pinned busybox
runtime base, `--version` prints `zero-indexer-shim 0.1.0`.

Two of those builds ran on frontend `sha256:87999aa3…` (what the old floating
`1` tag resolved to) and three on the pinned `1.26.0`. All five agree, which
says the pin cost nothing here even though it is the right thing to have.

That result carries one bonus, and it is the answer to the obvious objection
about the status note above. These builds had the whole of `deploy/` sitting in
the build context, where the earlier ones did not, and four different revisions
of `deploy/` were in play across them (this README changed between builds). The
binary hash did not move. So committing this directory will not re-baseline
anything, which is the sort of thing that is easy to assume and cheap to check.

`cargo test --locked` passed 56 tests **at that time**. Do not read that as a
current figure; it has now gone stale twice, because each predicate change
brought its own tests with it. The invariant worth recording is that the suite
passes with **one** ignored test, `regenerate_fixtures`, which rewrites
`tests/fixtures/` and is meant to be run explicitly. `git status --porcelain
zebra/ zaino/` is empty after every build and after the host test run.

Timings for this pass, cold, on the same arm64 Mac under Rosetta: `cargo fetch`
54 s and 65 s, compile 106 s and 104 s, so under three minutes per build and
5m50s for the `reproduce.sh` pair. Build logs were retained this time
(`reproduce.log`, `build1.log`, `build-final.log`), per the caveat on build A
above.

### What is proven, and what is not

- **Proven:** deterministic across repeated cold builds on this host, including
  across a fresh BuildKit instance with an empty cache and its own image store,
  across several very different host context paths, across two working
  directories, across both the scripted and the hand-run invocation, across
  context file mtimes differing by 26 years, and across the recipe rewrite in
  the hardening pass above. Every cold build attempted so far, across four
  sessions, has landed on the hash implied by its own source: `a9c19f2c…` for
  the old classifier (every build up to and including the 2026-08-01 control),
  `c6b7738f…` for the uncommitted working-tree overlay (five builds, three
  contexts, superseded, see the post-mortem under "Recorded hashes"),
  `62577649…` for the committed `orchard_value_balance > 0` classifier, and
  `4143ce5f…` for the widened presence predicate (two cold builds, one context,
  one host). Vendored subtrees untouched (`git status --porcelain zebra/ zaino/`
  empty after every build). The binary runs and is a real 4.2 MB static-PIE
  executable, not a stub.
- **Proven: the hash tracks the source.** A change to the compiled predicate
  moved the published hash, and a control build of the unchanged tree in the
  same session did not. The recipe is therefore sensitive to what it is supposed
  to be sensitive to, which is the property that makes a stale hash detectable
  at all. It is easy to demonstrate determinism and never demonstrate this one.
- **Specifically ruled out:** host-path leakage. Builds whose context paths
  differ in length, depth and content produced identical bytes, on both the old
  and the new source, and `strings` finds no host path in the binary at all.
- **Specifically ruled out:** context file mtimes affecting the artifact. A
  context with wall-clock mtimes and one forced entirely to `2000-01-01`
  produced identical binaries and identical OCI tars. This is what lets a hash
  measured from a working tree stand once the same content is committed, since
  `git archive` will then stamp everything with the commit timestamp.
- **Specifically ruled out:** `deploy/` content affecting the artifact. Builds
  with and without this directory in the context, and with three different
  revisions of it, all produced the same binary. Nothing here is compiled.
- **Specifically ruled out:** the reproduction scripts needing bash. They run
  under dash, confirmed in a Debian container, which is the shell a third party
  on Ubuntu will actually hand them.
- **Proven, for the current hash: determinism across *independent hardware*, and
  across execution modes.** Two data points on commit `2243adbdce`, measured not
  argued. GitHub Actions run
  [30688786506](https://github.com/ShieldedLabs/zero/actions/runs/30688786506)
  built from that commit on a `blacksmith-16vcpu-ubuntu-2404` runner (native
  x86_64 Linux) and produced
  `6257764933df4e2a907f2a0d7d371d42172d5b8350ee5916610c18731bda649f`; its
  artifact was downloaded and hashed independently rather than trusting the
  job's own comparison. A local `build.sh` on arm64 macOS, `linux/amd64` under
  Rosetta, from the same commit, produced the same value. That closes two axes
  at once: a different machine (CPU, kernel, filesystem, paths, Docker and
  BuildKit versions), and a different *execution mode* for the compiler, which
  is a stronger check than two native builds because it also rules out codegen
  varying with runtime CPU feature detection.
  **That run is recorded as failed, and that is the point.** It compared the
  binary against the then-stale `EXPECTED_SHA256` and refused to agree, catching
  a published hash that had drifted from the committed source on the tripwire's
  first live test. See the post-mortem under "Recorded hashes".
  The earlier run
  [30681137118](https://github.com/ShieldedLabs/zero/actions/runs/30681137118)
  attests the same properties for `a9c19f2c…`, the old classifier, and is kept
  as history.
  Not measured: OCI **image digest** agreement across machines. Only the binary
  was compared.
- **Proven for the current hash: commit-pinned reproducibility across two
  architectures.** `4143ce5f…`, the widened presence predicate, has two cold
  builds behind it from commit `c161012ff2` itself, with no working-tree overlay,
  agreeing with each other and with `EXPECTED_SHA256`; and a third on a **native
  x86_64** GitHub runner reaching the same value
  ([run 30720417584](https://github.com/ShieldedLabs/zero/actions/runs/30720417584)).
  The two hosts differ in architecture, emulation, kernel and BuildKit, so this
  also rules out CPU-feature-detection-dependent codegen. Same footing as
  `62577649…` and `a9c19f2c…`.
  Not measured, as for those: OCI **image digest** agreement. Only the binary was
  compared.
- **Not yet proven: the enclave half of the chain, which is now the only
  untested link.** This binary has never run inside a Nitro enclave. No PCR0,
  PCR1 or PCR2 has been computed from this image, no EIF has been assembled from
  it, and no attestation document exists. So the hash-to-attestation binding
  that motivates the entire exercise is *designed*, not *demonstrated*. Nothing
  above is false about the build; it is simply half of a two-link chain, and the
  other link is untouched.
- **Proven: that a third party can run these instructions at all.** The recipe
  is pushed, and CI executed the documented procedure verbatim (`sh
  zeronym/shim/deploy/reproduce.sh`) on a clean machine that had never seen this
  repository, reaching the hash recorded at that time. A hash nobody else can
  recompute would be unfalsifiable, which is the opposite of the point.
- **An invariant, not a snapshot: `EXPECTED_SHA256` is the hash of `HEAD`, never
  of your working tree.** Every script here builds `git archive HEAD`, so an
  uncommitted change under `src/`, or to `Cargo.toml` or `Cargo.lock`, is simply
  absent from the build and `reproduce.sh` will pass while describing code you
  did not change. The two states that follow from that are worth naming
  separately, because they look identical from the outside and mean opposite
  things:
  - `reproduce.sh` **passes** and `HEAD` has no pending compiled change: the
    published hash is confirmed. This is the steady state.
  - `reproduce.sh` **passes** and a compiled change is sitting in the working
    tree: the published hash is confirmed *for the wrong tree*. The commit-time
    run then **re-baselines** the published value rather than confirming it, and
    `EXPECTED_SHA256` must move in that same commit. `git status --porcelain
    zeronym/shim/src zeronym/shim/Cargo.toml zeronym/shim/Cargo.lock` is the
    check that tells the two apart.
  - `reproduce.sh` **fails** and a compiled change is sitting in the working
    tree, with `EXPECTED_SHA256` already re-baselined ahead of the commit: the
    published value describes the tree you are about to commit, not `HEAD`. This
    is the state as of 2026-08-01 for the widened predicate, and it is
    deliberate; see "Re-baseline, 2026-08-01 (second)". It resolves the moment
    the change is committed, and it is preferable to the case above, which is
    silent.
  A red `reproduce.sh` after a commit that touched compiled source is the
  tripwire working, and it has already caught one drifted hash on its first live
  test.

## Reproducing this yourself

Anyone with Docker and a checkout of this repo at the recorded commit can
recompute the hash. Nothing else is needed: no network beyond the base-image
pull and `cargo fetch`, no toolchain install, no protoc, and no bash (the
scripts are POSIX sh and run under dash).

> **Read the provenance note under "Recorded hashes" first.** This procedure
> reproduces whatever the commit you check out compiles, and compares it to that
> commit's `EXPECTED_SHA256`. At a commit whose compiled source matches its
> published hash it passes, and that is the steady state. Two commits reproduce
> older values on purpose: `ad20158cde` and `2243adbdce` both compile
> `62577649…`, the superseded `orchard_value_balance > 0` predicate, because the
> widening had not been committed yet. The one case where a PASS means nothing is
> a commit whose classifier change is still sitting in a working tree, since
> `git archive HEAD` cannot see it.

```sh
git clone https://github.com/ShieldedLabs/zero && cd zero
git checkout <the commit recorded below>

# Assemble the context (git archive HEAD, so your working tree is irrelevant)
# and build twice from cold, comparing the two against each other and against
# deploy/EXPECTED_SHA256.
sh zeronym/shim/deploy/reproduce.sh
```

`reproduce.sh` exits non-zero on any of: the two builds disagreeing with each
other, either build disagreeing with `EXPECTED_SHA256`, or a dirtied vendored
subtree. A clean exit is the whole claim.

To compare against the published hash by hand, without the wrapper:

```sh
CTX=$(mktemp -d)/ctx
sh zeronym/shim/deploy/assemble.sh "$CTX"

# Note the -f path: the Containerfile from INSIDE the context, which came out of
# `git archive HEAD`. Not your working-tree copy. That is what makes "I rebuilt
# commit X" mean something.
SOURCE_DATE_EPOCH=1 docker build \
  -f "$CTX/zeronym/shim/deploy/Containerfile" "$CTX" \
  --platform linux/amd64 --target export --no-cache \
  --output type=local,dest=out

sha256sum < out/zero-indexer-shim
# compare against deploy/EXPECTED_SHA256
```

Use `sha256sum < file`, the stdin form, so the filename never enters the digest
text and two differently-named copies compare cleanly.

If your hash differs, check these in order:

1. **In-image paths.** They are pinned (`WORKDIR /usr/src/app/zeronym/shim`,
   `CARGO_HOME=/usr/local/cargo`) and rustc embeds them. Editing either changes
   the hash. Your *host* paths do not matter and have been tested not to.
2. **Cache mounts.** If someone has added a `--mount=type=cache` to the
   Containerfile, `--no-cache` will not clear it and the build is no longer cold.
3. **A `rust-toolchain.toml`.** There is none today, and adding one whose channel
   differs from `pallet-rust:1.96.0` makes rustup fetch a different compiler.
4. **`stagex/user-protobuf` or a `PROTOC` env var.** Either one lets
   `zaino-proto`'s `build.rs` regenerate its committed protos, silently changing
   what gets compiled.
5. **The frontend pin.** `# syntax=docker/dockerfile:1.26.0@sha256:ecfaec9e…`
   on line 1 of the Containerfile. A different frontend can emit different LLB.
6. **Commit.** `git archive HEAD` means an unexpected `HEAD` silently builds
   different source. Confirm `git rev-parse HEAD`. If `assemble.sh` prints a
   `deploy/ is NOT COMMITTED` warning, stop: your context is not commit-pinned
   and its hash is not comparable to a published one.
7. **Uncommitted source, the quiet one.** There is no warning for this case, and
   it is the one that actually bit us. `git archive HEAD` ignores your working
   tree entirely, so an edit under `src/` that you have not committed is simply
   absent from the build, and every script here will confirm the hash of the
   code you did **not** change. `git status --porcelain zeronym/shim` before you
   trust a result. Note the asymmetry with item 6: a modified `Containerfile`
   gets you a warning, a modified `classify.rs` gets you silence.

If the **binary** matches but the **OCI tar** does not, that is a packaging-layer
difference (Docker or BuildKit version), not a build-determinism failure. The
binary hash is the load-bearing claim, because that is what an enclave
attestation binds.

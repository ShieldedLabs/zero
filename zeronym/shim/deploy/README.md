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

## Recorded hashes

Do not populate this section by hand, and never with a plausible-looking
placeholder. A wrong hash in an audit document is worse than a missing one. The
machine-readable copy is `deploy/EXPECTED_SHA256`; the two must move together.

> **Status, and the one thing an auditor cannot do yet.** At the time of
> writing, `zeronym/shim/deploy/` and the reproduce workflow are **not yet
> committed**. `assemble.sh` says so loudly when that is true, and until they
> are committed and pushed, the checkout-and-run procedure above has nothing to
> check out: a third party at the recorded commit finds no `deploy/` directory.
> The compiled *binary* provenance is unaffected, because `assemble.sh` archives
> `HEAD` and the Rust sources really are at that commit, but **the hash below is
> not yet independently recomputable by anyone.** Two follow-ups, in order:
> commit and push, then update the commit recorded here to the SHA that actually
> lands (a rebase or squash on merge changes it, which silently invalidates both
> the checkout instruction and the provenance line).
>
> Committing `deploy/` does not change the hash, and that is measured rather
> than assumed: the uncommitted-fallback path in `assemble.sh` puts the whole of
> `deploy/` into the context exactly where the archive will, and the build below
> produced the same binary hash as builds made before `deploy/` existed. Nothing
> under `deploy/` is compiled; cargo builds `src/` and the manifest.

Source: `zeronym/shim` at zero commit `f94d194d09`. Target
`x86_64-unknown-linux-musl`, built `linux/amd64` under Rosetta on an arm64 Mac
(Docker 29.5.3, BuildKit v0.30.0, 16 vCPU).

```
binary sha256:  a9c19f2c3c878da0e2048ff05c075e017a960b3c81c43b631be53f424462ce05
size:           4392728 bytes
ELF:            x86-64 static-PIE, no INTERP, no build-id, no DT_NEEDED

OCI manifest:   sha256:c657f0c87fc879e941455ecf2750eb47ca6833c398af142e078d8308b8c9db2a
OCI tar sha256: 8a19102ed78277f54cb97a43dad7725d8dcf98c1392c7ae811dfb10fc449b651
```

**Three cold builds produced that binary hash.** One via `build.sh`, two via
`reproduce.sh --no-cache`, using two different host context directories. The
context path not mattering is worth noting separately: only the *in-image*
paths are pinned, so the host location of the context is free.

**The OCI packaging reproduced too.** Two independent cold `--target runtime`
builds produced byte-identical tars (`8a19102e...`) and the same manifest
digest, which is the stronger of the two packaging outcomes and not something
to assume: `rewrite-timestamp` normalises layer mtimes, but compression and
layer ordering also have to land the same way.

Timings, cold: 78 s `cargo fetch`, 96 s to compile all 276 crates, under three
minutes end to end. `reproduce.sh` took 5m25s for both builds.

### Independent re-verification, 2026-07-31

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
binary sha256:  a9c19f2c3c878da0e2048ff05c075e017a960b3c81c43b631be53f424462ce05  (A == B == recorded)
OCI tar sha256: 8a19102ed78277f54cb97a43dad7725d8dcf98c1392c7ae811dfb10fc449b651  (A == B == recorded)
OCI manifest:   sha256:c657f0c87fc879e941455ecf2750eb47ca6833c398af142e078d8308b8c9db2a  (A == B == recorded)
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

`cargo test --locked` passes 56 tests, and `git status --porcelain zebra/
zaino/` is empty after every build and after the host test run.

Timings for this pass, cold, on the same arm64 Mac under Rosetta: `cargo fetch`
54 s and 65 s, compile 106 s and 104 s, so under three minutes per build and
5m50s for the `reproduce.sh` pair. Build logs were retained this time
(`reproduce.log`, `build1.log`, `build-final.log`), per the caveat on build A
above.

### What is proven, and what is not

- **Proven:** deterministic across repeated cold builds on this host, including
  across a fresh BuildKit instance with an empty cache and its own image store,
  across two very different host context paths, across two working directories,
  across both the scripted and the hand-run invocation, and across the recipe
  rewrite in the hardening pass above. Every cold build attempted so far, across
  three sessions, has produced `a9c19f2c…`. Vendored subtrees untouched
  (`git status --porcelain zebra/ zaino/` empty after every build). The binary
  runs and is a real 4.2 MB static-PIE executable, not a stub.
- **Specifically ruled out:** host-path leakage. Two builds whose context paths
  differ in length, depth and content produced identical bytes, and `strings`
  finds no host path in the binary at all.
- **Specifically ruled out:** `deploy/` content affecting the artifact. Builds
  with and without this directory in the context, and with three different
  revisions of it, all produced the same binary. Nothing here is compiled.
- **Specifically ruled out:** the reproduction scripts needing bash. They run
  under dash, confirmed in a Debian container, which is the shell a third party
  on Ubuntu will actually hand them.
- **Proven: determinism across *independent hardware*, and across execution
  modes.** GitHub Actions run
  [30681137118](https://github.com/ShieldedLabs/zero/actions/runs/30681137118)
  built this recipe twice from cold on a `blacksmith-16vcpu-ubuntu-2404` runner
  (native x86_64 Linux) and landed on the recorded hash
  `a9c19f2c3c878da0e2048ff05c075e017a960b3c81c43b631be53f424462ce05`. The
  binary it produced was downloaded and hashed independently, rather than
  trusting the job's own comparison. Because the recorded hash was originally
  produced on arm64 macOS under Rosetta emulation, that single result closes two
  axes at once: a different machine (CPU, kernel, filesystem, paths, Docker and
  BuildKit versions), and a different *execution mode* for the compiler itself.
  The latter is a stronger check than two native builds would have been, since
  it also rules out codegen that varies with runtime CPU feature detection.
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
  repository, reaching the recorded hash. A hash nobody else can recompute would
  be unfalsifiable, which is the opposite of the point; this one has now been
  recomputed by a machine we do not control.

## Reproducing this yourself

Anyone with Docker and a checkout of this repo at the recorded commit can
recompute the hash. Nothing else is needed: no network beyond the base-image
pull and `cargo fetch`, no toolchain install, no protoc, and no bash (the
scripts are POSIX sh and run under dash).

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

If the **binary** matches but the **OCI tar** does not, that is a packaging-layer
difference (Docker or BuildKit version), not a build-determinism failure. The
binary hash is the load-bearing claim, because that is what an enclave
attestation binds.

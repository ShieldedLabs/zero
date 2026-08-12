# syntax=docker/dockerfile:1.26.0@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=UndefinedVar,UserExist

# The frontend above is pinned BY DIGEST, like every other image here, and for
# the same reason: it parses this file and compiles it into LLB, so an unpinned
# `docker/dockerfile:1` would hand the interpretation of every COPY, RUN and
# --network=none to whatever version Docker Hub served that day, and a frontend
# release can change emitted LLB, hence layer construction, hence the OCI
# manifest digest. Bump this digest deliberately, with a hash re-baseline, and
# update deploy/README.md's determinism-ingredient list when you do. This is the
# exact digest the sibling zeronym/shim recipe pins; see its Containerfile for
# the full argument.

# Reproducible StageX build of zero-indexer-hub: one static-musl binary.
#
# Sibling of zeronym/shim/deploy/Containerfile, which does the same for the
# shim. Same determinism ingredients (StageX bases pinned by digest,
# SOURCE_DATE_EPOCH=1, codegen-units=1, crt-static, --build-id=none, committed
# lockfile via --locked/--frozen, no BuildKit cache mounts).
#
# WHY THIS EXISTS: the hub is the one component that sees migration transactions
# in PLAINTEXT. It receives the bytes the shim removed from the operator's link
# and broadcasts them to the network, so attestation is not optional here, it is
# the whole basis for anyone trusting the hub with that plaintext. The Zeronym
# trust model gives the auditor the job of rebuilding from source, getting the
# same hash, and matching it against the hash bound into the enclave
# attestation. Without that, an attestation proves only that SOME binary runs
# inside a genuine enclave, which collapses the design back into trusting
# whoever compiled it.
#
# Build context = a partial mirror of the zero repo (assemble via assemble.sh):
#   zeronym/hub/                  the crate, including this file under deploy/
#   zebra/Cargo.toml              workspace root zebra-chain inherits from
#   zebra/zebra-chain/            the vendored Zcash parser (the path dep)
#   zebra/zebra-test/             optional dep of zebra-chain, manifest only
# The layout is the repo's own, so the hub's `../../zebra/zebra-chain` path dep
# resolves unchanged. No manifest is edited, anywhere. Unlike the shim, the hub
# has NO zaino-proto dependency (it speaks JSON-RPC to full nodes, not the
# CompactTxStreamer gRPC), so zaino/ is deliberately absent from the context.
#
# BUILD THIS FILE FROM INSIDE THE CONTEXT, not from the working tree:
#   docker build -f "$CTX/zeronym/hub/deploy/Containerfile" "$CTX" ...
# assemble.sh puts a `git archive HEAD` copy there precisely so that the recipe,
# which IS the definition of the build, is pinned to the same commit as the
# sources it compiles.
#
# Neither the hub nor the repo root carries a rust-toolchain.toml, so the pinned
# pallet-rust digest IS the toolchain pin. If one is ever added and its channel
# differs from the image, rustup will try to download a toolchain, which needs
# network and destroys determinism. Do not add one.

ARG TARGET_ARCH="x86_64-unknown-linux-musl"

############################################################
# StageX bases, pinned by digest
############################################################
# pallet-rust is the ONLY builder pallet needed. It already ships rustc 1.96.0,
# clang targeting x86_64-unknown-linux-musl, /usr/bin/cc, ar, mold, ld.lld,
# /usr/include and /usr/lib/libc.a.
#
# DO NOT add stagex/user-protobuf. zaino-proto's build.rs regenerates its
# committed src/proto/*.rs whenever protoc is reachable, and while
# default-features = false already removes the `which::which("protoc")` branch,
# the PROTOC env-var branch is NOT feature-gated. An image with no protoc in it
# at all is the second, independent lock: nothing to find, nothing to
# regenerate. Never set PROTOC either.
FROM stagex/pallet-rust:1.96.0@sha256:abe9b95c93a5afa271f69fcd5eb18c8cd405fe5df6491a63c9418e3a170573dc AS pallet-rust
FROM stagex/core-busybox:1.38.0@sha256:e4a30addc8939c8e232472de904d1d9e97fc2e735fca9a9701ce49db04c6c181 AS busybox

############################################################
# Builder
############################################################
FROM pallet-rust AS builder
ARG TARGET_ARCH
# Which cargo features to compile. The deploy target is the mixnet hub, so this
# defaults to `mixnet-driver` (links nym-sdk; the binary still serves clearnet on
# --listen when --nym is unset). The feature CHANGES the binary and therefore
# EXPECTED_SHA256, so a rebaseline goes with any change to it. Build the leaner
# clearnet-only hub with `--build-arg CARGO_FEATURES=` (empty), which drops
# nym-sdk entirely.
ARG CARGO_FEATURES="mixnet-driver"
SHELL ["/bin/sh", "-euo", "pipefail", "-c"]

WORKDIR /usr/src/app

# CARGO_HOME and WORKDIR are load-bearing for reproducibility, not taste. rustc
# embeds absolute paths, and this recipe pins them rather than relying on
# --remap-path-prefix. An auditor who rebuilds at a different path gets a
# different hash and will think the build failed.
ENV CARGO_HOME=/usr/local/cargo
ENV CARGO_INCREMENTAL=0
ENV RUST_BACKTRACE=1
ENV RUSTFLAGS="-C codegen-units=1"
ENV RUSTFLAGS="${RUSTFLAGS} -C target-feature=+crt-static"
ENV RUSTFLAGS="${RUSTFLAGS} -C linker=clang -C link-arg=-fuse-ld=mold"
ENV RUSTFLAGS="${RUSTFLAGS} -C link-arg=-Wl,--build-id=none"
ENV SOURCE_DATE_EPOCH=1
# Deliberately ABSENT versus the caution-zaino reference: the rocksdb and
# libzcash_script link-args (CXXSTDLIB, --whole-archive, libc++.a shims, and so
# on). The hub's graph, like the shim's, has neither: its only C dependency is
# secp256k1-sys (pure C, via cc), and zcash_script is the pure-Rust
# reimplementation. It links clean without them. If a future dependency breaks
# the link, restore the reference's flags before debugging anything else.

# Repo-shaped context. Three COPYs so the layer cache is invalidated by the piece
# that actually changed.
COPY zebra/ ./zebra/
COPY zaino/ ./zaino/
COPY zeronym/ ./zeronym/

WORKDIR /usr/src/app/zeronym/hub

# Two phases so the network is only open for the fetch. --locked here and
# --frozen below make the committed Cargo.lock authoritative: any drift is a
# hard build failure rather than a silent re-resolution. That is what pins the
# hub's orchard from crates.io (the hub is its own workspace and does not
# inherit zebra's [patch.crates-io]), and it is also a free guard on the
# zaino-proto feature set, since a regression to default features would change
# the lock and fail the build.
RUN cargo fetch --locked --target ${TARGET_ARCH}

# No BuildKit cache mounts, anywhere. `docker build --no-cache` does NOT clear
# cache mounts, so a recipe that uses them cannot honestly support a
# two-cold-builds reproducibility proof.
# NETWORK RELAXATION for the mixnet build. This RUN keeps the network ON: nym-sdk
# (the mixnet-driver feature) pulls nym-network-defaults, whose build.rs runs
# `cargo metadata` over the WHOLE nym workspace purely to locate its own
# envs/mainnet.env, resolving nym's unrelated wasm members and their git deps
# (e.g. nymtech/smoltcp) which are NOT in this crate's lockfile and so were never
# `cargo fetch`ed; offline it dies resolving github. Determinism is NOT lost:
# every version is still pinned (our --frozen lock, nym's own lock at the tag), so
# the network only fetches content already pinned by rev/hash. git-fetch-with-cli
# makes those arbitrary-rev git deps fetch reliably. CAVEAT: this weakens the
# "offline build (--network=none)" ingredient in deploy/README.md; a fully
# hermetic mixnet build must pre-warm nym's workspace metadata cache during the
# fetch phase and set CARGO_NET_OFFLINE for this RUN. Tracked in NYM_PLAN.md M6.
RUN CARGO_NET_GIT_FETCH_WITH_CLI=true \
    cargo build --release --frozen --target ${TARGET_ARCH} \
      ${CARGO_FEATURES:+--features "${CARGO_FEATURES}"} --bin zero-indexer-hub && \
    install -D -m 0755 target/${TARGET_ARCH}/release/zero-indexer-hub \
      /usr/local/bin/zero-indexer-hub

############################################################
# Export stage: the artifact under audit, with nothing around it
############################################################
# `docker build --target export --output type=local,dest=DIR` drops the bare
# binary on the host. This is what the reproducibility check hashes.
FROM scratch AS export
COPY --from=builder /usr/local/bin/zero-indexer-hub /zero-indexer-hub

############################################################
# Runtime
############################################################
FROM busybox AS runtime

# The stagex busybox base is usr-merged (/lib -> usr/lib, /lib64 -> usr/lib) but
# ships no usr/lib, so /lib and /lib64 are DANGLING symlinks. Caution's EIF
# builder runs `test -e <rootfs>/lib || mkdir -p <rootfs>/lib`, and mkdir cannot
# create through a dangling symlink, so initramfs assembly dies with "No such
# file or directory". Materialising the targets makes the `test -e` succeed.
# The binary is static musl, so these stay empty. USER root is required because
# the stagex base runs as uid 1000 and cannot mkdir in /usr.
USER root
RUN mkdir -p /usr/lib /etc/ssl/certs /tmp && chmod 1777 /tmp

# The hub speaks plaintext HTTP/1.1 and has no TLS stack in its dependency graph
# today (on Caution the platform terminates TLS in-enclave and forwards here, and
# the shim->hub hop is protected by the transport around it). This CA bundle is
# purely defensive against a future dependency that expects a system trust store
# (a stale zaino image once failed at startup with "No CA certificates were
# loaded from the system"). Sourcing it from the same pinned pallet-rust keeps it
# deterministic.
COPY --from=pallet-rust /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copy THROUGH the export stage, so the bytes an auditor hashes and the bytes
# that ship are provably the same file.
COPY --from=export /zero-indexer-hub /zero-indexer-hub

# Configuration is ZIH_LISTEN plus at least one ZIH_NODES entry (comma-separated
# host:port), with optional ZIH_NODE_USER / ZIH_NODE_PASSWORD. The listen default
# is 0.0.0.0:8090; a Caution enclave sets ZIH_LISTEN=0.0.0.0:8083 to match the
# platform's forwarding. See deploy/caution/caution.hcl.tmpl.
ENTRYPOINT ["/zero-indexer-hub"]

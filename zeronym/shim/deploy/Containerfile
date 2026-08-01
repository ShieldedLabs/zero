# syntax=docker/dockerfile:1.26.0@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=skip=UndefinedVar,UserExist

# The frontend above is pinned BY DIGEST, like every other image here, and for
# the same reason. It is the component that parses this file and compiles it
# into LLB, so the unpinned `docker/dockerfile:1` this file used to carry handed
# the interpretation of every COPY, RUN, --network=none and export directive to
# whatever version Docker Hub happened to be serving that day. That tag really
# does float, measured rather than assumed: on 2026-07-31 `docker/dockerfile:1`
# resolved to sha256:87999aa3..., a DIFFERENT image from the 1.26.0 pinned here.
# A frontend release can change emitted LLB, hence layer construction, hence the
# OCI manifest digest this recipe publishes as a constant.
#
# (As it happens both frontends produce the same artifacts for this file: builds
# on 87999aa3 and on ecfaec9e agree to the byte, on the binary and on the OCI
# tar. That is luck about two particular versions, not a property to rely on.)
#
# The sibling caution-zaino recipes still use the floating tag; that is a flaw
# inherited from them, not a precedent to follow. The directive cannot simply be
# deleted, because the `check=` line below needs frontend 1.8+. Bump this digest
# deliberately, with a hash re-baseline, and update deploy/README.md's
# determinism-ingredient list when you do.

# Reproducible StageX build of zero-indexer-shim: one static-musl binary.
#
# Sibling of deploy/caution-zaino/combined/Containerfile, which does the same
# job for zebrad + zainod. Same determinism ingredients (StageX bases pinned by
# digest, SOURCE_DATE_EPOCH=1, codegen-units=1, crt-static, --build-id=none,
# committed lockfile via --locked/--frozen, no BuildKit cache mounts), but with
# every rocksdb / libzcash_script workaround SUBTRACTED, because the shim's
# dependency graph contains neither.
#
# WHY THIS EXISTS: the Zeronym trust model gives the auditor the job of
# rebuilding from source, getting the same hash, and matching it against the
# hash bound into the enclave attestation. Without that, an attestation proves
# only that SOME binary runs inside a genuine enclave, which collapses the whole
# design back into trusting whoever compiled it.
#
# Build context = a partial mirror of the zero repo (assemble via assemble.sh):
#   zeronym/shim/                 the crate, including this file under deploy/
#   zebra/Cargo.toml              workspace root zebra-chain inherits from
#   zebra/zebra-chain/            the vendored Zcash parser (the path dep)
#   zebra/zebra-test/             optional dep of zebra-chain, manifest only
#   zaino/Cargo.toml              workspace root zaino-proto inherits from
#   zaino/packages/zaino-proto/   the CompactTxStreamer codegen (the path dep)
# The layout is the repo's own, so the shim's `../../zebra/zebra-chain` and
# `../../zaino/packages/zaino-proto` path deps resolve unchanged. No manifest is
# edited, anywhere.
#
# BUILD THIS FILE FROM INSIDE THE CONTEXT, not from the working tree:
#   docker build -f "$CTX/zeronym/shim/deploy/Containerfile" "$CTX" ...
# assemble.sh puts a `git archive HEAD` copy there precisely so that the recipe,
# which IS the definition of the build, is pinned to the same commit as the
# sources it compiles. Building the working-tree copy instead would let an
# auditor at the recorded commit unknowingly build a different recipe. Nothing
# under deploy/ is compiled, so its presence in the context does not affect the
# binary hash; that is measured, not assumed.
#
# Neither the shim nor the repo root carries a rust-toolchain.toml, so the
# pinned pallet-rust digest IS the toolchain pin. If one is ever added and its
# channel differs from the image, rustup will try to download a toolchain, which
# needs network and destroys determinism. Do not add one.

ARG TARGET_ARCH="x86_64-unknown-linux-musl"

############################################################
# StageX bases, pinned by digest
############################################################
# pallet-rust is the ONLY builder pallet needed. It already ships rustc 1.96.0,
# clang 22.1.5 targeting x86_64-unknown-linux-musl, /usr/bin/cc, ar, mold,
# ld.lld, /usr/include and /usr/lib/libc.a. The reference recipe additionally
# copies pallet-clang, user-protobuf and user-abseil-cpp; all three are
# unnecessary here and user-protobuf is actively harmful (see below).
FROM stagex/pallet-rust:1.96.0@sha256:abe9b95c93a5afa271f69fcd5eb18c8cd405fe5df6491a63c9418e3a170573dc AS pallet-rust
FROM stagex/core-busybox:1.38.0@sha256:e4a30addc8939c8e232472de904d1d9e97fc2e735fca9a9701ce49db04c6c181 AS busybox

############################################################
# Builder
############################################################
FROM pallet-rust AS builder
ARG TARGET_ARCH
SHELL ["/bin/sh", "-euo", "pipefail", "-c"]

# DO NOT add stagex/user-protobuf here. zaino-proto's build.rs regenerates its
# committed src/proto/*.rs whenever protoc is reachable, and while
# default-features = false already removes the `which::which("protoc")` branch,
# the PROTOC env-var branch of protoc_available() is NOT feature-gated. An image
# with no protoc in it at all is the second, independent lock: nothing to find,
# nothing to regenerate, and the committed protos compile verbatim. Never set
# PROTOC either.

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
# Deliberately ABSENT versus the reference: CXXSTDLIB, CXXFLAGS,
# ROCKSDB_USE_PKG_CONFIG, the libc++.a / libc++abi.a / libzstd.a / libz.a
# link-args, --whole-archive, --allow-multiple-definition, and the
# /usr/lib/libstdc++.a INPUT() shim. Every one of those exists for rocksdb or
# libzcash_script. The shim's graph has neither: its only C dependency is
# secp256k1-sys (pure C, via cc), and zcash_script 0.4.5 is the pure-Rust
# reimplementation. It links clean without them. If a future dependency breaks
# the link, restore the reference's flags before debugging anything else.

# Repo-shaped context. Three COPYs rather than one so the layer cache is
# invalidated by the piece that actually changed.
COPY zebra/ ./zebra/
COPY zaino/ ./zaino/
COPY zeronym/ ./zeronym/

WORKDIR /usr/src/app/zeronym/shim

# Two phases so the network is only open for the fetch. --locked here and
# --frozen below make the committed Cargo.lock authoritative: any drift is a
# hard build failure rather than a silent re-resolution. That is what pins the
# shim's orchard 0.15.4 from crates.io (the shim is its own workspace and does
# not inherit zebra's [patch.crates-io]), and it is also a free guard on the
# zaino-proto feature set, since a regression to default features would change
# the lock and fail the build.
RUN cargo fetch --locked --target ${TARGET_ARCH}

# No BuildKit cache mounts, anywhere. `docker build --no-cache` does NOT clear
# cache mounts, so a recipe that uses them cannot honestly support a
# two-cold-builds reproducibility proof.
RUN --network=none \
    cargo build --release --frozen --target ${TARGET_ARCH} --bin zero-indexer-shim && \
    install -D -m 0755 target/${TARGET_ARCH}/release/zero-indexer-shim \
      /usr/local/bin/zero-indexer-shim

############################################################
# Export stage: the artifact under audit, with nothing around it
############################################################
# `docker build --target export --output type=local,dest=DIR` drops the bare
# binary on the host. This is what the reproducibility check hashes.
FROM scratch AS export
COPY --from=builder /usr/local/bin/zero-indexer-shim /zero-indexer-shim

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

# The shim speaks plaintext h2c and has no TLS stack in its dependency graph
# today, so this bundle is purely defensive (a stale zaino image once failed at
# startup with "No CA certificates were loaded from the system"). Sourcing it
# from the same pinned pallet-rust keeps it deterministic.
COPY --from=pallet-rust /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copy THROUGH the export stage, so the bytes an auditor hashes and the bytes
# that ship are provably the same file.
COPY --from=export /zero-indexer-shim /zero-indexer-shim

# Configuration is two flags, or ZIS_LISTEN / ZIS_BACKEND in the environment.
# Defaults are 127.0.0.1:9068 (listen) and 127.0.0.1:9067 (backend); a container
# or enclave deployment will want ZIS_LISTEN=0.0.0.0:9068.
ENTRYPOINT ["/zero-indexer-shim"]

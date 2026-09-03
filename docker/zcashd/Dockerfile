# Build the Zero zcashd from vendored source (multi-stage).
#
# Build context is the repo's `zcashd/` directory:
#   docker build -f docker/zcashd/Dockerfile -t zero-zcashd zcashd
#
# Unlike zcashd/contrib/docker/Dockerfile (which apt-installs the upstream zcash
# release), this compiles the vendored Zero source via ./zcutil/build.sh.
#
# Two build stages: `depends` (about 13 of the 15 cold minutes at -j16, and
# unchanged between subtree pulls) is a cached layer that an edit under src/
# does not invalidate, so a source-only change rebuilds zcashd alone, about
# 2 minutes. `--target depends` builds just the first stage.

# ---- depends ---------------------------------------------------------------
FROM debian:bookworm AS depends

# zcashd build dependencies. The bundled `depends/` system builds its own Rust,
# Boost, BDB, libc++, etc., so no rustup or extra crypto libs are needed here.
RUN apt-get update && apt-get install -y --no-install-recommends \
        autoconf automake bsdmainutils build-essential ca-certificates \
        curl g++ git libtool m4 ncurses-dev pkg-config python3 unzip wget \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

# build.sh exports this for the whole build; the builder stage below runs its
# steps directly, so set it once here.
ENV LC_ALL=C
WORKDIR /src

# Everything `make -C depends` reads: depends/ itself, ../Cargo.toml and
# ../Cargo.lock (it vendors the crates), and zcutil/ (build.sh drives it, and
# bdb installs its db_* tools into zcutil/bin). Only build.sh from zcutil/, so
# churn in the release scripts does not rebuild this layer.
COPY depends/ depends/
COPY Cargo.toml Cargo.lock ./
COPY zcutil/build.sh zcutil/build.sh

# The fetched tarballs and per-package caches are read only by a later
# `make -C depends`, which nothing below runs: ~2.8 GB less to push and pull.
# /root/.cargo stays: the zcashd build resolves the [patch.crates-io] git
# crates from it offline.
RUN BUILD_STAGE=depends ./zcutil/build.sh -j"$(nproc)" \
    && rm -rf depends/sources depends/built depends/work

# ---- builder ---------------------------------------------------------------
FROM depends AS builder

COPY . /src

# The steps build.sh runs after its `make -C depends`. Not build.sh itself:
# that make is never a no-op (the package targets are phony, so it removes and
# re-extracts the whole depends prefix every time), and with the caches
# trimmed above it would fail. Strip the shipped binaries.
RUN ./zcutil/clean.sh \
    && ./autogen.sh \
    && CONFIG_SITE="$PWD/depends/$(./depends/config.guess)/share/config.site" ./configure --quiet \
    && make -j"$(nproc)" \
    && strip src/zcashd src/zcash-cli src/zcash-tx

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# zcashd statically links most of its deps; only these remain dynamic.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libgomp1 zlib1g \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. The named volume mounted at the datadir inherits this
# ownership on first use.
RUN useradd --create-home --home-dir /srv/zcash --shell /bin/bash --uid 2001 zcash \
    && mkdir -p /srv/zcash/.zcash \
    && chown -R zcash:zcash /srv/zcash

COPY --from=builder /src/src/zcashd     /usr/local/bin/zcashd
COPY --from=builder /src/src/zcash-cli  /usr/local/bin/zcash-cli
COPY --from=builder /src/src/zcash-tx   /usr/local/bin/zcash-tx

RUN zcashd --version

ENV HOME=/srv/zcash
WORKDIR /srv/zcash
USER zcash

# Sapling/Orchard proving parameters are generated at runtime; no download
# needed. (Only spending legacy Sprout funds requires sprout-groth16.params.)
ENTRYPOINT ["zcashd"]
CMD ["-printtoconsole"]

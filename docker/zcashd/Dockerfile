# Build the Zero zcashd from vendored source (multi-stage).
#
# Build context is the repo's `zcashd/` directory:
#   docker build -f docker/zcashd/Dockerfile -t zero-zcashd zcashd
#
# Unlike zcashd/contrib/docker/Dockerfile (which apt-installs the upstream zcash
# release), this compiles the vendored Zero source via ./zcutil/build.sh.

# ---- builder ---------------------------------------------------------------
FROM debian:bookworm AS builder

# zcashd build dependencies. The bundled `depends/` system builds its own Rust,
# Boost, BDB, libc++, etc., so no rustup or extra crypto libs are needed here.
RUN apt-get update && apt-get install -y --no-install-recommends \
        autoconf automake bsdmainutils build-essential ca-certificates \
        curl g++ git libtool m4 ncurses-dev pkg-config python3 unzip wget \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . /src
WORKDIR /src

# Builds depends/ then zcashd. ~15-25 min cold. Strip the shipped binaries.
RUN ./zcutil/build.sh -j"$(nproc)" \
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

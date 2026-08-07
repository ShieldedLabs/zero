# Build the Zero lightwalletd from vendored source (multi-stage).
#
# Build context is the repo's `lightwalletd/` directory:
#   docker build -f docker/lightwalletd/Dockerfile -t zero-lightwalletd lightwalletd
#
# Zero owns this file rather than using lightwalletd/Dockerfile (the vendored
# upstream one) for two reasons. First, images.yml builds every component with
# `target: runtime`, and upstream's Dockerfile has a single stage named
# `lightwalletd_base`, so it cannot satisfy that target. Second, upstream's image
# ships the whole Go toolchain in the final layer: it measures 433 MB where this
# one is a slim runtime plus a static binary. Same pattern, and the same reason,
# as docker/zcashd/Dockerfile.

# ---- builder ---------------------------------------------------------------
FROM golang:1.25 AS builder

WORKDIR /src

# Dependencies first, so a source-only change does not re-download the module
# graph. go.sum is copied with go.mod because `go mod download` verifies against
# it and fails without it.
COPY go.mod go.sum ./
RUN go mod download

COPY . .

# The version stamps come in as build args rather than from git. The Makefile
# derives them with `git describe` / `git rev-parse`, but the build context here
# is the lightwalletd subdirectory, which is not a git repository, so under the
# vendored Dockerfile those shell-outs silently produce empty strings and the
# binary reports `version ""`. Passing them explicitly is the difference between
# an image that can say what it is and one that cannot.
ARG LWD_VERSION=zero-dev
ARG LWD_GIT_COMMIT=
ARG LWD_BUILD_DATE=

# CGO_ENABLED=0 because nothing here imports "C"; the result is a static binary
# that needs no libc in the runtime layer. -w -s drop DWARF and the symbol
# table, which is a large fraction of a Go binary's size and is not wanted in a
# release image.
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath \
        -ldflags "-w -s \
          -X github.com/zcash/lightwalletd/common.Version=${LWD_VERSION} \
          -X github.com/zcash/lightwalletd/common.GitCommit=${LWD_GIT_COMMIT} \
          -X github.com/zcash/lightwalletd/common.BuildDate=${LWD_BUILD_DATE} \
          -X github.com/zcash/lightwalletd/common.BuildUser=zero-ci" \
        -o /out/lightwalletd . \
    && /out/lightwalletd --help > /dev/null

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates only: the binary is static, and lightwalletd needs a trust
# store to speak TLS to anything outside the cluster.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# uid 2002 and the /var/lib/lightwalletd datadir match upstream's Dockerfile and
# the `--data-dir` default, so a volume written by either image is readable by
# the other.
RUN useradd --create-home --home-dir /srv/lightwalletd --shell /bin/bash \
        --uid 2002 lightwalletd \
    && mkdir -p /var/lib/lightwalletd/db \
    && chown -R lightwalletd:lightwalletd /var/lib/lightwalletd

COPY --from=builder /out/lightwalletd /usr/local/bin/lightwalletd

WORKDIR /srv/lightwalletd
USER lightwalletd

# No CMD with default flags on purpose. lightwalletd requires TLS unless
# --no-tls-very-insecure is passed, and baking that into the image would make
# the insecure mode the path of least resistance. The deployment states it.
ENTRYPOINT ["lightwalletd"]
CMD ["--help"]

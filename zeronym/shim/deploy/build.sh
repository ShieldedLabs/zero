#!/bin/sh
# Reproducible build of zero-indexer-shim, using the same deterministic docker
# flags as deploy/caution-zaino/combined/build.sh, which in turn takes them from
# zcash/zallet utils/build.sh (line 21): type=oci + rewrite-timestamp +
# force-compression, with SOURCE_DATE_EPOCH=1 exported into the shell so the
# image packaging is timestamp-stable as well as the binary.
#
# The binary hash is the claim that matters, because that is what gets bound
# into the enclave attestation and what an auditor recomputes. The OCI digest is
# a second, weaker claim about the packaging.
#
# PLATFORM, STATED PLAINLY: this builds linux/amd64 targeting
# x86_64-unknown-linux-musl. Every StageX image pinned here is published for
# amd64 ONLY (verified with `docker manifest inspect`), so there is no such
# thing as a native arm64 build of this recipe; an arm64 variant would need
# substitute base images and would prove nothing about the deployable artifact.
# amd64 is also the AWS Nitro enclave target. On an arm64 Mac this runs under
# Rosetta emulation, and the zebrad + zainod recipe's warning against emulation
# does not transfer: the shim has no rocksdb and no libzcash_script, and a cold
# build here measures under three minutes (78 s fetch, 96 s compile).
#
# Env overrides: CTX (assembled context dir), OUT (artifact dir). Extra args are
# forwarded to `docker build`.

set -e

ZERO_ROOT="$(git rev-parse --show-toplevel)"
CTX="${CTX:-$(mktemp -d)/zero-indexer-shim-ctx}"
# Outside the repo on purpose. Part of the audit ritual is checking that a build
# left the tree clean (`git status --porcelain zebra/ zaino/`), and artifacts
# dropped inside the checkout make that check noisier than it needs to be.
OUT="${OUT:-$(dirname "$ZERO_ROOT")/zero-indexer-shim-build}"

export DOCKER_BUILDKIT=1
export SOURCE_DATE_EPOCH=1

sh "$ZERO_ROOT/zeronym/shim/deploy/assemble.sh" "$CTX"
mkdir -p "$OUT"

# The recipe INSIDE the context, which assemble.sh took from `git archive HEAD`.
# Not the working-tree copy: the Containerfile is the whole definition of the
# build, so building the working-tree copy would mean an auditor at the recorded
# commit could unknowingly build a different recipe.
RECIPE="$CTX/zeronym/shim/deploy/Containerfile"

echo "Extracting the binary from the export stage..."
docker build -f "$RECIPE" "$CTX" \
	--platform linux/amd64 \
	--target export \
	--output "type=local,dest=$OUT/bin" \
	"$@"

echo "Building the runtime image (deterministic OCI)..."
docker build -f "$RECIPE" "$CTX" \
	--platform linux/amd64 \
	--target runtime \
	--output "type=oci,rewrite-timestamp=true,force-compression=true,dest=$OUT/zero-indexer-shim.tar,name=zero-indexer-shim" \
	"$@"

echo
echo "=== artifacts ==="
echo "binary:    $OUT/bin/zero-indexer-shim"
echo "OCI image: $OUT/zero-indexer-shim.tar"
echo
echo "=== binary sha256 (this is the hash an auditor reproduces) ==="
# stdin form, so the filename never enters the digest text.
sha256sum < "$OUT/bin/zero-indexer-shim" 2>/dev/null \
	|| shasum -a 256 < "$OUT/bin/zero-indexer-shim"
echo
echo "=== OCI tar sha256 ==="
sha256sum < "$OUT/zero-indexer-shim.tar" 2>/dev/null \
	|| shasum -a 256 < "$OUT/zero-indexer-shim.tar"

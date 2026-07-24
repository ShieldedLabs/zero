#!/bin/sh
# Local reproducible build of the combined z3 enclave image, using the same
# deterministic docker flags as zcash/zallet utils/build.sh (line 21):
# type=oci + rewrite-timestamp + force-compression, with SOURCE_DATE_EPOCH=1.
#
# You do NOT need this to deploy: the caution backend auto-adds these flags,
# builds the image, and transplants it into EnclaveOS. This script is for
# local reproduction / verification and to document the exact flag set.
#
# Build on x86_64. arm64 via emulation is far too slow (Anton's warning).
#
# Env overrides: CTX (assembled context dir), OUT (artifact dir). Extra args
# are forwarded to `docker build`.

set -e

ZERO_ROOT="$(git rev-parse --show-toplevel)"
CTX="${CTX:-$(mktemp -d)/z3-enclave}"
OUT="${OUT:-$ZERO_ROOT/build/oci}"

export DOCKER_BUILDKIT=1
export SOURCE_DATE_EPOCH=1

sh "$ZERO_ROOT/deploy/caution-zaino/combined/assemble-combined.sh" "$CTX"
mkdir -p "$OUT"

echo "Building combined runtime image (deterministic OCI)..."
docker build -f "$CTX/Containerfile" "$CTX" \
	--platform linux/amd64 \
	--target runtime \
	--output "type=oci,rewrite-timestamp=true,force-compression=true,dest=$OUT/z3.tar,name=z3-node" \
	"$@"

echo "Extracting binaries from the export stages..."
docker build -f "$CTX/Containerfile" "$CTX" --quiet \
	--platform linux/amd64 --target export-zebra \
	--output "type=local,dest=$OUT/zebra" "$@"
docker build -f "$CTX/Containerfile" "$CTX" --quiet \
	--platform linux/amd64 --target export-zaino \
	--output "type=local,dest=$OUT/zaino" "$@"

echo "OCI image: $OUT/z3.tar"
echo "binaries:  $OUT/zebra/zebrad  $OUT/zaino/zainod"

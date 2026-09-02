#!/bin/sh

set -e

UTILS_DIR="$(CDPATH= cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(CDPATH= cd "$UTILS_DIR/.." && pwd)"

PLATFORM="linux/amd64"
OCI_OUTPUT="$REPO_ROOT/build/oci"
DOCKERFILE="$REPO_ROOT/Dockerfile"

export DOCKER_BUILDKIT=1
export SOURCE_DATE_EPOCH=1

echo "$DOCKERFILE"
mkdir -p "$OCI_OUTPUT"

echo "Building runtime image..."
docker build --file "$DOCKERFILE" \
       --platform "$PLATFORM" \
       --target runtime \
       --output "type=oci,rewrite-timestamp=true,force-compression=true,dest=$OCI_OUTPUT/zallet.tar,name=zallet" \
       "$@" "$REPO_ROOT"

echo "Extracting binary..."
docker build --file "$DOCKERFILE" --quiet \
       --platform "$PLATFORM" \
       --target export \
       --output "type=local,dest=$REPO_ROOT/build" \
       "$@" "$REPO_ROOT"

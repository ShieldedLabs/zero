#!/bin/sh
# The Auditor Role, in one script: build zero-indexer-hub twice from cold, check
# that the two binaries are byte-identical, AND check that they equal the hash
# this repo publishes in deploy/EXPECTED_SHA256.
#
# Both halves matter, and the second is the one that is easy to leave out. Two
# builds that agree with each other but not with the published hash is exactly
# the cross-machine divergence this script exists to catch, and without the
# EXPECTED comparison that case exits 0.
#
# WHAT THIS PROVES, AND WHAT IT DOES NOT. Two builds on ONE machine control for
# time, PID, tmpdir and build ordering. They do NOT control for CPU feature
# detection, kernel, or Docker and BuildKit versions. The gold standard is a
# second build on independent hardware whose hash matches the published one,
# which is what the EXPECTED check turns from an eyeballed log line into a
# pass/fail. .github/workflows/zeronym-hub-reproduce.yml runs this script on a
# native x86_64 runner for precisely that reason.
#
# Env overrides: CTX (assembled context dir), OUT (artifact dir),
# EXPECTED (override the published hash; empty string skips the comparison,
# which is only correct when deliberately re-baselining).

set -eu

ZERO_ROOT="$(git rev-parse --show-toplevel)"
HERE="$ZERO_ROOT/zeronym/hub/deploy"
CTX="${CTX:-$(mktemp -d)/zero-indexer-hub-ctx}"
OUT="${OUT:-$(dirname "$ZERO_ROOT")/zero-indexer-hub-reproduce}"

# The published hash lives in ONE machine-readable file that this script, CI and
# deploy/README.md all point at, so re-baselining the recipe is an explicit,
# reviewable edit rather than a silently stale number in prose.
if [ "${EXPECTED+set}" != "set" ]; then
	EXPECTED=$(cat "$HERE/EXPECTED_SHA256" 2>/dev/null || echo "")
fi

export DOCKER_BUILDKIT=1
export SOURCE_DATE_EPOCH=1

sh "$HERE/assemble.sh" "$CTX"
rm -rf "$OUT"
mkdir -p "$OUT"

# The recipe from inside the context (git archive HEAD), not the working tree.
RECIPE="$CTX/zeronym/hub/deploy/Containerfile"

for n in 1 2; do
	echo "==> cold build $n of 2"
	docker build -f "$RECIPE" "$CTX" \
		--platform linux/amd64 \
		--target export --no-cache \
		--output "type=local,dest=$OUT/out$n"
done

echo
echo "=== zero-indexer-hub hashes ==="
# stdin form, so the differing filenames never enter the digest text.
h1=$(sha256sum < "$OUT/out1/zero-indexer-hub" 2>/dev/null \
	|| shasum -a 256 < "$OUT/out1/zero-indexer-hub")
h2=$(sha256sum < "$OUT/out2/zero-indexer-hub" 2>/dev/null \
	|| shasum -a 256 < "$OUT/out2/zero-indexer-hub")
# Both forms print "<hex>  -"; keep only the hex.
h1=$(echo "$h1" | cut -d' ' -f1)
h2=$(echo "$h2" | cut -d' ' -f1)
echo "build 1:  $h1"
echo "build 2:  $h2"
echo "expected: ${EXPECTED:-<none recorded>}"

fail=0

if [ "$h1" = "$h2" ]; then
	echo "SELF-CONSISTENT: two cold builds on this host agree."
else
	echo "FAIL: this host disagrees with ITSELF between two cold builds."
	echo "      That is a determinism bug in the recipe, not a porting issue."
	fail=1
fi

if [ -z "$EXPECTED" ]; then
	echo "NOTE: no published hash to compare against (EXPECTED_SHA256 missing"
	echo "      or explicitly cleared). Self-consistency only."
elif [ "$h1" = "$EXPECTED" ]; then
	echo "MATCHES PUBLISHED: $EXPECTED"
else
	echo "FAIL: this host disagrees with the PUBLISHED hash."
	echo "      got      $h1"
	echo "      expected $EXPECTED"
	echo "      If the recipe or its inputs changed on purpose, re-baseline"
	echo "      deploy/EXPECTED_SHA256 and deploy/README.md together."
	fail=1
fi

echo
echo "=== vendored subtrees must be untouched ==="
# zaino/ is checked as well as zebra/, and not merely for symmetry:
# zaino-proto's build.rs regenerates its committed src/proto/*.rs whenever protoc
# is reachable, writing INTO the vendored subtree. The recipe's defence is an
# image with no protoc in it at all, and this is the assertion that the defence
# is still holding.
dirty=$(git -C "$ZERO_ROOT" status --porcelain zebra/ zaino/)
if [ -z "$dirty" ]; then
	echo "zebra/ and zaino/ clean"
else
	echo "DIRTY, this is a build failure, not a nuisance:"
	echo "$dirty"
	fail=1
fi

echo
if [ "$fail" = 0 ]; then
	echo "zero-indexer-hub: REPRODUCES"
else
	echo "zero-indexer-hub: DOES NOT REPRODUCE (see FAIL lines above)"
fi

exit $fail

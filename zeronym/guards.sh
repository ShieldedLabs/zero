#!/bin/sh
# Cheap assertions over the repository's own claims.
#
# These are not unit tests. They guard the class of defect that has actually
# shipped here twice: a published document drifting away from the code it
# describes, where nothing fails and nobody notices. Both incidents were found by
# a person reading carefully, months apart -- the published hash table naming a
# hash `EXPECTED_SHA256` had already moved past, and PCR advice that survived a
# documentation sweep because it lived in a `.sh` file rather than a `.md` one.
#
# Every check below corresponds to a finding in Taylor Hornby's 2026-08-19 review
# that is now FIXED. The point is to keep it fixed.
#
# ASSERTION vs CITATION. A bare substring search flags the corrections as well as
# the errors: a document that says "the old advice was X" contains X. So where a
# phrase is being retired, the rule is that it may appear only inside quotation
# marks -- a citation -- and never as the document's own voice.
#
# Run: sh zeronym/guards.sh   (from the repository root, or anywhere)

set -eu

cd "$(dirname "$0")"

fails=0
pass() { printf '  PASS  %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; shift; [ $# -gt 0 ] && printf '        %s\n' "$@"; fails=$((fails + 1)); }

printf '\nzeronym guards\n\n'

# ---------------------------------------------------------------- H3
# The one-command deploy must default to the attested configuration. Debug
# zeroes the attestation PCRs, opens SSH on the parent, and turns on
# per-request wallet-method logging.
if grep -q '^DEBUG=${DEBUG:-0}' deploy.sh && grep -q '^DEBUG=0' deploy.env.example; then
	pass "H3  deploy defaults to attested"
else
	fail "H3  deploy must default to DEBUG=0" \
		"deploy.sh needs DEBUG=\${DEBUG:-0} and deploy.env.example needs DEBUG=0"
fi

# ---------------------------------------------------------------- M6
# PCR2 is byte-identical across different binaries, so advice to accept it alone
# would accept an attestation that proves nothing about the code. The phrase may
# be CITED while being corrected, but never asserted.
stale=$(grep -rn "PCR2 is the check that matters" \
	--include="*.md" --include="*.sh" --include="*.rs" --include="*.tmpl" . 2>/dev/null \
	| grep -v '"PCR2 is the check that matters"' || true)
if [ -z "$stale" ]; then
	pass "M6  no unquoted 'PCR2 alone' advice"
else
	fail "M6  PCR2-alone advice asserted, not merely quoted" "$stale"
fi

# ---------------------------------------------------------------- M7
# Caution does not enforce `egress` rules at all, so no manifest may describe
# containment as a network-level property.
claim=$(grep -rn "network-level$" --include="*.tmpl" . 2>/dev/null \
	| grep -v "NOT ENFORCED" || true)
egress_ok=1
for tmpl in shim/deploy/caution/caution.hcl.tmpl hub/deploy/caution/caution.hcl.tmpl; do
	grep -q "NOT ENFORCED BY THE PLATFORM" "$tmpl" || egress_ok=0
done
if [ "$egress_ok" = 1 ]; then
	pass "M7  both manifests say the egress rules are unenforced"
else
	fail "M7  a manifest has lost its 'NOT ENFORCED BY THE PLATFORM' note" \
		"Caution reduces the egress list to 'is it empty', so the rules constrain nothing."
fi

# ---------------------------------------------------------------- M8 / hash drift
# The hash a third-party verifier reads must be the hash the build produces.
# This exact drift shipped: the table named 2009f9b3... while EXPECTED_SHA256
# had moved to 4f60e630...
for comp in hub shim; do
	expected=$(tr -d ' \n\r\t' < "$comp/deploy/EXPECTED_SHA256")
	current=$(grep -E '^\| \*\*current\*\*' "$comp/deploy/README.md" 2>/dev/null \
		| grep -oE '[0-9a-f]{64}' | head -1)
	# `grep -c` already prints 0 when nothing matches, and exits 1 while doing it.
	# `|| echo 0` therefore appends a SECOND zero and the test never matches "0".
	published=$(grep -cE '[0-9a-f]{64}' "$comp/deploy/README.md" 2>/dev/null || true)
	if [ "$published" = "0" ]; then
		# Not every component publishes a prose table. Where none exists there is
		# nothing to drift, and `EXPECTED_SHA256` is the authority -- which is what
		# the shim's table now says explicitly. Only a table that EXISTS can lie.
		pass "HASH $comp: no published table (EXPECTED_SHA256 is the only claim)"
	elif [ -z "$current" ]; then
		fail "HASH $comp: deploy/README.md publishes hashes but has no '**current**' row" \
			"A reader cannot tell which of them is the deployed one."
	elif [ "$expected" = "$current" ]; then
		pass "HASH $comp: published table matches EXPECTED_SHA256"
	else
		fail "HASH $comp: published table disagrees with EXPECTED_SHA256" \
			"EXPECTED_SHA256: $expected" \
			"README current:  $current" \
			"A verifier reads the table. If the two disagree, believe the file."
	fi
done

# ---------------------------------------------------------------- M4
# The auditor recipe must name the checks that exist and require all three PCRs.
recipe_ok=1
grep -q "caution verify" README.md || recipe_ok=0
grep -qi "all three PCRs" README.md || recipe_ok=0
grep -qi "dig\b\|Resolve the domain" README.md || recipe_ok=0
if [ "$recipe_ok" = 1 ]; then
	pass "M4  auditor recipe names caution verify, all three PCRs, and the DNS step"
else
	fail "M4  auditor recipe is missing a step" \
		"It must name 'caution verify', require all three PCRs, and check the network path."
fi

# ---------------------------------------------------------------- M31
# "Not protected" must state the consequence, not just name the endpoints.
if grep -q "txid \*\*and its value\*\*" README.md; then
	pass "M31 README states what the operator can actually recover"
else
	fail "M31 README's 'Not protected' no longer says the operator can recover the value" \
		"Naming the endpoints without the consequence is the overclaim that was corrected."
fi

# ---------------------------------------------------------------- M26
# The only automated gate on the attested binaries must run on push, because
# this branch takes direct pushes.
for comp in hub shim; do
	wf="../.github/workflows/zeronym-$comp-reproduce.yml"
	if grep -qE '^  push:' "$wf"; then
		pass "M26 $comp reproduce gate runs on push"
	else
		fail "M26 $comp reproduce gate does not trigger on push" \
			"Gating only on pull_request means it never runs on a commit that publishes a hash."
	fi
done

# ---------------------------------------------------------------- M14
# The example config must not send the wallet's txid to the very indexer the
# interception exists to hide it from.
backend=$(grep -E '^BACKEND=' deploy.env.example | cut -d= -f2 | awk '{print $1}')
indexers=$(grep -E '^INDEXERS=' deploy.env.example | cut -d= -f2 | awk '{print $1}')
if [ "$backend" = "$indexers" ]; then
	fail "M14 deploy.env.example points BACKEND and INDEXERS at the same host" \
		"BACKEND=$backend  INDEXERS=$indexers" \
		"The hub's lookup fall-through then hands the txid to the operator's own indexer."
else
	pass "M14 example config separates the backend from the hub's indexers"
fi

printf '\n'
if [ "$fails" -eq 0 ]; then
	printf 'all guards passed\n\n'
else
	printf '%s guard(s) FAILED\n\n' "$fails"
	exit 1
fi

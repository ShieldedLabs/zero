#!/usr/bin/env bash
# Release gates. release.yml runs this before it tags. Run it locally before
# you cut the changelog, so a bad zebra version or halt height fails early.
#
# Usage: .github/scripts/release-preflight.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0

# Refuse to release a pre-release zebra.
# Zebra's end-of-support check gives release candidates a 10-day fuse
# (EOS_PANIC_AFTER in zebrad/src/components/sync/end_of_support.rs):
# every image built from an -rc/-alpha/-beta zebra subtree stops
# starting when mainnet passes the fuse height. Releases v6-v8 shipped
# a 6.0.0-rc.0 zebra and all expired simultaneously at height
# 3,410,520 (2026-07-13), taking operator deployments down. Only
# release from a final zebra version.
zebra_version=$(sed -n 's/^version = "\(.*\)"/\1/p' zebra/zebrad/Cargo.toml | head -1)
echo "zebra/zebrad version: ${zebra_version}"
if [[ "$zebra_version" == *-* ]]; then
  echo "::error::zebra/zebrad is a pre-release (${zebra_version});" \
    "its end-of-support fuse is days, not months. Update the zebra" \
    "subtree to a final release before cutting a Zero release."
  fail=1
fi

# Refuse to release a node that halts soon.
# Both nodes auto-halt on mainnet at a hand-edited height: zcashd at
# APPROX_RELEASE_HEIGHT + RELEASE_TO_DEPRECATION_WEEKS of blocks
# (zcashd/src/deprecation.h), zebra at ESTIMATED_RELEASE_HEIGHT +
# EOS_PANIC_AFTER days of blocks (zebrad/src/components/sync/
# end_of_support.rs, a [zero] carry of the newest upstream release's
# value). v20 through v27 all shipped v20's zcashd value and shared one
# halt block (3,471,448, ~2026-09-04), caught two days ahead. Fail when
# either halt is under 4 weeks of blocks past the mainnet tip. The tip
# is estimated from a fixed anchor and the wall clock, not fetched:
# DigiShield holds the long-run average at the 75-second target
# (observed +0.1% over the 48 days before the anchor), so the estimate
# drifts by hours over months and the anchor never needs refreshing
# for the check to stay correct.
#
# Mainnet block 3,469,914 carried timestamp 2026-09-03T00:35:44Z.
ANCHOR_HEIGHT=3469914
ANCHOR_EPOCH=1788395744
zcashd_h=zcashd/src/deprecation.h
zebra_rs=zebra/zebrad/src/components/sync/end_of_support.rs
zcashd_release=$(sed -n 's/^static const int APPROX_RELEASE_HEIGHT = \([0-9]*\);.*/\1/p' "$zcashd_h")
zcashd_weeks=$(sed -n 's/^static const int RELEASE_TO_DEPRECATION_WEEKS = \([0-9]*\);.*/\1/p' "$zcashd_h")
zebra_release=$(sed -n 's/^pub const ESTIMATED_RELEASE_HEIGHT: u32 = \([0-9_]*\);.*/\1/p' "$zebra_rs" | tr -d _)
zebra_days=$(sed -n 's/^pub const EOS_PANIC_AFTER: u32 = \([0-9]*\);.*/\1/p' "$zebra_rs")
if ! [[ "$zcashd_release" =~ ^[0-9]{1,9}$ && "$zcashd_weeks" =~ ^[0-9]{1,2}$ ]]; then
  echo "::error::Could not parse APPROX_RELEASE_HEIGHT and RELEASE_TO_DEPRECATION_WEEKS from ${zcashd_h}"
  exit 1
fi
if ! [[ "$zebra_release" =~ ^[0-9]{1,9}$ && "$zebra_days" =~ ^[0-9]{1,3}$ ]]; then
  echo "::error::Could not parse ESTIMATED_RELEASE_HEIGHT and EOS_PANIC_AFTER from ${zebra_rs}"
  exit 1
fi
blocks_per_day=$(( 24 * 60 * 60 / 75 ))   # post-Blossom block spacing
zcashd_halt=$(( zcashd_release + zcashd_weeks * 7 * blocks_per_day ))
zebra_halt=$(( zebra_release + zebra_days * blocks_per_day ))
min_runway=$(( 4 * 7 * blocks_per_day ))
now=$(date +%s)
if (( now < ANCHOR_EPOCH )); then
  echo "::error::ANCHOR_EPOCH (${ANCHOR_EPOCH}) is in the future relative to the" \
    "runner clock (${now}); the tip estimate would be invalid."
  exit 1
fi
tip=$(( ANCHOR_HEIGHT + ( ( now - ANCHOR_EPOCH ) + 74 ) / 75 ))
echo "estimated mainnet tip ${tip}; minimum runway ${min_runway} blocks"
echo "zcashd halts at ${zcashd_halt}: $(( zcashd_halt - tip )) blocks of runway"
echo "zebra halts at ${zebra_halt}: $(( zebra_halt - tip )) blocks of runway"
if (( zcashd_halt - tip < min_runway )); then
  echo "::error::zcashd halts at ${zcashd_halt}, under 4 weeks past the estimated mainnet" \
    "tip (${tip}). Bump APPROX_RELEASE_HEIGHT in ${zcashd_h} (RELEASING.md," \
    "Pre-release checklist), push, then re-dispatch."
  fail=1
fi
if (( zebra_halt - tip < min_runway )); then
  echo "::error::zebra halts at ${zebra_halt}, under 4 weeks past the estimated mainnet" \
    "tip (${tip}). Re-set ESTIMATED_RELEASE_HEIGHT in ${zebra_rs} to the newest" \
    "upstream zebra release's value, or pull that tag (RELEASING.md," \
    "Pre-release checklist), push, then re-dispatch."
  fail=1
fi
exit "$fail"

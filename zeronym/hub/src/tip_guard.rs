//! Which mainnet heights are plausible, and how fast the chain can climb.
//!
//! The hub learns the tip from its indexers and cannot check a height itself:
//! `CompactTxStreamer` serves no headers to verify work against. So a height is
//! judged against the only witnesses the enclave has that an indexer does not
//! control: a compiled anchor and the enclave's own clocks. Both bounds here
//! are mainnet-only. Testnet's minimum-difficulty rule lets blocks arrive in
//! bursts these bounds would refuse.
//!
//! The bounds limit how far one lying indexer can move the hub; they cannot
//! remove that lever while one operator supplies every answer. `batcher.rs`
//! says what is left.

// See queue.rs: a panic here is a fleet-wide privacy event, not a crash.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A known mainnet block and its time: the same anchor `release.yml` uses for
/// its EOL runway check. DigiShield held the long-run average at the 75-second
/// target to +0.1% over the 48 days before it, so it ages slowly; a fresher
/// one tightens [`mainnet_band`].
pub const ANCHOR_HEIGHT: u32 = 3_469_914;
pub const ANCHOR_EPOCH: u64 = 1_788_395_744;

/// Post-Blossom target block spacing.
pub const TARGET_BLOCK_SECS: u64 = 75;

/// Standard deviations of a Poisson block count the bounds allow. Five puts a
/// false refusal near 3e-7 per check.
const SIGMA: f64 = 5.0;

/// Seconds since the Unix epoch, or `None` for a clock set before it.
pub fn unix_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

/// The inclusive range of heights mainnet can plausibly be at, at `unix_secs`.
/// `None` before the anchor: a clock that early is wrong, and nothing can be
/// judged against it.
///
/// Centred on the anchor plus one block per 75 s. The slack is a SIGMA Poisson
/// walk since the anchor, twice the drift observed before it (0.2%), and 200
/// blocks for propagation and short halts.
pub fn mainnet_band(unix_secs: u64) -> Option<(u32, u32)> {
    let blocks = unix_secs.checked_sub(ANCHOR_EPOCH)? / TARGET_BLOCK_SECS;
    let slack = (SIGMA * (blocks as f64).sqrt()) as u64 + blocks / 500 + 200;
    let estimate = u64::from(ANCHOR_HEIGHT) + blocks;
    let clamp = |height: u64| u32::try_from(height).unwrap_or(u32::MAX);
    Some((
        clamp(estimate.saturating_sub(slack)),
        clamp(estimate.saturating_add(slack)),
    ))
}

/// The most blocks mainnet plausibly produces in `elapsed`: the Poisson mean
/// plus SIGMA standard deviations, plus three so a short window still allows a
/// small burst. 30 s allows 6, 25 minutes 45, 24 hours 1324.
pub fn max_new_blocks(elapsed: Duration) -> u32 {
    let mean = elapsed.as_secs_f64() / TARGET_BLOCK_SECS as f64;
    // `as` saturates a float into the integer range.
    (mean + SIGMA * mean.sqrt() + 3.0) as u32
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn the_band_at_the_anchor_is_the_fixed_slack_around_it() {
        assert_eq!(
            mainnet_band(ANCHOR_EPOCH),
            Some((ANCHOR_HEIGHT - 200, ANCHOR_HEIGHT + 200))
        );
    }

    #[test]
    fn a_clock_before_the_anchor_has_no_band() {
        assert_eq!(mainnet_band(ANCHOR_EPOCH - 1), None);
        assert_eq!(mainnet_band(0), None);
    }

    #[test]
    fn the_band_widens_with_age_and_tracks_the_target_rate() {
        // Sixty days on: 69,120 blocks, slack 1314 + 138 + 200.
        let (low, high) = mainnet_band(ANCHOR_EPOCH + 60 * DAY).unwrap();
        let estimate = ANCHOR_HEIGHT + 69_120;
        assert_eq!((low, high), (estimate - 1652, estimate + 1652));
    }

    #[test]
    fn the_band_never_reaches_the_heights_that_brick_the_hub() {
        // A century on, the ceiling is still nowhere near u32::MAX, so a height
        // that saturates the flush arithmetic is always refused.
        let (_, high) = mainnet_band(ANCHOR_EPOCH + 100 * 365 * DAY).unwrap();
        assert!(high < 100_000_000, "ceiling {high}");
    }

    #[test]
    fn max_new_blocks_at_the_windows_the_tracker_uses() {
        assert_eq!(max_new_blocks(Duration::ZERO), 3);
        assert_eq!(max_new_blocks(Duration::from_secs(30)), 6);
        assert_eq!(max_new_blocks(Duration::from_secs(25 * 60)), 45);
        assert_eq!(max_new_blocks(Duration::from_secs(DAY)), 1324);
    }

    #[test]
    fn max_new_blocks_never_shrinks_as_the_window_grows() {
        let mut previous = 0;
        for secs in (0..=2 * DAY).step_by(15) {
            let bound = max_new_blocks(Duration::from_secs(secs));
            assert!(bound >= previous, "{bound} < {previous} at {secs} s");
            previous = bound;
        }
    }

    #[test]
    fn max_new_blocks_stays_above_the_mean_rate() {
        // Every window allows at least the blocks the target rate produces in
        // it, so an honest chain on schedule always passes.
        for secs in (0..=2 * DAY).step_by(75) {
            let mean = (secs / TARGET_BLOCK_SECS) as u32;
            assert!(max_new_blocks(Duration::from_secs(secs)) > mean);
        }
    }
}

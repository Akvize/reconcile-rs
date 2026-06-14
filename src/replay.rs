// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-peer replay protection for the authenticated protocol modes.
//!
//! When a cluster key is configured (MAC or encrypted mode), every outgoing datagram carries a
//! 16-byte **replay header** — a monotonically increasing `u64` sequence number followed by a
//! `u64` sender wall-clock stamp (milliseconds since the Unix epoch, little-endian) — that is
//! included in the authenticated portion of the datagram. The receiver maintains per-peer state
//! to reject:
//!
//! - **Duplicate or stale datagrams**: a sequence number within the backward window that has
//!   already been seen (sliding-bitmap check), or a sequence number older than the window.
//! - **Stale freshness stamps**: a sender wall-clock stamp that deviates from the receiver's
//!   physical clock by more than [`FRESHNESS_WINDOW_DEFAULT`] in either direction.
//!
//! Only datagrams that carry an authenticated replay header are checked. Unauthenticated mode
//! (no cluster key) is explicitly exempt — and documented as unprotected.
//!
//! # Replay state lifetime and staleness purge
//!
//! Per-peer replay state **intentionally outlives membership**. Decommissioning a peer removes it
//! from `members` and `peers` so it no longer gates tombstone garbage collection or receives gossip
//! probes, but the replay filter entry is kept. This matters because an adversary who captured a
//! datagram from the peer while it was active can replay it within the freshness window after
//! decommission: the replayed datagram passes MAC verification and the freshness check, but hits
//! the bitmap duplicate check and is rejected. Without this, decommission + replay would re-add
//! the peer to `members`, re-poisoning causal-stability membership — which is exactly what this
//! design exists to prevent.
//!
//! **Staleness purge is sound**: the purge retains only entries where
//! `now - stamp_at_max <= window`. Any replayable datagram from a peer has a stamp no greater than
//! `stamp_at_max` (the bitmap path never raises it; a strictly newer stamp triggers `reset` instead
//! of replay). Once `now - stamp_at_max > window`, the freshness check on any such stamp would fail
//! even accounting for the maximum allowed sender clock skew, so no replay can pass. The entry is
//! therefore safe to drop at that point. The purge runs opportunistically on every call to
//! [`ReplayFilter::check_and_record`] via `HashMap::retain`, which is cheap for the small maps
//! typical in a gossip cluster.
//!
//! # Sequence-number regression
//!
//! A sender's sequence counter is per-process lifetime, so a restart resets it to 1. The receiver
//! distinguishes a restart from a replay with the freshness stamp. The rule applies on **any**
//! sequence regression (`seq <= max_seq`):
//!
//! - The stamp is checked **first**: if `stamp > stamp_at_max`, the regression is treated as a
//!   legitimate restart — per-peer state is reset and the datagram is accepted.
//! - Otherwise, the bitmap check runs normally: if the sequence is within the out-of-order window
//!   and has not been seen, it is accepted; otherwise it is rejected.
//! - If the sequence is beyond the backward window and the stamp is not strictly newer, the
//!   datagram is unconditionally rejected.
//!
//! **Residual**: a restart that happens within the same millisecond as the sender's last
//! pre-restart send produces `stamp == stamp_at_max`, which does not satisfy `stamp > stamp_at_max`.
//! Such a restart is indistinguishable from a replay within that millisecond and is dropped. This
//! is a deliberate trade-off: the alternative (accepting same-stamp regressions) would allow
//! replays within the same millisecond.
//!
//! # Post-restart tail guard (monotone max-stamp)
//!
//! After a genuine restart triggers `reset()` to a low `max_seq` with a cleared bitmap, captured
//! pre-restart datagrams with high sequence numbers (but stamps still inside the freshness window)
//! would otherwise be re-accepted on the forward path (`seq > max_seq`). To prevent this, each
//! `PeerState` tracks `max_stamp_seen`: the maximum sender stamp ever accepted from this peer.
//! `reset()` does **not** rewind `max_stamp_seen`. On the forward path (`seq > max_seq`), any
//! datagram whose stamp is strictly less than `max_stamp_seen` is rejected.
//!
//! The monotonicity premise is enforced at the source: [`SenderCounter::next_stamp`] maintains an
//! in-process floor (`AtomicU64`) so that each minted stamp is `max(wall_clock_now, floor)`.
//! Stamps therefore never decrease within a process lifetime, and same-millisecond bursts share a
//! stamp — hence the guard uses strict `<` (not `<=`) to accept those bursts correctly.
//!
//! **Residual**: a sender that *restarts* while its wall clock is still behind its pre-restart
//! stamps (e.g. after an NTP backward step or VM resume that shrinks wall time) loses the
//! in-memory floor on restart and re-mints stamps lower than the receiver's recorded
//! `max_stamp_seen`. Such datagrams are treated as replays and silently dropped until the sender's
//! clock advances past the old high-water mark — bounded by the size of the clock step. This is the
//! same family of trade-off as the documented same-millisecond-restart residual.
//!
//! # Wire layout (authenticated portion only)
//!
//! ```text
//! seq   (8 bytes, little-endian u64)
//! stamp (8 bytes, little-endian u64, milliseconds since Unix epoch)
//! <protocol messages ...>
//! ```
//!
//! This 16-byte header is the **first thing inside** the authenticated or encrypted region, before
//! any protocol messages. For MAC mode the tag still authenticates the whole payload including the
//! header; for encryption mode the header is encrypted together with the messages.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;

/// Length of the replay header prepended to the authenticated portion of every datagram.
///
/// `seq (8 bytes) || stamp (8 bytes)`.
pub(crate) const REPLAY_HEADER_LEN: usize = 16;

/// Default freshness window: datagrams whose sender wall-clock stamp deviates from local physical
/// time by more than this value in either direction are rejected.
pub const FRESHNESS_WINDOW_DEFAULT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Number of bits in the out-of-order acceptance bitmap.
///
/// A bitmap of 1024 entries allows sequence numbers up to 1024 behind the highest accepted to be
/// accepted as out-of-order legitimate UDP reordering. Each bit represents whether the
/// corresponding sequence number (relative to `max_seq`) was received. Anything older than 1024
/// behind `max_seq` is unconditionally rejected as outside the window.
const WINDOW_SIZE: u64 = 1024;

/// The per-peer replay state.
///
/// Tracks the highest sequence number seen from a peer, the sender stamp at which that maximum was
/// accepted, a sliding bitmap for the out-of-order acceptance window, and a monotone high-water
/// mark of sender stamps (used for the post-restart tail guard).
struct PeerState {
    /// Highest sequence number accepted from this peer.
    max_seq: u64,
    /// Sender wall-clock stamp (ms since epoch) that was present on the datagram carrying `max_seq`.
    /// Used for restart detection: if a new datagram has a higher stamp and a lower seq, the peer
    /// has restarted.
    stamp_at_max: u64,
    /// Monotonically non-decreasing high-water mark of all sender stamps ever accepted from this
    /// peer. Never reset by `reset()`. Used on the forward path to reject captured pre-restart
    /// datagrams whose stamp predates the restart stamp.
    max_stamp_seen: u64,
    /// Sliding bitmap. Bit `i` represents whether sequence number `max_seq - i` was already
    /// accepted. `bitmap & 1` is always `1` (max_seq itself was accepted). Bits beyond
    /// `WINDOW_SIZE` are not tracked (always treated as seen/rejected if below window).
    ///
    /// Stored as a fixed-size array of `u64` words; `WINDOW_SIZE / 64 = 16`.
    bitmap: [u64; (WINDOW_SIZE / 64) as usize],
}

impl PeerState {
    fn new(first_seq: u64, first_stamp: u64) -> Self {
        let mut bitmap = [0u64; (WINDOW_SIZE / 64) as usize];
        // Mark the first sequence as seen (bit 0 of word 0).
        bitmap[0] = 1;
        PeerState {
            max_seq: first_seq,
            stamp_at_max: first_stamp,
            max_stamp_seen: first_stamp,
            bitmap,
        }
    }

    /// Return the sender stamp at the highest accepted sequence number (used for staleness purge).
    fn stamp_at_max(&self) -> u64 {
        self.stamp_at_max
    }

    /// Attempt to accept a datagram with the given `seq` and `stamp`.
    ///
    /// Returns `true` if the datagram is fresh (not a replay), `false` if it should be rejected.
    ///
    /// Side effect on success: updates the bitmap and `max_seq`/`stamp_at_max` as appropriate.
    fn accept(&mut self, seq: u64, stamp: u64) -> bool {
        if seq > self.max_seq {
            // Forward path: new high-water sequence.
            // Post-restart tail guard: reject pre-restart captured datagrams. A genuinely
            // later-minted datagram always has stamp >= every prior datagram (sender mints
            // monotonically). Same-millisecond bursts share a stamp, so guard uses strict <.
            if stamp < self.max_stamp_seen {
                return false;
            }
            // Advance the window. Shift the bitmap forward by (seq - max_seq) positions.
            let delta = seq - self.max_seq;
            self.shift_bitmap(delta);
            self.max_seq = seq;
            self.stamp_at_max = stamp;
            self.max_stamp_seen = self.max_stamp_seen.max(stamp);
            // Mark the new max as seen (bit 0).
            self.bitmap[0] |= 1;
            true
        } else {
            // seq <= max_seq: check stamp FIRST for restart detection.
            // If the sender's stamp is strictly newer than what we recorded at max_seq,
            // this is a restart — reset state regardless of how far behind seq is.
            if stamp > self.stamp_at_max {
                self.reset(seq, stamp);
                return true;
            }
            // Not a restart: fall through to bitmap / window check.
            let behind = self.max_seq - seq;
            if behind >= WINDOW_SIZE {
                // Outside the window: unconditionally reject.
                return false;
            }
            let word = (behind / 64) as usize;
            let bit = behind % 64;
            if self.bitmap[word] & (1 << bit) != 0 {
                // Already seen: duplicate.
                return false;
            }
            // First time in window: accept and mark.
            self.bitmap[word] |= 1 << bit;
            true
        }
    }

    /// Shift the bitmap forward by `delta` positions (oldest bits fall off).
    ///
    /// A shift of 1 means `max_seq` moved up by 1; the former bit 0 becomes bit 1, etc.
    /// Bits that shift past position `WINDOW_SIZE - 1` are discarded.
    fn shift_bitmap(&mut self, delta: u64) {
        if delta >= WINDOW_SIZE {
            // The whole bitmap falls off — start fresh.
            self.bitmap = [0u64; (WINDOW_SIZE / 64) as usize];
            return;
        }
        let word_shift = (delta / 64) as usize;
        let bit_shift = (delta % 64) as u32;
        let words = (WINDOW_SIZE / 64) as usize;

        if bit_shift == 0 {
            // Whole-word shift only.
            for i in (0..words).rev() {
                self.bitmap[i] = if i >= word_shift {
                    self.bitmap[i - word_shift]
                } else {
                    0
                };
            }
        } else {
            // Combined word + bit shift.
            for i in (0..words).rev() {
                let lo = if i >= word_shift {
                    self.bitmap[i - word_shift] << bit_shift
                } else {
                    0
                };
                let hi = if i > word_shift {
                    self.bitmap[i - word_shift - 1] >> (64 - bit_shift)
                } else {
                    0
                };
                self.bitmap[i] = lo | hi;
            }
        }
    }

    /// Reset state for a restarted sender.
    ///
    /// `max_stamp_seen` is intentionally NOT reset — it is a monotone high-water mark that
    /// persists across restarts to guard against replays of captured pre-restart datagrams.
    fn reset(&mut self, new_seq: u64, new_stamp: u64) {
        self.max_seq = new_seq;
        self.stamp_at_max = new_stamp;
        // max_stamp_seen is never rewound — keep the monotone high-water mark.
        self.max_stamp_seen = self.max_stamp_seen.max(new_stamp);
        self.bitmap = [0u64; (WINDOW_SIZE / 64) as usize];
        self.bitmap[0] = 1;
    }
}

/// Read the local physical time as milliseconds since the Unix epoch.
fn phys_now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// Sender-side replay state: a monotonically increasing sequence counter and stamp floor.
///
/// One instance per node, incremented for every datagram sent in an authenticated mode.
/// The sequence counter starts at 1; 0 is reserved as "no sequence" (unauthenticated mode).
///
/// The stamp floor (`stamp_floor`) guarantees that minted sender stamps never decrease within a
/// process lifetime. Each call to [`next_stamp`](Self::next_stamp) returns
/// `max(Utc::now().timestamp_millis(), previous_stamp)`. Concurrent senders sharing a
/// millisecond will emit equal stamps; the receiver's forward-path guard uses strict `<` precisely
/// to tolerate same-millisecond bursts.
///
/// **Residual**: a sender that *restarts* while its wall clock is still behind its pre-restart
/// stamps (e.g. after an NTP correction or VM resume that shrinks wall time) re-mints lower stamps
/// because the in-memory floor is lost on restart. The receiver treats the regressed stamps as a
/// replayer until the sender's clock catches back up — bounded by the clock-step size. This is the
/// same family of trade-off as the documented same-millisecond-restart residual.
pub(crate) struct SenderCounter {
    seq: AtomicU64,
    stamp_floor: AtomicU64,
}

impl SenderCounter {
    pub(crate) fn new() -> Self {
        SenderCounter {
            seq: AtomicU64::new(1),
            stamp_floor: AtomicU64::new(0),
        }
    }

    /// Allocate the next sequence number (strictly increasing).
    pub(crate) fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Mint a monotonically non-decreasing sender stamp (milliseconds since Unix epoch).
    ///
    /// Each call returns `max(Utc::now().timestamp_millis().max(0), floor)` and advances the
    /// internal floor so subsequent calls never return a smaller value.
    pub(crate) fn next_stamp(&self) -> u64 {
        self.next_stamp_at(phys_now_ms())
    }

    /// Inner implementation with an injectable `now_ms` for unit-testing backward clock steps.
    pub(crate) fn next_stamp_at(&self, now_ms: u64) -> u64 {
        // `fetch_max` atomically sets floor = max(floor, now_ms) and returns the OLD value.
        // The stamp we return is the maximum of the returned old floor and now_ms, which equals
        // the new floor value after the update.
        let prev = self.stamp_floor.fetch_max(now_ms, Ordering::Relaxed);
        prev.max(now_ms)
    }
}

/// Receiver-side per-peer replay filter.
///
/// Maintains per-peer [`PeerState`] and enforces the freshness window. Entries are purged
/// opportunistically once `now - stamp_at_max > window`: at that point any replayable datagram
/// (stamp ≤ `stamp_at_max`) would fail the freshness check, so no replay can pass and the entry is
/// safe to drop, reclaiming memory automatically.
pub(crate) struct ReplayFilter {
    peers: Mutex<HashMap<IpAddr, PeerState>>,
    freshness_window: Duration,
}

impl ReplayFilter {
    pub(crate) fn new(freshness_window: Duration) -> Self {
        ReplayFilter {
            peers: Mutex::new(HashMap::new()),
            freshness_window,
        }
    }

    /// Decide whether a datagram from `sender` with the given `seq` and `stamp` should be accepted.
    ///
    /// Returns `true` when the datagram is fresh and unique — the caller may proceed to process it.
    /// Returns `false` when the datagram is a replay (duplicate, too old within the window, or
    /// outside the freshness window) — the caller should drop it silently.
    pub(crate) fn check_and_record(&self, sender: IpAddr, seq: u64, stamp: u64) -> bool {
        self.check_and_record_at(sender, seq, stamp, phys_now_ms())
    }

    /// Inner implementation with an injectable `now_ms` (milliseconds since epoch).
    ///
    /// Separating the clock source allows unit tests to exercise time-dependent logic
    /// (staleness purge, skew-positive scenarios) without sleeping.
    fn check_and_record_at(&self, sender: IpAddr, seq: u64, stamp: u64, now: u64) -> bool {
        // Freshness window check: reject stamps that are too far in the past or future.
        let window_ms = self.freshness_window.as_millis() as u64;
        if now.saturating_sub(stamp) > window_ms || stamp.saturating_sub(now) > window_ms {
            return false;
        }

        let mut map = self.peers.lock();

        // Opportunistic staleness purge: entries whose stamp_at_max is older than the freshness
        // window cannot produce any accepted datagrams (any replayable stamp ≤ stamp_at_max would
        // fail the freshness check above), so it is safe to drop them.
        map.retain(|_, s| now.saturating_sub(s.stamp_at_max()) <= window_ms);

        match map.get_mut(&sender) {
            None => {
                // First datagram from this sender.
                map.insert(sender, PeerState::new(seq, stamp));
                true
            }
            Some(state) => state.accept(seq, stamp),
        }
    }

    /// Number of peers currently tracked by the filter.
    ///
    /// Exposed for test assertions under the `reconcile_internal_testing` cfg gate.
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn len(&self) -> usize {
        self.peers.lock().len()
    }

    /// Remove the replay state for a peer, freeing its memory immediately.
    ///
    /// This is an explicit escape hatch for tests. Production code does not call this — per-peer
    /// state is reclaimed automatically by the staleness purge in [`check_and_record`].
    #[cfg(test)]
    pub(crate) fn evict(&self, peer: IpAddr) {
        self.peers.lock().remove(&peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sender counter ────────────────────────────────────────────────────────

    #[test]
    fn sender_counter_starts_at_1_and_increments() {
        let c = SenderCounter::new();
        assert_eq!(c.next_seq(), 1);
        assert_eq!(c.next_seq(), 2);
        assert_eq!(c.next_seq(), 3);
    }

    /// The stamp mint must be monotonically non-decreasing even when the wall clock steps backward.
    ///
    /// Uses the injectable `next_stamp_at` to simulate a backward clock step without sleeping.
    #[test]
    fn stamp_mint_is_monotonic_across_backward_clock_steps() {
        let c = SenderCounter::new();

        // Feed a normal increasing sequence of wall-clock values.
        let t1 = 1_700_000_000_000_u64;
        let t2 = t1 + 50;
        let t3 = t2 + 50;

        assert_eq!(c.next_stamp_at(t1), t1, "first stamp equals wall clock");
        assert_eq!(
            c.next_stamp_at(t2),
            t2,
            "second stamp equals advancing wall clock"
        );
        assert_eq!(
            c.next_stamp_at(t3),
            t3,
            "third stamp equals advancing wall clock"
        );

        // Simulate a backward step: wall clock goes back 500 ms (e.g. NTP correction).
        let t_regressed = t3 - 500;
        let s_after_regression = c.next_stamp_at(t_regressed);
        assert_eq!(
            s_after_regression, t3,
            "stamp after backward step must equal the floor (t3), not the regressed wall clock"
        );
        assert!(
            s_after_regression >= t3,
            "stamp must not decrease after backward clock step"
        );

        // Verify the floor is sticky: a second regressed call still yields t3.
        assert_eq!(
            c.next_stamp_at(t_regressed),
            t3,
            "floor remains at t3 for further backward clock values"
        );

        // Once the wall clock catches back up past the floor, stamps advance again.
        let t_recovered = t3 + 1;
        assert_eq!(
            c.next_stamp_at(t_recovered),
            t_recovered,
            "stamps advance normally once wall clock passes the floor"
        );
    }

    // ── PeerState bitmap ──────────────────────────────────────────────────────

    #[test]
    fn bitmap_shift_by_1() {
        let mut state = PeerState::new(10, 1000);
        // max_seq = 10, bit 0 set
        assert!(!state.accept(10, 1000)); // already seen
                                          // out of order: seq 9
        assert!(state.accept(9, 999));
        // seq 9 accepted, now bitmap has bit 0 (=10) and bit 1 (=9) set
        assert!(!state.accept(9, 999)); // already seen
    }

    #[test]
    fn bitmap_advance_and_reuse() {
        let mut state = PeerState::new(10, 1000);
        // advance max_seq to 20
        assert!(state.accept(20, 1000));
        // seq 10 was already accepted when state was created: must be rejected as duplicate
        assert!(!state.accept(10, 1000));
        // seq 15 (mid-window, never seen before) must be accepted
        assert!(state.accept(15, 1000));
        // seq 15 again: rejected
        assert!(!state.accept(15, 1000));
    }

    #[test]
    fn bitmap_outside_window_is_rejected() {
        let mut state = PeerState::new(1024, 1000);
        // advance max to 2048; seq 1 is 2047 behind, outside window
        assert!(state.accept(2048, 1000));
        assert!(!state.accept(1, 1000));
    }

    #[test]
    fn bitmap_large_jump_clears_window() {
        let mut state = PeerState::new(10, 1000);
        // Jump so big the whole window rolls over
        assert!(state.accept(10 + WINDOW_SIZE + 5, 1000));
        // seq 10 is now (WINDOW_SIZE + 5) behind, outside the window
        assert!(!state.accept(10, 1000));
    }

    #[test]
    fn in_order_sequence_all_accepted_once() {
        let mut state = PeerState::new(1, 1000);
        for seq in 2..=100 {
            assert!(state.accept(seq, 1000), "seq {seq} should be accepted");
        }
        // All previously accepted seqs rejected on replay
        for seq in 1..=100 {
            assert!(
                !state.accept(seq, 1000),
                "seq {seq} should be rejected as duplicate"
            );
        }
    }

    // ── ReplayFilter freshness check ──────────────────────────────────────────

    fn filter_5min() -> ReplayFilter {
        ReplayFilter::new(FRESHNESS_WINDOW_DEFAULT)
    }

    /// Helper: call check_and_record_at with a caller-supplied `now`.
    fn check_at(filter: &ReplayFilter, peer: IpAddr, seq: u64, stamp: u64, now: u64) -> bool {
        filter.check_and_record_at(peer, seq, stamp, now)
    }

    #[test]
    fn fresh_datagram_accepted() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let now = phys_now_ms();
        assert!(filter.check_and_record(peer, 1, now));
    }

    #[test]
    fn replay_of_same_datagram_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.2".parse().unwrap();
        let now = phys_now_ms();
        assert!(filter.check_and_record(peer, 1, now));
        assert!(!filter.check_and_record(peer, 1, now));
    }

    #[test]
    fn stale_stamp_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.3".parse().unwrap();
        // Stamp 10 minutes in the past — beyond the 5-minute window.
        let old_stamp = phys_now_ms().saturating_sub(10 * 60 * 1000 + 1);
        assert!(!filter.check_and_record(peer, 1, old_stamp));
    }

    #[test]
    fn far_future_stamp_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.4".parse().unwrap();
        // Stamp 10 minutes in the future.
        let future_stamp = phys_now_ms() + 10 * 60 * 1000 + 1;
        assert!(!filter.check_and_record(peer, 1, future_stamp));
    }

    #[test]
    fn out_of_order_within_window_accepted_once() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.5".parse().unwrap();
        let now = phys_now_ms();
        // Accept seq 5 first, then 3 (out of order but within window).
        assert!(filter.check_and_record(peer, 5, now));
        assert!(filter.check_and_record(peer, 3, now));
        // Both must be rejected on replay.
        assert!(!filter.check_and_record(peer, 5, now));
        assert!(!filter.check_and_record(peer, 3, now));
    }

    #[test]
    fn seq_regression_outside_window_with_newer_stamp_accepted_as_restart() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.6".parse().unwrap();
        let now = phys_now_ms();
        // Advance to a seq well beyond WINDOW_SIZE so a restart lands outside the bitmap.
        let high_seq = WINDOW_SIZE + 100;
        assert!(filter.check_and_record(peer, high_seq, now));
        // Simulated restart: seq resets to 1 (outside the backward window) with a newer stamp.
        let newer = now + 1000;
        assert!(
            filter.check_and_record(peer, 1, newer),
            "a seq regression outside the window with a strictly newer stamp must be accepted as a restart"
        );
        // After reset the new state has max_seq=1, stamp_at_max=newer.
        // A further seq=2 with the new stamp must be accepted normally.
        assert!(filter.check_and_record(peer, 2, newer));
        // Replaying seq=1 with the new stamp is rejected (already seen after restart).
        assert!(!filter.check_and_record(peer, 1, newer));
    }

    #[test]
    fn seq_regression_outside_window_with_old_stamp_rejected_as_replay() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.7".parse().unwrap();
        let now = phys_now_ms();
        let high_seq = WINDOW_SIZE + 100;
        assert!(filter.check_and_record(peer, high_seq, now));
        // Lower seq outside the window with the SAME stamp: not a restart, must be rejected.
        assert!(
            !filter.check_and_record(peer, 1, now),
            "seq regression outside the window with same stamp must be rejected as replay"
        );
    }

    #[test]
    fn evict_clears_state_and_allows_fresh_start() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.8".parse().unwrap();
        let now = phys_now_ms();
        assert!(filter.check_and_record(peer, 10, now));
        // After eviction the peer state is gone; any new datagram is accepted.
        filter.evict(peer);
        assert!(filter.check_and_record(peer, 1, now));
    }

    /// Test 3 (updated): staleness purge uses sender `stamp_at_max`, not receiver clock.
    ///
    /// A peer is purged once `now - stamp_at_max > window`, regardless of when the receiver
    /// last saw activity. This test injects time via `check_and_record_at`.
    #[test]
    fn staleness_purge_removes_silent_peer_and_accepts_fresh_start() {
        // A peer whose stamp_at_max is older than the freshness window is purged;
        // a fresh datagram from it afterwards is accepted as first contact.
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.20".parse().unwrap();
        let window_ms = FRESHNESS_WINDOW_DEFAULT.as_millis() as u64;

        // Use a fixed "now" well into the future so we can control relative ages.
        // Base receiver time: t0. Sender stamp: also t0.
        let t0: u64 = 1_700_000_000_000; // arbitrary fixed ms epoch

        // Record one datagram: stamp = t0, receiver now = t0.
        assert!(check_at(&filter, peer, 1, t0, t0));

        // Advance simulated receiver clock past stamp_at_max + window: now = t0 + window + 1.
        // The stamp (t0) is now older than the window relative to the new `now`.
        let now_after_purge = t0 + window_ms + 1;

        // Deliver a fresh datagram from a DIFFERENT peer to trigger the opportunistic purge.
        // The other peer uses stamp = now_after_purge (fresh relative to that now).
        let other: IpAddr = "127.0.0.21".parse().unwrap();
        assert!(check_at(
            &filter,
            other,
            1,
            now_after_purge,
            now_after_purge
        ));

        // The original peer must have been purged (stamp_at_max = t0, now - t0 > window).
        assert!(
            !filter.peers.lock().contains_key(&peer),
            "stale peer entry should have been purged"
        );

        // A fresh datagram from the original peer is now accepted as first contact.
        assert!(
            check_at(&filter, peer, 1, now_after_purge, now_after_purge),
            "first-contact datagram after purge must be accepted"
        );
    }

    #[test]
    fn restart_with_small_seq_regression_is_accepted() {
        // Accept seqs 1..=5, then a regression to seq 1 with a strictly newer stamp is ACCEPTED
        // (restart reset), and an immediate replay of an old captured (seq, stamp) pair is rejected.
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.22".parse().unwrap();
        let now = phys_now_ms();

        // Accept seqs 1..=5 with stamp `now`.
        for seq in 1u64..=5 {
            assert!(
                filter.check_and_record(peer, seq, now),
                "seq {seq} should be accepted"
            );
        }

        // Restart: seq resets to 1 with a strictly newer stamp. This is INSIDE the window
        // (behind = 4, which is < WINDOW_SIZE = 1024), so the old code would check the bitmap
        // and reject it as a duplicate. The new code checks stamp FIRST.
        let newer = now + 1000;
        assert!(
            filter.check_and_record(peer, 1, newer),
            "seq regression inside the window with strictly newer stamp must be accepted as restart"
        );

        // After reset: max_seq=1, stamp_at_max=newer. seq=2 with newer stamp is accepted.
        assert!(filter.check_and_record(peer, 2, newer));

        // A replay of the old (seq=1, old stamp) pair must be rejected — stamp is not strictly
        // newer than stamp_at_max (now < newer), so restart check fails, then bitmap check:
        // bit 0 is set (seq=1 was the reset), so rejected as duplicate.
        assert!(
            !filter.check_and_record(peer, 1, now),
            "replay of old (seq=1, old stamp) after restart must be rejected"
        );
    }

    /// Test 1: skew-positive purge — sender clock ahead of receiver by some skew S ≤ window.
    ///
    /// Plant a datagram whose stamp is ahead of receiver `now` (within window), then advance
    /// simulated receiver time past `last-activity + window` but within `stamp_at_max + window`.
    /// The entry must NOT be purged, and a replay of the captured (seq, stamp) must be REJECTED.
    ///
    /// This demonstrates that purging on `stamp_at_max` (not receiver clock) is correct: even
    /// after the receiver's notion of "last activity" falls outside the window, an attacker cannot
    /// trigger a purge-and-replay if the sender's stamp is still within the window.
    #[test]
    fn skew_positive_purge_does_not_evict_while_stamp_at_max_is_fresh() {
        let filter = filter_5min();
        let peer: IpAddr = "127.1.0.1".parse().unwrap();
        let window_ms = FRESHNESS_WINDOW_DEFAULT.as_millis() as u64;

        // Receiver time at acceptance.
        let receiver_t0: u64 = 1_700_000_000_000;
        // Sender clock is 2 minutes ahead of receiver (positive skew, within window).
        let skew_ms: u64 = 2 * 60 * 1000;
        let sender_stamp: u64 = receiver_t0 + skew_ms;

        // Accept the datagram: stamp is ahead of receiver now but within window.
        assert!(check_at(&filter, peer, 42, sender_stamp, receiver_t0));

        // Advance receiver time past receiver_t0 + window (where last_activity_ms would have been
        // purged under the old scheme), but still within stamp_at_max + window.
        // Specifically: now = receiver_t0 + window + 1  (old scheme would purge here),
        // but stamp_at_max = receiver_t0 + skew_ms, so now - stamp_at_max = window + 1 - skew_ms
        // = window - skew_ms + 1 < window (since skew_ms > 0). Entry must NOT be purged.
        let now_mid = receiver_t0 + window_ms + 1;

        // Trigger opportunistic purge via a different peer (stamp = now_mid, fresh).
        let other: IpAddr = "127.1.0.2".parse().unwrap();
        assert!(check_at(&filter, other, 1, now_mid, now_mid));

        // The original peer must still be present (stamp_at_max - now_mid = skew_ms - 1 < window).
        assert!(
            filter.peers.lock().contains_key(&peer),
            "entry must NOT be purged while stamp_at_max is still within the freshness window"
        );

        // A replay of the captured (seq=42, sender_stamp) must be REJECTED despite the time
        // advance — the bitmap still has the sequence marked.
        assert!(
            !check_at(&filter, peer, 42, sender_stamp, now_mid),
            "replay of captured datagram must be rejected even when receiver clock has advanced"
        );
    }

    /// Test 2: post-restart tail — captured pre-restart datagrams are rejected after a restart.
    ///
    /// Accept seqs 1..=8 (stamps T..T+2), then a restart-reset via seq 1 stamp T+5.
    /// A replay of captured seq 8 (stamp ≤ T+2) is REJECTED (stamp < max_stamp_seen = T+5).
    /// A genuine new seq 2 stamp T+5 (same ms as reset) is ACCEPTED.
    #[test]
    fn post_restart_tail_captured_pre_restart_datagrams_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.2.0.1".parse().unwrap();
        let window_ms = FRESHNESS_WINDOW_DEFAULT.as_millis() as u64;

        // Fixed base stamp; receiver now tracks the same base so all stamps are fresh.
        let t: u64 = 1_700_000_000_000;
        // All datagrams and receiver now within freshness window of each other.
        let receiver_now = t + window_ms / 2; // midpoint; all stamps ≤ t+5 are fresh

        // Accept seqs 1..=8 with stamps in T..T+2 (simulate slight stamp spread).
        // Seqs 1..=6 at stamp T, seqs 7..=8 at stamp T+2.
        for seq in 1u64..=6 {
            assert!(
                check_at(&filter, peer, seq, t, receiver_now),
                "seq {seq} at stamp T should be accepted"
            );
        }
        for seq in 7u64..=8 {
            assert!(
                check_at(&filter, peer, seq, t + 2, receiver_now),
                "seq {seq} at stamp T+2 should be accepted"
            );
        }

        // Restart: seq resets to 1 with stamp T+5 (strictly newer than stamp_at_max = T+2).
        let restart_stamp = t + 5;
        assert!(
            check_at(&filter, peer, 1, restart_stamp, receiver_now),
            "restart datagram (seq=1, stamp=T+5) must be accepted"
        );
        // After reset: max_seq=1, stamp_at_max=T+5, max_stamp_seen=T+5.

        // Replay of captured pre-restart seq=8 with stamp T+2 (< max_stamp_seen = T+5):
        // forward path (8 > max_seq=1), stamp < max_stamp_seen → REJECTED.
        assert!(
            !check_at(&filter, peer, 8, t + 2, receiver_now),
            "captured pre-restart seq=8 (stamp T+2 < max_stamp_seen T+5) must be REJECTED"
        );

        // Genuine new datagram: seq=2, stamp=T+5 (same ms as reset stamp, >= max_stamp_seen).
        // Forward path (2 > max_seq=1), stamp == max_stamp_seen → ACCEPTED.
        assert!(
            check_at(&filter, peer, 2, restart_stamp, receiver_now),
            "genuine new seq=2 with stamp=T+5 (same ms as reset) must be ACCEPTED"
        );
    }
}

// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-peer replay protection for the authenticated modes (AGENTS.md §8).
//!
//! Every authenticated datagram carries a 16-byte replay header (`seq || stamp`, little-endian,
//! ms since epoch) inside the authenticated region. A `seq` already seen or behind the sliding
//! bitmap is rejected, as is a `stamp` deviating from local time by more than
//! [`FRESHNESS_WINDOW_DEFAULT`]. Unauthenticated mode carries no header and is exempt.
//!
//! Three rules the code must keep:
//!
//! - **Replay state outlives membership.** A decommissioned peer keeps its filter entry, or a
//!   captured datagram re-adds it to `members` and re-poisons causal stability. The staleness
//!   purge is sound only because no datagram can raise `stamp_at_max` without being accepted or
//!   triggering `reset`.
//! - **Restart beats regression.** For `seq <= max_seq`, `stamp > stamp_at_max` means a genuine
//!   restart and resets the state; otherwise the bitmap decides. *Residual*: a restart within the
//!   same millisecond is indistinguishable from a replay and is dropped.
//! - **Post-restart tail guard.** `PeerState::max_stamp_seen`, never rewound by `reset`, blocks a
//!   forward-path datagram with a strictly lower stamp; strict `<` because same-millisecond bursts
//!   share a stamp. Relies on [`SenderCounter::next_stamp`]'s in-process floor. *Residual*: a
//!   sender restarting with its clock behind its own stamps is treated as a replay until the clock
//!   catches up.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::ops::Sub;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;

/// Length of the replay header prepended to the authenticated portion of every datagram.
///
/// `seq (8 bytes) || stamp (8 bytes)`.
pub const REPLAY_HEADER_LEN: usize = 16;

/// Default freshness window: datagrams whose sender wall-clock stamp deviates from local physical
/// time by more than this value in either direction are rejected.
pub const FRESHNESS_WINDOW_DEFAULT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Size of the out-of-order acceptance bitmap: a `seq` up to this far behind `max_seq` is accepted
/// as legitimate UDP reordering (one bit per relative sequence number); older is rejected.
const WINDOW_SIZE: u64 = 1024;

/// A per-sender monotonic sequence number carried in the replay header. This module owns its wire
/// encoding and its ordering semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

impl Seq {
    /// The first sequence number a [`SenderCounter`] issues in authenticated mode.
    pub const FIRST: Seq = Seq(1);
    /// Sentinel carried on a [`crate::auth::Payload`] produced in unauthenticated mode, where no
    /// replay header exists.
    pub const NONE: Seq = Seq(0);

    /// Wrap a raw sequence number.
    #[allow(dead_code)] // used by cfg(test) unit tests and the `reconcile_internal_testing` cfg seam
    pub const fn new(value: u64) -> Seq {
        Seq(value)
    }

    /// Encode as the 8-byte little-endian replay-header representation.
    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Decode from the 8-byte little-endian replay-header representation.
    pub fn from_le_bytes(bytes: [u8; 8]) -> Seq {
        Seq(u64::from_le_bytes(bytes))
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Number of sequence numbers between two `Seq`s. Precondition: `self >= rhs`.
impl Sub for Seq {
    type Output = u64;

    fn sub(self, rhs: Seq) -> u64 {
        self.0 - rhs.0
    }
}

/// A sender wall-clock stamp (milliseconds since the Unix epoch) carried in the replay header.
///
/// This module owns its wire encoding and its freshness check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp(u64);

impl Stamp {
    /// Sentinel carried on a [`crate::auth::Payload`] produced in unauthenticated mode, where no
    /// replay header exists.
    pub const NONE: Stamp = Stamp(0);

    /// Wrap a raw millisecond-since-epoch value.
    #[allow(dead_code)] // used by cfg(test) unit tests and the `reconcile_internal_testing` cfg seam
    pub const fn new(value: u64) -> Stamp {
        Stamp(value)
    }

    /// Encode as the 8-byte little-endian replay-header representation.
    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Decode from the 8-byte little-endian replay-header representation.
    pub fn from_le_bytes(bytes: [u8; 8]) -> Stamp {
        Stamp(u64::from_le_bytes(bytes))
    }

    /// Read local physical time as a `Stamp` (ms since the Unix epoch, clamped to non-negative).
    fn now() -> Stamp {
        Stamp(phys_now_ms())
    }

    /// Whether `self` deviates from `now` by no more than `window` in either direction.
    fn is_fresh(self, now: Stamp, window: Duration) -> bool {
        let window_ms = window.as_millis() as u64;
        self.age_relative_to(now) <= window_ms && now.age_relative_to(self) <= window_ms
    }

    /// How far behind `now` this stamp is, saturating at 0 if `self` is not older than `now`.
    fn age_relative_to(self, now: Stamp) -> u64 {
        now.0.saturating_sub(self.0)
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Fixed-size out-of-order acceptance window: bit `i` records that `high_water - i` was accepted,
/// for `i < WINDOW_SIZE`.
///
/// Bit 0 set — the high-water sequence itself — is an invariant every mutator here maintains, so
/// no caller has to.
#[derive(Debug)]
struct SlidingBitmap([u64; (WINDOW_SIZE / 64) as usize]);

impl SlidingBitmap {
    /// A fresh window with only the high-water sequence itself marked.
    fn new() -> Self {
        let mut bitmap = SlidingBitmap([0u64; (WINDOW_SIZE / 64) as usize]);
        bitmap.mark(0);
        bitmap
    }

    /// Mark `offset` positions behind the high-water mark as seen. Precondition:
    /// `offset < WINDOW_SIZE`.
    fn mark(&mut self, offset: u64) {
        let word = (offset / 64) as usize;
        let bit = offset % 64;
        self.0[word] |= 1 << bit;
    }

    /// Whether `offset` has been marked seen. Same precondition as [`mark`](Self::mark).
    fn is_marked(&self, offset: u64) -> bool {
        let word = (offset / 64) as usize;
        let bit = offset % 64;
        self.0[word] & (1 << bit) != 0
    }

    /// Advance the high-water mark by `delta`, discarding bits past `WINDOW_SIZE - 1`, and mark
    /// the new high-water sequence as seen.
    fn advance(&mut self, delta: u64) {
        if delta >= WINDOW_SIZE {
            *self = Self::new();
            return;
        }
        let word_shift = (delta / 64) as usize;
        let bit_shift = (delta % 64) as u32;
        let words = (WINDOW_SIZE / 64) as usize;

        if bit_shift == 0 {
            for i in (0..words).rev() {
                self.0[i] = if i >= word_shift {
                    self.0[i - word_shift]
                } else {
                    0
                };
            }
        } else {
            for i in (0..words).rev() {
                let lo = if i >= word_shift {
                    self.0[i - word_shift] << bit_shift
                } else {
                    0
                };
                let hi = if i > word_shift {
                    self.0[i - word_shift - 1] >> (64 - bit_shift)
                } else {
                    0
                };
                self.0[i] = lo | hi;
            }
        }
        self.mark(0);
    }
}

/// The per-peer replay state.
#[derive(Debug)]
struct PeerState {
    /// Highest sequence number accepted from this peer.
    max_seq: Seq,
    /// Sender stamp on the datagram carrying `max_seq`; restart detection compares against it.
    stamp_at_max: Stamp,
    /// Monotone high-water mark of accepted sender stamps. Never rewound by `reset` — the
    /// post-restart tail guard.
    max_stamp_seen: Stamp,
    /// Sliding out-of-order acceptance window, relative to `max_seq`.
    bitmap: SlidingBitmap,
}

impl PeerState {
    fn new(first_seq: Seq, first_stamp: Stamp) -> Self {
        PeerState {
            max_seq: first_seq,
            stamp_at_max: first_stamp,
            max_stamp_seen: first_stamp,
            bitmap: SlidingBitmap::new(),
        }
    }

    /// The sender stamp at the highest accepted sequence number.
    fn stamp_at_max(&self) -> Stamp {
        self.stamp_at_max
    }

    /// Whether the datagram is fresh, updating the bitmap and high-water marks if it is.
    fn accept(&mut self, seq: Seq, stamp: Stamp) -> bool {
        if seq > self.max_seq {
            // Post-restart tail guard; strict `<` so same-millisecond bursts still pass.
            if stamp < self.max_stamp_seen {
                return false;
            }
            let delta = seq - self.max_seq;
            self.bitmap.advance(delta);
            self.max_seq = seq;
            self.stamp_at_max = stamp;
            self.max_stamp_seen = self.max_stamp_seen.max(stamp);
            true
        } else {
            // A strictly newer stamp at or below `max_seq` is a restart, not a regression.
            if stamp > self.stamp_at_max {
                self.reset(seq, stamp);
                return true;
            }
            let behind = self.max_seq - seq;
            if behind >= WINDOW_SIZE {
                return false;
            }
            if self.bitmap.is_marked(behind) {
                return false;
            }
            self.bitmap.mark(behind);
            true
        }
    }

    /// Reset state for a restarted sender. `max_stamp_seen` is never rewound — it is the tail
    /// guard.
    fn reset(&mut self, new_seq: Seq, new_stamp: Stamp) {
        self.max_seq = new_seq;
        self.stamp_at_max = new_stamp;
        self.max_stamp_seen = self.max_stamp_seen.max(new_stamp);
        self.bitmap = SlidingBitmap::new();
    }
}

/// Read the local physical time as milliseconds since the Unix epoch.
fn phys_now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// Sender-side replay state, one per node. `stamp_floor` keeps minted stamps monotonic within the
/// process — the guarantee the receiver's tail guard relies on, lost on restart (module docs).
#[derive(Debug)]
pub struct SenderCounter {
    seq: AtomicU64,
    stamp_floor: AtomicU64,
}

impl Default for SenderCounter {
    fn default() -> Self {
        SenderCounter::new()
    }
}

impl SenderCounter {
    /// A fresh counter: sequence numbers start at [`Seq::FIRST`], the stamp floor at 0.
    pub fn new() -> Self {
        SenderCounter {
            seq: AtomicU64::new(Seq::FIRST.0),
            stamp_floor: AtomicU64::new(0),
        }
    }

    /// Allocate the next sequence number (strictly increasing).
    pub fn next_seq(&self) -> Seq {
        Seq(self.seq.fetch_add(1, Ordering::Relaxed))
    }

    /// Mint a monotonically non-decreasing sender stamp, advancing the internal floor.
    pub fn next_stamp(&self) -> Stamp {
        self.next_stamp_at(phys_now_ms())
    }

    /// [`next_stamp`](Self::next_stamp) with an injectable `now_ms`.
    pub fn next_stamp_at(&self, now_ms: u64) -> Stamp {
        let prev = self.stamp_floor.fetch_max(now_ms, Ordering::Relaxed);
        Stamp(prev.max(now_ms))
    }
}

/// Receiver-side per-peer replay filter.
///
/// Entries are purged once `now - stamp_at_max > window`, at which point no replayable datagram
/// could clear the freshness check anyway. `enabled` mirrors the owning
/// [`crate::auth::Authenticator`]'s mode, fixed at construction; a disabled filter accepts
/// everything, so no caller decides whether replay-checking applies.
#[derive(Debug)]
pub struct ReplayFilter {
    peers: Mutex<HashMap<IpAddr, PeerState>>,
    freshness_window: Duration,
    enabled: bool,
}

impl ReplayFilter {
    /// Build an empty filter. `enabled` mirrors the owning
    /// [`crate::auth::Authenticator`]'s mode.
    pub fn new(freshness_window: Duration, enabled: bool) -> Self {
        ReplayFilter {
            peers: Mutex::new(HashMap::new()),
            freshness_window,
            enabled,
        }
    }

    /// Whether a datagram from `sender` is fresh and unique; `false` means drop it silently.
    /// Always `true` on a disabled filter.
    pub fn check_and_record(&self, sender: IpAddr, seq: Seq, stamp: Stamp) -> bool {
        if !self.enabled {
            return true;
        }
        self.check_and_record_at(sender, seq, stamp, Stamp::now())
    }

    /// [`check_and_record`](Self::check_and_record) with an injectable `now`.
    fn check_and_record_at(&self, sender: IpAddr, seq: Seq, stamp: Stamp, now: Stamp) -> bool {
        if !stamp.is_fresh(now, self.freshness_window) {
            return false;
        }

        let mut map = self.peers.lock();

        // Opportunistic staleness purge; sound because a replayable stamp would fail above.
        let window = self.freshness_window;
        map.retain(|_, s| s.stamp_at_max().age_relative_to(now) <= window.as_millis() as u64);

        match map.get_mut(&sender) {
            None => {
                map.insert(sender, PeerState::new(seq, stamp));
                true
            }
            Some(state) => state.accept(seq, stamp),
        }
    }

    /// Number of peers currently tracked. For test assertions; the `reconcile_internal_testing` cfg sits
    /// on `reconcile::testing::replay_filter_len`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.peers.lock().len()
    }

    /// Drop a peer's replay state. For tests: production relies on the staleness purge.
    #[cfg(test)]
    pub fn evict(&self, peer: IpAddr) {
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
        assert_eq!(c.next_seq(), Seq::new(1));
        assert_eq!(c.next_seq(), Seq::new(2));
        assert_eq!(c.next_seq(), Seq::new(3));
    }

    /// The stamp mint stays monotonic across a backward clock step.
    #[test]
    fn stamp_mint_is_monotonic_across_backward_clock_steps() {
        let c = SenderCounter::new();

        let t1 = 1_700_000_000_000_u64;
        let t2 = t1 + 50;
        let t3 = t2 + 50;

        assert_eq!(
            c.next_stamp_at(t1),
            Stamp::new(t1),
            "first stamp equals wall clock"
        );
        assert_eq!(
            c.next_stamp_at(t2),
            Stamp::new(t2),
            "second stamp equals advancing wall clock"
        );
        assert_eq!(
            c.next_stamp_at(t3),
            Stamp::new(t3),
            "third stamp equals advancing wall clock"
        );

        let t_regressed = t3 - 500;
        let s_after_regression = c.next_stamp_at(t_regressed);
        assert_eq!(
            s_after_regression,
            Stamp::new(t3),
            "stamp after backward step must equal the floor (t3), not the regressed wall clock"
        );
        assert!(
            s_after_regression >= Stamp::new(t3),
            "stamp must not decrease after backward clock step"
        );

        assert_eq!(
            c.next_stamp_at(t_regressed),
            Stamp::new(t3),
            "floor remains at t3 for further backward clock values"
        );

        let t_recovered = t3 + 1;
        assert_eq!(
            c.next_stamp_at(t_recovered),
            Stamp::new(t_recovered),
            "stamps advance normally once wall clock passes the floor"
        );
    }

    // ── PeerState bitmap ──────────────────────────────────────────────────────

    #[test]
    fn bitmap_shift_by_1() {
        let mut state = PeerState::new(Seq::new(10), Stamp::new(1000));
        assert!(!state.accept(Seq::new(10), Stamp::new(1000))); // already seen
                                                                // out of order: seq 9
        assert!(state.accept(Seq::new(9), Stamp::new(999)));
        assert!(!state.accept(Seq::new(9), Stamp::new(999))); // already seen
    }

    #[test]
    fn bitmap_advance_and_reuse() {
        let mut state = PeerState::new(Seq::new(10), Stamp::new(1000));
        assert!(state.accept(Seq::new(20), Stamp::new(1000)));
        assert!(!state.accept(Seq::new(10), Stamp::new(1000)));
        assert!(state.accept(Seq::new(15), Stamp::new(1000)));
        assert!(!state.accept(Seq::new(15), Stamp::new(1000)));
    }

    #[test]
    fn bitmap_outside_window_is_rejected() {
        let mut state = PeerState::new(Seq::new(1024), Stamp::new(1000));
        assert!(state.accept(Seq::new(2048), Stamp::new(1000)));
        assert!(!state.accept(Seq::new(1), Stamp::new(1000)));
    }

    #[test]
    fn bitmap_large_jump_clears_window() {
        let mut state = PeerState::new(Seq::new(10), Stamp::new(1000));
        assert!(state.accept(Seq::new(10 + WINDOW_SIZE + 5), Stamp::new(1000)));
        assert!(!state.accept(Seq::new(10), Stamp::new(1000)));
    }

    #[test]
    fn in_order_sequence_all_accepted_once() {
        let mut state = PeerState::new(Seq::new(1), Stamp::new(1000));
        for seq in 2..=100 {
            assert!(
                state.accept(Seq::new(seq), Stamp::new(1000)),
                "seq {seq} should be accepted"
            );
        }
        for seq in 1..=100 {
            assert!(
                !state.accept(Seq::new(seq), Stamp::new(1000)),
                "seq {seq} should be rejected as duplicate"
            );
        }
    }

    // ── ReplayFilter freshness check ──────────────────────────────────────────

    fn filter_5min() -> ReplayFilter {
        ReplayFilter::new(FRESHNESS_WINDOW_DEFAULT, true)
    }

    /// `check_and_record_at` with a caller-supplied `now`.
    fn check_at(filter: &ReplayFilter, peer: IpAddr, seq: u64, stamp: u64, now: u64) -> bool {
        filter.check_and_record_at(peer, Seq::new(seq), Stamp::new(stamp), Stamp::new(now))
    }

    #[test]
    fn fresh_datagram_accepted() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let now = phys_now_ms();
        assert!(filter.check_and_record(peer, Seq::new(1), Stamp::new(now)));
    }

    /// A disabled filter accepts everything, including stamps outside the freshness window.
    #[test]
    fn disabled_filter_always_accepts() {
        let filter = ReplayFilter::new(FRESHNESS_WINDOW_DEFAULT, false);
        let peer: IpAddr = "127.0.0.9".parse().unwrap();
        assert!(filter.check_and_record(peer, Seq::NONE, Stamp::NONE));
        assert!(filter.check_and_record(peer, Seq::new(1), Stamp::new(0)));
    }

    #[test]
    fn replay_of_same_datagram_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.2".parse().unwrap();
        let now = phys_now_ms();
        assert!(filter.check_and_record(peer, Seq::new(1), Stamp::new(now)));
        assert!(!filter.check_and_record(peer, Seq::new(1), Stamp::new(now)));
    }

    #[test]
    fn stale_stamp_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.3".parse().unwrap();
        let old_stamp = phys_now_ms().saturating_sub(10 * 60 * 1000 + 1);
        assert!(!filter.check_and_record(peer, Seq::new(1), Stamp::new(old_stamp)));
    }

    #[test]
    fn far_future_stamp_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.4".parse().unwrap();
        let future_stamp = phys_now_ms() + 10 * 60 * 1000 + 1;
        assert!(!filter.check_and_record(peer, Seq::new(1), Stamp::new(future_stamp)));
    }

    #[test]
    fn out_of_order_within_window_accepted_once() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.5".parse().unwrap();
        let now = phys_now_ms();
        assert!(filter.check_and_record(peer, Seq::new(5), Stamp::new(now)));
        assert!(filter.check_and_record(peer, Seq::new(3), Stamp::new(now)));
        assert!(!filter.check_and_record(peer, Seq::new(5), Stamp::new(now)));
        assert!(!filter.check_and_record(peer, Seq::new(3), Stamp::new(now)));
    }

    #[test]
    fn seq_regression_outside_window_with_newer_stamp_accepted_as_restart() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.6".parse().unwrap();
        let now = phys_now_ms();
        let high_seq = WINDOW_SIZE + 100;
        assert!(filter.check_and_record(peer, Seq::new(high_seq), Stamp::new(now)));
        let newer = now + 1000;
        assert!(
            filter.check_and_record(peer, Seq::new(1), Stamp::new(newer)),
            "a seq regression outside the window with a strictly newer stamp must be accepted as a restart"
        );
        assert!(filter.check_and_record(peer, Seq::new(2), Stamp::new(newer)));
        assert!(!filter.check_and_record(peer, Seq::new(1), Stamp::new(newer)));
    }

    #[test]
    fn seq_regression_outside_window_with_old_stamp_rejected_as_replay() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.7".parse().unwrap();
        let now = phys_now_ms();
        let high_seq = WINDOW_SIZE + 100;
        assert!(filter.check_and_record(peer, Seq::new(high_seq), Stamp::new(now)));
        assert!(
            !filter.check_and_record(peer, Seq::new(1), Stamp::new(now)),
            "seq regression outside the window with same stamp must be rejected as replay"
        );
    }

    #[test]
    fn evict_clears_state_and_allows_fresh_start() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.8".parse().unwrap();
        let now = phys_now_ms();
        assert!(filter.check_and_record(peer, Seq::new(10), Stamp::new(now)));
        filter.evict(peer);
        assert!(filter.check_and_record(peer, Seq::new(1), Stamp::new(now)));
    }

    /// The staleness purge keys on sender `stamp_at_max`, not on receiver activity.
    #[test]
    fn staleness_purge_removes_silent_peer_and_accepts_fresh_start() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.20".parse().unwrap();
        let window_ms = FRESHNESS_WINDOW_DEFAULT.as_millis() as u64;

        let t0: u64 = 1_700_000_000_000; // arbitrary fixed ms epoch

        assert!(check_at(&filter, peer, 1, t0, t0));

        let now_after_purge = t0 + window_ms + 1;

        // A datagram from another peer triggers the opportunistic purge.
        let other: IpAddr = "127.0.0.21".parse().unwrap();
        assert!(check_at(
            &filter,
            other,
            1,
            now_after_purge,
            now_after_purge
        ));

        assert!(
            !filter.peers.lock().contains_key(&peer),
            "stale peer entry should have been purged"
        );

        assert!(
            check_at(&filter, peer, 1, now_after_purge, now_after_purge),
            "first-contact datagram after purge must be accepted"
        );
    }

    #[test]
    fn restart_with_small_seq_regression_is_accepted() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.22".parse().unwrap();
        let now = phys_now_ms();

        for seq in 1u64..=5 {
            assert!(
                filter.check_and_record(peer, Seq::new(seq), Stamp::new(now)),
                "seq {seq} should be accepted"
            );
        }

        // Restart inside the bitmap window: the stamp check must win over the bitmap.
        let newer = now + 1000;
        assert!(
            filter.check_and_record(peer, Seq::new(1), Stamp::new(newer)),
            "seq regression inside the window with strictly newer stamp must be accepted as restart"
        );

        assert!(filter.check_and_record(peer, Seq::new(2), Stamp::new(newer)));

        // A replay of the old (seq, stamp) pair is neither a restart nor unseen.
        assert!(
            !filter.check_and_record(peer, Seq::new(1), Stamp::new(now)),
            "replay of old (seq=1, old stamp) after restart must be rejected"
        );
    }

    /// With the sender's clock ahead, a receiver-activity-based purge would open a replay window;
    /// purging on `stamp_at_max` must not.
    #[test]
    fn skew_positive_purge_does_not_evict_while_stamp_at_max_is_fresh() {
        let filter = filter_5min();
        let peer: IpAddr = "127.1.0.1".parse().unwrap();
        let window_ms = FRESHNESS_WINDOW_DEFAULT.as_millis() as u64;

        let receiver_t0: u64 = 1_700_000_000_000;
        let skew_ms: u64 = 2 * 60 * 1000;
        let sender_stamp: u64 = receiver_t0 + skew_ms;

        assert!(check_at(&filter, peer, 42, sender_stamp, receiver_t0));

        // Past `receiver_t0 + window`, but still within `stamp_at_max + window`.
        let now_mid = receiver_t0 + window_ms + 1;

        let other: IpAddr = "127.1.0.2".parse().unwrap();
        assert!(check_at(&filter, other, 1, now_mid, now_mid));

        assert!(
            filter.peers.lock().contains_key(&peer),
            "entry must NOT be purged while stamp_at_max is still within the freshness window"
        );

        assert!(
            !check_at(&filter, peer, 42, sender_stamp, now_mid),
            "replay of captured datagram must be rejected even when receiver clock has advanced"
        );
    }

    /// After a restart, a captured pre-restart datagram is rejected on the forward path, while a
    /// genuine one minted in the reset millisecond is accepted.
    #[test]
    fn post_restart_tail_captured_pre_restart_datagrams_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.2.0.1".parse().unwrap();
        let window_ms = FRESHNESS_WINDOW_DEFAULT.as_millis() as u64;

        let t: u64 = 1_700_000_000_000;
        let receiver_now = t + window_ms / 2; // midpoint; all stamps ≤ t+5 are fresh

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

        let restart_stamp = t + 5;
        assert!(
            check_at(&filter, peer, 1, restart_stamp, receiver_now),
            "restart datagram (seq=1, stamp=T+5) must be accepted"
        );

        assert!(
            !check_at(&filter, peer, 8, t + 2, receiver_now),
            "captured pre-restart seq=8 (stamp T+2 < max_stamp_seen T+5) must be REJECTED"
        );

        assert!(
            check_at(&filter, peer, 2, restart_stamp, receiver_now),
            "genuine new seq=2 with stamp=T+5 (same ms as reset) must be ACCEPTED"
        );
    }
}

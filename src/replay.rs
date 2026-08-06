// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-peer replay protection for the authenticated protocol modes.
//!
//! When a cluster key is configured, every outgoing datagram carries a 16-byte **replay header**
//! (`seq: u64` then `stamp: u64`, little-endian, milliseconds since epoch) inside the authenticated
//! region, first before any protocol messages. The receiver keeps per-peer state to reject a
//! sequence number already seen or older than the sliding-bitmap window, and a stamp that deviates
//! from local physical time by more than [`FRESHNESS_WINDOW_DEFAULT`]. Unauthenticated mode carries
//! no header and is exempt.
//!
//! # Replay state outlives membership
//!
//! Decommissioning a peer drops it from `members`/`peers`, but its replay-filter entry is kept: a
//! captured datagram replayed after decommission would otherwise pass MAC and freshness checks and
//! re-add the peer to `members`, re-poisoning causal stability. The staleness purge
//! (`now - stamp_at_max <= window`, opportunistic on every `check_and_record` via `HashMap::retain`)
//! is sound because no datagram can carry a stamp greater than `stamp_at_max` without either being
//! accepted (raising it) or triggering `reset`, so once the window has passed no replay can clear
//! the freshness check regardless of sender skew.
//!
//! # Sequence regression vs. restart
//!
//! A sender's sequence counter resets to 1 on restart, so every `seq <= max_seq` is checked against
//! the stamp first: `stamp > stamp_at_max` means a genuine restart (state resets, datagram
//! accepted); otherwise the bitmap decides. **Residual**: a restart within the same millisecond as
//! the pre-restart send produces `stamp == stamp_at_max`, fails the strict `>`, and is
//! indistinguishable from a same-millisecond replay — dropped by design.
//!
//! # Post-restart tail guard
//!
//! A reset clears `max_seq`, so a captured high-`seq` pre-restart datagram would otherwise re-enter
//! on the forward path. `PeerState::max_stamp_seen` — never rewound by `reset()` — blocks any
//! forward-path datagram with a strictly lower stamp. [`SenderCounter::next_stamp`] enforces the
//! monotonicity this relies on via an in-process floor (`max(wall_clock_now, floor)`), which is why
//! the guard uses strict `<`: same-millisecond bursts share a stamp and must still pass. **Residual**:
//! a sender restarting with its wall clock behind its own pre-restart stamps (NTP step, VM resume)
//! loses that floor and is treated as a replay until its clock catches up — bounded by the clock
//! step, same trade-off family as the regression residual above.

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
pub(crate) const REPLAY_HEADER_LEN: usize = 16;

/// Default freshness window: datagrams whose sender wall-clock stamp deviates from local physical
/// time by more than this value in either direction are rejected.
pub const FRESHNESS_WINDOW_DEFAULT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Size of the out-of-order acceptance bitmap: a `seq` up to this far behind `max_seq` is accepted
/// as legitimate UDP reordering (one bit per relative sequence number); older is rejected.
const WINDOW_SIZE: u64 = 1024;

/// A per-sender monotonic sequence number carried in the replay header.
///
/// This module is the sole owner of `seq`'s semantics — its wire encoding
/// ([`to_le_bytes`](Seq::to_le_bytes)/[`from_le_bytes`](Seq::from_le_bytes)) and the ordering used
/// to detect replays and restarts live here rather than being reconstructed from a bare `u64` at
/// each call site (see `AGENTS.md`, "entities own their own validation").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Seq(u64);

impl Seq {
    /// The first sequence number a [`SenderCounter`] issues in authenticated mode.
    pub(crate) const FIRST: Seq = Seq(1);
    /// Sentinel carried on a [`crate::auth::Payload`] produced in unauthenticated mode, where no
    /// replay header exists.
    pub(crate) const NONE: Seq = Seq(0);

    #[allow(dead_code)] // used by cfg(test) unit tests and the `internal-testing` feature seam
    pub(crate) const fn new(value: u64) -> Seq {
        Seq(value)
    }

    pub(crate) fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub(crate) fn from_le_bytes(bytes: [u8; 8]) -> Seq {
        Seq(u64::from_le_bytes(bytes))
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Number of sequence numbers between two `Seq`s. Only meaningful (and only used) where the
/// caller already knows `self >= rhs`, mirroring the raw `u64` subtraction it replaces.
impl Sub for Seq {
    type Output = u64;

    fn sub(self, rhs: Seq) -> u64 {
        self.0 - rhs.0
    }
}

/// A sender wall-clock stamp (milliseconds since the Unix epoch) carried in the replay header.
///
/// This module is the sole owner of `stamp`'s semantics: its wire encoding and the freshness
/// check ([`is_fresh`](Stamp::is_fresh)) that decides whether a stamp is acceptable both live here,
/// rather than [`ReplayFilter`] reaching into a bare `u64` to redo that arithmetic itself (see
/// `AGENTS.md`, "entities own their own validation").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Stamp(u64);

impl Stamp {
    /// Sentinel carried on a [`crate::auth::Payload`] produced in unauthenticated mode, where no
    /// replay header exists.
    pub(crate) const NONE: Stamp = Stamp(0);

    #[allow(dead_code)] // used by cfg(test) unit tests and the `internal-testing` feature seam
    pub(crate) const fn new(value: u64) -> Stamp {
        Stamp(value)
    }

    pub(crate) fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub(crate) fn from_le_bytes(bytes: [u8; 8]) -> Stamp {
        Stamp(u64::from_le_bytes(bytes))
    }

    /// Read local physical time as a `Stamp` (ms since the Unix epoch, clamped to non-negative).
    fn now() -> Stamp {
        Stamp(phys_now_ms())
    }

    /// Whether `self` deviates from `now` by no more than `window` in either direction — the
    /// freshness check applied to every incoming replay header.
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

/// Fixed-size out-of-order acceptance window: bit `i` records whether the sequence number
/// `high_water - i` has already been accepted, for `i` in `0..WINDOW_SIZE`; bits beyond that are
/// not tracked (always treated as outside the window). Bit 0 — "the current high-water sequence
/// itself was accepted" — is an invariant every constructor/mutator here maintains, so
/// [`PeerState`] never has to remember to set it by hand at each of its call sites (the bug class
/// this type exists to close: three call sites independently poking the same bit, one of which
/// forgets).
///
/// Stored as a fixed-size array of `u64` words; `WINDOW_SIZE / 64 = 16`.
struct SlidingBitmap([u64; (WINDOW_SIZE / 64) as usize]);

impl SlidingBitmap {
    /// A fresh window with only the high-water sequence itself marked.
    fn new() -> Self {
        let mut bitmap = SlidingBitmap([0u64; (WINDOW_SIZE / 64) as usize]);
        bitmap.mark(0);
        bitmap
    }

    /// Mark `offset` (sequence numbers behind the current high-water mark) as seen.
    ///
    /// Only meaningful for `offset < WINDOW_SIZE`; callers already check that before reaching
    /// here (a wider offset is unconditionally out of window, never marked or tested).
    fn mark(&mut self, offset: u64) {
        let word = (offset / 64) as usize;
        let bit = offset % 64;
        self.0[word] |= 1 << bit;
    }

    /// Whether `offset` has already been marked seen. Same `offset < WINDOW_SIZE` precondition as
    /// [`mark`](Self::mark).
    fn is_marked(&self, offset: u64) -> bool {
        let word = (offset / 64) as usize;
        let bit = offset % 64;
        self.0[word] & (1 << bit) != 0
    }

    /// Advance the high-water mark by `delta` positions (oldest bits fall off the window) and
    /// mark the new high-water sequence as seen.
    ///
    /// A shift of 1 means the high-water sequence moved up by 1; the former bit 0 becomes bit 1,
    /// etc. Bits that shift past position `WINDOW_SIZE - 1` are discarded.
    fn advance(&mut self, delta: u64) {
        if delta >= WINDOW_SIZE {
            // The whole window falls off — start fresh.
            *self = Self::new();
            return;
        }
        let word_shift = (delta / 64) as usize;
        let bit_shift = (delta % 64) as u32;
        let words = (WINDOW_SIZE / 64) as usize;

        if bit_shift == 0 {
            // Whole-word shift only.
            for i in (0..words).rev() {
                self.0[i] = if i >= word_shift {
                    self.0[i - word_shift]
                } else {
                    0
                };
            }
        } else {
            // Combined word + bit shift.
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
///
/// Tracks the highest sequence number seen from a peer, the sender stamp at which that maximum was
/// accepted, a sliding bitmap for the out-of-order acceptance window, and a monotone high-water
/// mark of sender stamps (used for the post-restart tail guard).
struct PeerState {
    /// Highest sequence number accepted from this peer.
    max_seq: Seq,
    /// Sender wall-clock stamp that was present on the datagram carrying `max_seq`.
    /// Used for restart detection: if a new datagram has a higher stamp and a lower seq, the peer
    /// has restarted.
    stamp_at_max: Stamp,
    /// Monotonically non-decreasing high-water mark of all sender stamps ever accepted from this
    /// peer. Never reset by `reset()`. Used on the forward path to reject captured pre-restart
    /// datagrams whose stamp predates the restart stamp.
    max_stamp_seen: Stamp,
    /// Sliding out-of-order acceptance window, relative to `max_seq`. See [`SlidingBitmap`].
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

    /// Return the sender stamp at the highest accepted sequence number (used for staleness purge).
    fn stamp_at_max(&self) -> Stamp {
        self.stamp_at_max
    }

    /// Attempt to accept a datagram with the given `seq` and `stamp`.
    ///
    /// Returns `true` if the datagram is fresh (not a replay), `false` if it should be rejected.
    ///
    /// Side effect on success: updates the bitmap and `max_seq`/`stamp_at_max` as appropriate.
    fn accept(&mut self, seq: Seq, stamp: Stamp) -> bool {
        if seq > self.max_seq {
            // Forward path: new high-water sequence.
            // Post-restart tail guard: reject pre-restart captured datagrams. A genuinely
            // later-minted datagram always has stamp >= every prior datagram (sender mints
            // monotonically). Same-millisecond bursts share a stamp, so guard uses strict <.
            if stamp < self.max_stamp_seen {
                return false;
            }
            // Advance the window by (seq - max_seq) positions; `advance` marks the new
            // high-water sequence itself as seen.
            let delta = seq - self.max_seq;
            self.bitmap.advance(delta);
            self.max_seq = seq;
            self.stamp_at_max = stamp;
            self.max_stamp_seen = self.max_stamp_seen.max(stamp);
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
            if self.bitmap.is_marked(behind) {
                // Already seen: duplicate.
                return false;
            }
            // First time in window: accept and mark.
            self.bitmap.mark(behind);
            true
        }
    }

    /// Reset state for a restarted sender.
    ///
    /// `max_stamp_seen` is intentionally NOT reset — it is a monotone high-water mark that
    /// persists across restarts to guard against replays of captured pre-restart datagrams.
    fn reset(&mut self, new_seq: Seq, new_stamp: Stamp) {
        self.max_seq = new_seq;
        self.stamp_at_max = new_stamp;
        // max_stamp_seen is never rewound — keep the monotone high-water mark.
        self.max_stamp_seen = self.max_stamp_seen.max(new_stamp);
        self.bitmap = SlidingBitmap::new();
    }
}

/// Read the local physical time as milliseconds since the Unix epoch.
fn phys_now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// Sender-side replay state: one per node, incremented for every authenticated-mode datagram.
/// `seq` starts at 1 (0 means unauthenticated). `stamp_floor` makes minted stamps monotonic within
/// the process lifetime — [`next_stamp`](Self::next_stamp) returns
/// `max(Utc::now().timestamp_millis(), previous_stamp)` — which is the guarantee the receiver's
/// tail guard relies on (module docs). The floor is lost on restart; see the residual there.
pub(crate) struct SenderCounter {
    seq: AtomicU64,
    stamp_floor: AtomicU64,
}

impl SenderCounter {
    pub(crate) fn new() -> Self {
        SenderCounter {
            seq: AtomicU64::new(Seq::FIRST.0),
            stamp_floor: AtomicU64::new(0),
        }
    }

    /// Allocate the next sequence number (strictly increasing).
    pub(crate) fn next_seq(&self) -> Seq {
        Seq(self.seq.fetch_add(1, Ordering::Relaxed))
    }

    /// Mint a monotonically non-decreasing sender stamp.
    ///
    /// Each call returns `max(Utc::now().timestamp_millis().max(0), floor)` and advances the
    /// internal floor so subsequent calls never return a smaller value.
    pub(crate) fn next_stamp(&self) -> Stamp {
        self.next_stamp_at(phys_now_ms())
    }

    /// Inner implementation with an injectable `now_ms` for unit-testing backward clock steps.
    pub(crate) fn next_stamp_at(&self, now_ms: u64) -> Stamp {
        // `fetch_max` atomically sets floor = max(floor, now_ms) and returns the OLD value.
        // The stamp we return is the maximum of the returned old floor and now_ms, which equals
        // the new floor value after the update.
        let prev = self.stamp_floor.fetch_max(now_ms, Ordering::Relaxed);
        Stamp(prev.max(now_ms))
    }
}

/// Receiver-side per-peer replay filter.
///
/// Maintains per-peer [`PeerState`] and enforces the freshness window. Entries are purged
/// opportunistically once `now - stamp_at_max > window`: at that point any replayable datagram
/// (stamp ≤ `stamp_at_max`) would fail the freshness check, so no replay can pass and the entry is
/// safe to drop, reclaiming memory automatically.
///
/// `enabled` mirrors the owning [`crate::auth::Authenticator`]'s mode, fixed for the filter's
/// whole lifetime (the authenticator's mode never changes after startup). Baking it in here — a
/// one-time decision at construction — means [`check_and_record`](Self::check_and_record) itself
/// never has to ask another object whether replay-checking even applies; a disabled filter simply
/// accepts everything ("parse, don't validate": the filter owns the answer to "is this datagram
/// fresh", including the degenerate case where the question doesn't apply).
pub(crate) struct ReplayFilter {
    peers: Mutex<HashMap<IpAddr, PeerState>>,
    freshness_window: Duration,
    enabled: bool,
}

impl ReplayFilter {
    pub(crate) fn new(freshness_window: Duration, enabled: bool) -> Self {
        ReplayFilter {
            peers: Mutex::new(HashMap::new()),
            freshness_window,
            enabled,
        }
    }

    /// Decide whether a datagram from `sender` with the given `seq` and `stamp` should be accepted.
    ///
    /// Returns `true` when the datagram is fresh and unique — the caller may proceed to process it.
    /// Returns `false` when the datagram is a replay (duplicate, too old within the window, or
    /// outside the freshness window) — the caller should drop it silently.
    ///
    /// Always `true` when the filter is disabled (unauthenticated mode carries no replay header
    /// to check) — the caller does not need to ask separately.
    pub(crate) fn check_and_record(&self, sender: IpAddr, seq: Seq, stamp: Stamp) -> bool {
        if !self.enabled {
            return true;
        }
        self.check_and_record_at(sender, seq, stamp, Stamp::now())
    }

    /// Inner implementation with an injectable `now` (as a [`Stamp`]).
    ///
    /// Separating the clock source allows unit tests to exercise time-dependent logic
    /// (staleness purge, skew-positive scenarios) without sleeping.
    fn check_and_record_at(&self, sender: IpAddr, seq: Seq, stamp: Stamp, now: Stamp) -> bool {
        // Freshness window check: reject stamps that are too far in the past or future. `Stamp`
        // owns this check, not the filter (see the `Stamp` doc comment).
        if !stamp.is_fresh(now, self.freshness_window) {
            return false;
        }

        let mut map = self.peers.lock();

        // Opportunistic staleness purge: entries whose stamp_at_max is older than the freshness
        // window cannot produce any accepted datagrams (any replayable stamp ≤ stamp_at_max would
        // fail the freshness check above), so it is safe to drop them.
        let window = self.freshness_window;
        map.retain(|_, s| s.stamp_at_max().age_relative_to(now) <= window.as_millis() as u64);

        match map.get_mut(&sender) {
            None => {
                // First datagram from this sender.
                map.insert(sender, PeerState::new(seq, stamp));
                true
            }
            Some(state) => state.accept(seq, stamp),
        }
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
        assert_eq!(c.next_seq(), Seq::new(1));
        assert_eq!(c.next_seq(), Seq::new(2));
        assert_eq!(c.next_seq(), Seq::new(3));
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

        // Simulate a backward step: wall clock goes back 500 ms (e.g. NTP correction).
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

        // Verify the floor is sticky: a second regressed call still yields t3.
        assert_eq!(
            c.next_stamp_at(t_regressed),
            Stamp::new(t3),
            "floor remains at t3 for further backward clock values"
        );

        // Once the wall clock catches back up past the floor, stamps advance again.
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
        // max_seq = 10, bit 0 set
        assert!(!state.accept(Seq::new(10), Stamp::new(1000))); // already seen
                                                                // out of order: seq 9
        assert!(state.accept(Seq::new(9), Stamp::new(999)));
        // seq 9 accepted, now bitmap has bit 0 (=10) and bit 1 (=9) set
        assert!(!state.accept(Seq::new(9), Stamp::new(999))); // already seen
    }

    #[test]
    fn bitmap_advance_and_reuse() {
        let mut state = PeerState::new(Seq::new(10), Stamp::new(1000));
        // advance max_seq to 20
        assert!(state.accept(Seq::new(20), Stamp::new(1000)));
        // seq 10 was already accepted when state was created: must be rejected as duplicate
        assert!(!state.accept(Seq::new(10), Stamp::new(1000)));
        // seq 15 (mid-window, never seen before) must be accepted
        assert!(state.accept(Seq::new(15), Stamp::new(1000)));
        // seq 15 again: rejected
        assert!(!state.accept(Seq::new(15), Stamp::new(1000)));
    }

    #[test]
    fn bitmap_outside_window_is_rejected() {
        let mut state = PeerState::new(Seq::new(1024), Stamp::new(1000));
        // advance max to 2048; seq 1 is 2047 behind, outside window
        assert!(state.accept(Seq::new(2048), Stamp::new(1000)));
        assert!(!state.accept(Seq::new(1), Stamp::new(1000)));
    }

    #[test]
    fn bitmap_large_jump_clears_window() {
        let mut state = PeerState::new(Seq::new(10), Stamp::new(1000));
        // Jump so big the whole window rolls over
        assert!(state.accept(Seq::new(10 + WINDOW_SIZE + 5), Stamp::new(1000)));
        // seq 10 is now (WINDOW_SIZE + 5) behind, outside the window
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
        // All previously accepted seqs rejected on replay
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

    /// Helper: call check_and_record_at with a caller-supplied `now`.
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

    /// A disabled filter (unauthenticated mode) accepts everything unconditionally, including a
    /// stamp that would fail the freshness window on an enabled filter — it never even reaches
    /// that check.
    #[test]
    fn disabled_filter_always_accepts() {
        let filter = ReplayFilter::new(FRESHNESS_WINDOW_DEFAULT, false);
        let peer: IpAddr = "127.0.0.9".parse().unwrap();
        assert!(filter.check_and_record(peer, Seq::NONE, Stamp::NONE));
        // A stamp that would fail freshness on an enabled filter must still be accepted.
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
        // Stamp 10 minutes in the past — beyond the 5-minute window.
        let old_stamp = phys_now_ms().saturating_sub(10 * 60 * 1000 + 1);
        assert!(!filter.check_and_record(peer, Seq::new(1), Stamp::new(old_stamp)));
    }

    #[test]
    fn far_future_stamp_rejected() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.4".parse().unwrap();
        // Stamp 10 minutes in the future.
        let future_stamp = phys_now_ms() + 10 * 60 * 1000 + 1;
        assert!(!filter.check_and_record(peer, Seq::new(1), Stamp::new(future_stamp)));
    }

    #[test]
    fn out_of_order_within_window_accepted_once() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.5".parse().unwrap();
        let now = phys_now_ms();
        // Accept seq 5 first, then 3 (out of order but within window).
        assert!(filter.check_and_record(peer, Seq::new(5), Stamp::new(now)));
        assert!(filter.check_and_record(peer, Seq::new(3), Stamp::new(now)));
        // Both must be rejected on replay.
        assert!(!filter.check_and_record(peer, Seq::new(5), Stamp::new(now)));
        assert!(!filter.check_and_record(peer, Seq::new(3), Stamp::new(now)));
    }

    #[test]
    fn seq_regression_outside_window_with_newer_stamp_accepted_as_restart() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.6".parse().unwrap();
        let now = phys_now_ms();
        // Advance to a seq well beyond WINDOW_SIZE so a restart lands outside the bitmap.
        let high_seq = WINDOW_SIZE + 100;
        assert!(filter.check_and_record(peer, Seq::new(high_seq), Stamp::new(now)));
        // Simulated restart: seq resets to 1 (outside the backward window) with a newer stamp.
        let newer = now + 1000;
        assert!(
            filter.check_and_record(peer, Seq::new(1), Stamp::new(newer)),
            "a seq regression outside the window with a strictly newer stamp must be accepted as a restart"
        );
        // After reset the new state has max_seq=1, stamp_at_max=newer.
        // A further seq=2 with the new stamp must be accepted normally.
        assert!(filter.check_and_record(peer, Seq::new(2), Stamp::new(newer)));
        // Replaying seq=1 with the new stamp is rejected (already seen after restart).
        assert!(!filter.check_and_record(peer, Seq::new(1), Stamp::new(newer)));
    }

    #[test]
    fn seq_regression_outside_window_with_old_stamp_rejected_as_replay() {
        let filter = filter_5min();
        let peer: IpAddr = "127.0.0.7".parse().unwrap();
        let now = phys_now_ms();
        let high_seq = WINDOW_SIZE + 100;
        assert!(filter.check_and_record(peer, Seq::new(high_seq), Stamp::new(now)));
        // Lower seq outside the window with the SAME stamp: not a restart, must be rejected.
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
        // After eviction the peer state is gone; any new datagram is accepted.
        filter.evict(peer);
        assert!(filter.check_and_record(peer, Seq::new(1), Stamp::new(now)));
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
                filter.check_and_record(peer, Seq::new(seq), Stamp::new(now)),
                "seq {seq} should be accepted"
            );
        }

        // Restart: seq resets to 1 with a strictly newer stamp. This is INSIDE the window
        // (behind = 4, which is < WINDOW_SIZE = 1024), so the old code would check the bitmap
        // and reject it as a duplicate. The new code checks stamp FIRST.
        let newer = now + 1000;
        assert!(
            filter.check_and_record(peer, Seq::new(1), Stamp::new(newer)),
            "seq regression inside the window with strictly newer stamp must be accepted as restart"
        );

        // After reset: max_seq=1, stamp_at_max=newer. seq=2 with newer stamp is accepted.
        assert!(filter.check_and_record(peer, Seq::new(2), Stamp::new(newer)));

        // A replay of the old (seq=1, old stamp) pair must be rejected — stamp is not strictly
        // newer than stamp_at_max (now < newer), so restart check fails, then bitmap check:
        // bit 0 is set (seq=1 was the reset), so rejected as duplicate.
        assert!(
            !filter.check_and_record(peer, Seq::new(1), Stamp::new(now)),
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

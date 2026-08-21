// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::*;

// ── SlidingBitmap (direct) ─────────────────────────────────────────────────
//
// `PeerState::accept`'s existing tests only ever advance by small deltas (< 64), so
// `word_shift` (`delta / 64`) is always 0 there and the array-indexing arithmetic in
// `SlidingBitmap::advance`'s multi-word shift is never exercised for `word_shift > 0`. These two
// tests advance across whole words (word-aligned and non-word-aligned) and check every offset in
// the window against the shift-by-`delta` property directly.

/// After `advance(delta)`, offset `j` is marked iff `j == 0` (the new high-water mark, always
/// (re-)marked) or `j >= delta` and `j - delta` was marked before the shift.
fn assert_bitmap_shifted_by(
    bitmap: &super::bitmap::SlidingBitmap,
    marked_before: &[u64],
    delta: u64,
) {
    for offset in 0..WINDOW_SIZE {
        let expected =
            offset == 0 || (offset >= delta && marked_before.contains(&(offset - delta)));
        assert_eq!(
            bitmap.is_marked(offset),
            expected,
            "offset {offset} marked-state mismatch after advance({delta})"
        );
    }
}

#[test]
fn sliding_bitmap_word_aligned_advance_shifts_the_whole_window() {
    let mut bitmap = super::bitmap::SlidingBitmap::new();
    let marked_before = [0u64, 1, 63, 64, 65, 127, 200, 500, 999, 1023];
    for &offset in &marked_before {
        bitmap.mark(offset);
    }
    let delta = 128; // word_shift = 2, bit_shift = 0: exercises the word-aligned branch.
    bitmap.advance(delta);
    assert_bitmap_shifted_by(&bitmap, &marked_before, delta);
}

#[test]
fn sliding_bitmap_non_word_aligned_advance_shifts_the_whole_window() {
    let mut bitmap = super::bitmap::SlidingBitmap::new();
    let marked_before = [0u64, 1, 63, 64, 65, 127, 200, 500, 999, 1023];
    for &offset in &marked_before {
        bitmap.mark(offset);
    }
    let delta = 130; // word_shift = 2, bit_shift = 2: exercises the cross-word-boundary branch.
    bitmap.advance(delta);
    assert_bitmap_shifted_by(&bitmap, &marked_before, delta);
}

// ── Seq/Stamp Display ────────────────────────────────────────────────────

/// `Display` forwards to the wrapped `u64`'s own `Display` — not a literal this
/// implementation happens to produce today, but the type's one job (it exists so log/error
/// output shows the sequence number instead of a `Seq(..)` debug tuple).
#[test]
fn seq_display_matches_wrapped_value() {
    for value in [0u64, 1, 42, 1_000_000, u64::MAX] {
        assert_eq!(format!("{}", Seq::new(value)), value.to_string());
    }
}

/// Same property as `seq_display_matches_wrapped_value`, for `Stamp`.
#[test]
fn stamp_display_matches_wrapped_value() {
    for value in [0u64, 1, 1_700_000_000_000, u64::MAX] {
        assert_eq!(format!("{}", Stamp::new(value)), value.to_string());
    }
}

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

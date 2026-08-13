// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Centralized observability helpers: the one place the `#[cfg(feature = "metrics")]` gate lives.
//! Each helper is an `#[inline]` no-op when the feature is off.
//!
//! Metric names use a flat `reconcile_` prefix:
//!
//! | Metric | Type | Meaning |
//! |---|---|---|
//! | `reconcile_inserts_total` | counter | local key insertions |
//! | `reconcile_removes_total` | counter | local removals (tombstones created) |
//! | `reconcile_updates_received_total` | counter | updates merged from peers |
//! | `reconcile_messages_sent_total` | counter | datagrams sent |
//! | `reconcile_bytes_sent_total` | counter | wire bytes sent |
//! | `reconcile_messages_received_total` | counter | datagrams accepted |
//! | `reconcile_bytes_received_total` | counter | wire bytes received |
//! | `reconcile_send_failures_total` | counter | sends that exhausted all retries |
//! | `reconcile_values_oversized_total` | counter | single encoded messages exceeding the datagram budget, dropped on the send path — the key never converges |
//! | `reconcile_datagrams_dropped_total` | counter (`reason` label) | dropped datagrams |
//! | `reconcile_rounds_total` | counter | reconciliation rounds initiated |
//! | `reconcile_tombstone_acks_resent_total` | counter | tombstone acks resent on reconciliation rounds |
//! | `reconcile_tombstone_stamp_bounded_total` | counter (`outcome` label) | tombstones whose expiry instant had to be bounded because the stored stamp led local time by more than the drift budget |
//! | `reconcile_round_duration_seconds` | histogram | `start_reconciliation` wall time |
//! | `reconcile_handle_messages_duration_seconds` | histogram | `handle_messages` wall time |

#[cfg(feature = "metrics")]
mod imp {
    use std::time::Instant;

    use metrics::{counter, histogram};

    pub(crate) const INSERTS_TOTAL: &str = "reconcile_inserts_total";
    pub(crate) const REMOVES_TOTAL: &str = "reconcile_removes_total";
    pub(crate) const UPDATES_RECEIVED_TOTAL: &str = "reconcile_updates_received_total";
    pub(crate) const MESSAGES_SENT_TOTAL: &str = "reconcile_messages_sent_total";
    pub(crate) const BYTES_SENT_TOTAL: &str = "reconcile_bytes_sent_total";
    pub(crate) const MESSAGES_RECEIVED_TOTAL: &str = "reconcile_messages_received_total";
    pub(crate) const BYTES_RECEIVED_TOTAL: &str = "reconcile_bytes_received_total";
    pub(crate) const SEND_FAILURES_TOTAL: &str = "reconcile_send_failures_total";
    pub(crate) const VALUES_OVERSIZED_TOTAL: &str = "reconcile_values_oversized_total";
    pub(crate) const DATAGRAMS_DROPPED_TOTAL: &str = "reconcile_datagrams_dropped_total";
    pub(crate) const ROUNDS_TOTAL: &str = "reconcile_rounds_total";
    pub(crate) const TOMBSTONE_ACKS_RESENT_TOTAL: &str = "reconcile_tombstone_acks_resent_total";
    pub(crate) const TOMBSTONE_STAMP_BOUNDED_TOTAL: &str =
        "reconcile_tombstone_stamp_bounded_total";
    pub(crate) const ROUND_DURATION_SECONDS: &str = "reconcile_round_duration_seconds";
    pub(crate) const HANDLE_DURATION_SECONDS: &str = "reconcile_handle_messages_duration_seconds";

    /// Start a latency-histogram timer; `None` when the `metrics` feature is off.
    #[inline]
    pub(crate) fn timer() -> Option<Instant> {
        Some(Instant::now())
    }

    #[inline]
    pub(crate) fn record_insert() {
        counter!(INSERTS_TOTAL).increment(1);
    }

    #[inline]
    pub(crate) fn record_remove() {
        counter!(REMOVES_TOTAL).increment(1);
    }

    #[inline]
    pub(crate) fn record_updates_received(n: usize) {
        counter!(UPDATES_RECEIVED_TOTAL).increment(n as u64);
    }

    #[inline]
    pub(crate) fn record_bytes_sent(bytes: usize) {
        counter!(MESSAGES_SENT_TOTAL).increment(1);
        counter!(BYTES_SENT_TOTAL).increment(bytes as u64);
    }

    #[inline]
    pub(crate) fn record_bytes_received(bytes: usize) {
        counter!(MESSAGES_RECEIVED_TOTAL).increment(1);
        counter!(BYTES_RECEIVED_TOTAL).increment(bytes as u64);
    }

    #[inline]
    pub(crate) fn record_send_failure() {
        counter!(SEND_FAILURES_TOTAL).increment(1);
    }

    /// A single message's encoded size exceeds the datagram budget on its own — dropped, never
    /// sent. Distinct from [`record_send_failure`] (a transport-level failure to send a
    /// well-formed datagram): this is a structurally undeliverable message, alertable on its own.
    #[inline]
    pub(crate) fn record_value_oversized() {
        counter!(VALUES_OVERSIZED_TOTAL).increment(1);
    }

    #[inline]
    pub(crate) fn record_datagram_dropped(reason: &'static str) {
        counter!(DATAGRAMS_DROPPED_TOTAL, "reason" => reason).increment(1);
    }

    #[inline]
    pub(crate) fn record_reconcile_round() {
        counter!(ROUNDS_TOTAL).increment(1);
    }

    #[inline]
    pub(crate) fn record_tombstone_acks_resent(n: usize) {
        counter!(TOMBSTONE_ACKS_RESENT_TOTAL).increment(n as u64);
    }

    /// A tombstone's expiry instant had to be bounded. A non-zero rate means a peer is planting
    /// stamps far ahead of this node's clock.
    #[inline]
    pub(crate) fn record_tombstone_stamp_bounded(outcome: &'static str) {
        counter!(TOMBSTONE_STAMP_BOUNDED_TOTAL, "outcome" => outcome).increment(1);
    }

    #[inline]
    pub(crate) fn record_round_duration(start: Option<Instant>) {
        if let Some(start) = start {
            histogram!(ROUND_DURATION_SECONDS).record(start.elapsed().as_secs_f64());
        }
    }

    #[inline]
    pub(crate) fn record_handle_duration(start: Option<Instant>) {
        if let Some(start) = start {
            histogram!(HANDLE_DURATION_SECONDS).record(start.elapsed().as_secs_f64());
        }
    }

    /// Register descriptions and units for all metrics. Idempotent; call after installing a
    /// recorder.
    #[cfg(feature = "metrics-prometheus")]
    pub(crate) fn describe() {
        use metrics::{describe_counter, describe_histogram, Unit};

        describe_counter!(INSERTS_TOTAL, Unit::Count, "Local key insertions");
        describe_counter!(
            REMOVES_TOTAL,
            Unit::Count,
            "Local removals (tombstones created)"
        );
        describe_counter!(
            UPDATES_RECEIVED_TOTAL,
            Unit::Count,
            "Updates merged from peers"
        );
        describe_counter!(MESSAGES_SENT_TOTAL, Unit::Count, "Datagrams sent");
        describe_counter!(BYTES_SENT_TOTAL, Unit::Bytes, "Wire bytes sent");
        describe_counter!(MESSAGES_RECEIVED_TOTAL, Unit::Count, "Datagrams accepted");
        describe_counter!(BYTES_RECEIVED_TOTAL, Unit::Bytes, "Wire bytes received");
        describe_counter!(
            SEND_FAILURES_TOTAL,
            Unit::Count,
            "Sends that exhausted all retries"
        );
        describe_counter!(
            VALUES_OVERSIZED_TOTAL,
            Unit::Count,
            "Single encoded messages exceeding the datagram budget, dropped on the send path"
        );
        describe_counter!(
            DATAGRAMS_DROPPED_TOTAL,
            Unit::Count,
            "Datagrams dropped, by reason"
        );
        describe_counter!(ROUNDS_TOTAL, Unit::Count, "Reconciliation rounds initiated");
        describe_counter!(
            TOMBSTONE_ACKS_RESENT_TOTAL,
            Unit::Count,
            "Tombstone acks resent on reconciliation rounds"
        );
        describe_counter!(
            TOMBSTONE_STAMP_BOUNDED_TOTAL,
            Unit::Count,
            "Tombstones whose expiry instant had to be bounded, by outcome"
        );
        describe_histogram!(
            ROUND_DURATION_SECONDS,
            Unit::Seconds,
            "Duration of start_reconciliation"
        );
        describe_histogram!(
            HANDLE_DURATION_SECONDS,
            Unit::Seconds,
            "Duration of handle_messages"
        );
    }
}

#[cfg(not(feature = "metrics"))]
mod imp {
    use std::time::Instant;

    #[inline(always)]
    pub(crate) fn timer() -> Option<Instant> {
        None
    }

    #[inline(always)]
    pub(crate) fn record_insert() {}

    #[inline(always)]
    pub(crate) fn record_remove() {}

    #[inline(always)]
    pub(crate) fn record_updates_received(_n: usize) {}

    #[inline(always)]
    pub(crate) fn record_bytes_sent(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_bytes_received(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_send_failure() {}

    #[inline(always)]
    pub(crate) fn record_value_oversized() {}

    #[inline(always)]
    pub(crate) fn record_datagram_dropped(_reason: &'static str) {}

    #[inline(always)]
    pub(crate) fn record_reconcile_round() {}

    #[inline(always)]
    pub(crate) fn record_tombstone_acks_resent(_n: usize) {}

    #[inline(always)]
    pub(crate) fn record_tombstone_stamp_bounded(_outcome: &'static str) {}

    #[inline(always)]
    pub(crate) fn record_round_duration(_start: Option<Instant>) {}

    #[inline(always)]
    pub(crate) fn record_handle_duration(_start: Option<Instant>) {}
}

pub(crate) use imp::*;

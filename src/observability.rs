// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Centralized observability helpers.
//!
//! Every metric emission in the crate goes through one of the helpers defined here, so that
//! the `#[cfg(feature = "metrics")]` gate lives in exactly one place and hot-path call sites
//! stay clean and unconditional. When the `metrics` feature is disabled, each helper compiles
//! to an `#[inline]` no-op, so the default build keeps its lean dependency footprint and the
//! call sites are optimized away.
//!
//! Metric names use a flat `reconcile_` prefix, following Prometheus conventions:
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
//! | `reconcile_datagrams_dropped_total` | counter (`reason` label) | dropped datagrams |
//! | `reconcile_rounds_total` | counter | reconciliation rounds initiated |
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
    pub(crate) const DATAGRAMS_DROPPED_TOTAL: &str = "reconcile_datagrams_dropped_total";
    pub(crate) const ROUNDS_TOTAL: &str = "reconcile_rounds_total";
    pub(crate) const ROUND_DURATION_SECONDS: &str = "reconcile_round_duration_seconds";
    pub(crate) const HANDLE_DURATION_SECONDS: &str = "reconcile_handle_messages_duration_seconds";

    /// Start a timer for a latency histogram. Returns `None` (and costs nothing) when the
    /// `metrics` feature is disabled, so callers can keep the timer unconditional.
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

    #[inline]
    pub(crate) fn record_datagram_dropped(reason: &'static str) {
        counter!(DATAGRAMS_DROPPED_TOTAL, "reason" => reason).increment(1);
    }

    #[inline]
    pub(crate) fn record_reconcile_round() {
        counter!(ROUNDS_TOTAL).increment(1);
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

    /// Register human-readable descriptions and units for all metrics. Safe to call more than
    /// once; intended to be invoked right after a recorder is installed (see [`crate::prometheus`]).
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
            DATAGRAMS_DROPPED_TOTAL,
            Unit::Count,
            "Datagrams dropped, by reason"
        );
        describe_counter!(ROUNDS_TOTAL, Unit::Count, "Reconciliation rounds initiated");
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
    pub(crate) fn record_datagram_dropped(_reason: &'static str) {}

    #[inline(always)]
    pub(crate) fn record_reconcile_round() {}

    #[inline(always)]
    pub(crate) fn record_round_duration(_start: Option<Instant>) {}

    #[inline(always)]
    pub(crate) fn record_handle_duration(_start: Option<Instant>) {}
}

pub(crate) use imp::*;

// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Protocol-level cost of one full RBSR reconciliation: how many messages, how many advertised
//! ranges, and how many wire bytes two peers exchange to resolve a difference of size `d` in a
//! store of size `n`.
//!
//! This target exists because the two published cost bounds for range-based set reconciliation —
//! `O(d log n)` communication and `O(log n)` sequential rounds — are stated for the **fixed**
//! branching factor `b` of Algorithm 2 in E. G. Amparore, *RBSR via Range-Summarizable
//! Order-Statistics Stores* (arXiv:2603.19820), and this crate does not use a fixed `b`:
//! `rbsr::protocol_round` cuts at `step = ⌊√m⌋`. Neither bound therefore describes what actually
//! goes on the wire here, and neither had ever been measured. See the deviation note on
//! `protocol_round` and `SOTA.md` §2.2; the open question is
//! [#257](https://github.com/Akvize/reconcile-rs/issues/257).
//!
//! Unlike `bench` (structure micro-benchmarks) and `system` (end-to-end over `ReplicatedMap`), this
//! target drives the protocol driver directly, through the same two crates a downstream consumer
//! would use without the facade: `rsos` for the store and `rbsr` for the round. It needs no feature
//! gate and no runtime — a reconciliation is a pure function of two stores.
//!
//! Reproduction and interpretation: `benches/README.md`. Not run in CI (only compile-checked); run
//! locally with `cargo bench --bench protocol`.

use std::hint::black_box;

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
};

use rbsr::{initial_ranges, protocol_round, EnumerationRange, RangeAggregate};
use rsos::FingerprintTreeMap;

/// Store sizes swept by the cost report (log scale). Capped at 10⁶: the point is the growth rate of
/// the exchanged volume, and two 10⁷-entry trees would dominate the benchmark's own runtime with
/// setup rather than measurement.
const SIZES: &[usize] = &[1_000, 10_000, 100_000, 1_000_000];

/// Difference sizes swept against each store size. `d = 1` is the case the bounds are usually
/// quoted for (and the one a single dropped datagram produces); `d = 10` checks that the dominant
/// term really is `n`-driven rather than `d`-driven.
const DIFFS: &[usize] = &[1, 10];

/// Build a store of `n` sequential entries, omitting `missing`.
///
/// Sequential `u64` keys rather than random ones: the measured quantity is the *shape* of the
/// refinement (how many ranges each round advertises), which depends on rank positions, not on key
/// distribution — and sequential keys keep the corpus reproducible without carrying a PRNG.
fn store(n: usize, missing: &[u64]) -> FingerprintTreeMap<u64, u64> {
    let mut map = FingerprintTreeMap::new();
    for key in 0..n as u64 {
        if !missing.contains(&key) {
            map.insert(key, key.wrapping_mul(2_654_435_761));
        }
    }
    map
}

/// The `d` keys withheld from the second store, spread evenly so the differences land in distinct
/// subranges rather than clustering into one leaf.
fn missing_keys(n: usize, d: usize) -> Vec<u64> {
    (1..=d as u64)
        .map(|i| (n as u64 / (d as u64 + 1)) * i)
        .collect()
}

/// What one full reconciliation cost, counted rather than timed.
#[derive(Debug, Default)]
struct Cost {
    /// One-way protocol messages, i.e. how many times a batch of active ranges crossed the wire.
    /// Halve it for a round-trip count.
    messages: usize,
    /// Total `RangeAggregate`s advertised across every message.
    ranges: usize,
    /// Total bincode-encoded bytes of those aggregates — the same encoder the real transport uses,
    /// so this is the payload byte count before authentication framing.
    bytes: usize,
    /// The largest single message, in ranges, and the bytes those ranges encode to. This is the
    /// quantity that has to fit in one datagram: `send_messages_paced` splits at
    /// `BUFFER_SIZE - authenticator overhead` (65 507 B minus framing), so exceeding it costs an
    /// extra datagram rather than failing — but a message near the ceiling is still ~44 IP
    /// fragments, any one of which loses the whole round.
    largest_message: usize,
    largest_message_bytes: usize,
    /// Ranges handed back for explicit enumeration (the paper's IDLIST outcome).
    enumerations: usize,
}

/// Drive both peers to convergence, counting what crosses the wire.
///
/// The loop is the protocol's own termination argument: a batch of active ranges is answered by one
/// peer, whose SPLIT children become the next batch for the other peer, until no range is left
/// active. Every mismatching range is either resolved or strictly refined, so this terminates; the
/// guard is a bug net, not a bound.
fn reconcile(a: &FingerprintTreeMap<u64, u64>, b: &FingerprintTreeMap<u64, u64>) -> Cost {
    let mut cost = Cost::default();
    let mut active: Vec<RangeAggregate<u64>> = initial_ranges(a);
    // `initial_ranges` came from `a`, so `b` answers first, and the responder alternates from there.
    let mut responder_is_b = true;
    let mut encoded = Vec::new();

    while !active.is_empty() {
        encoded.clear();
        for segment in &active {
            gossip::bincode::encode(segment, &mut encoded)
                .expect("encoding a RangeAggregate into an in-memory buffer cannot fail");
        }
        cost.messages += 1;
        cost.ranges += active.len();
        cost.bytes += encoded.len();
        if active.len() > cost.largest_message {
            cost.largest_message = active.len();
            cost.largest_message_bytes = encoded.len();
        }

        let mut children = Vec::new();
        let mut enumerations: Vec<EnumerationRange<u64>> = Vec::new();
        let responder = if responder_is_b { b } else { a };
        protocol_round(responder, active, &mut children, &mut enumerations);
        cost.enumerations += enumerations.len();

        active = children;
        responder_is_b = !responder_is_b;
        assert!(
            cost.messages < 1_000,
            "reconciliation failed to converge — the refinement is not shrinking"
        );
    }
    cost
}

/// Deterministic report of exchanged volume, plus the timed local cost of driving the run.
///
/// The counts are printed rather than timed (like `system`'s `memory_footprint` and the traffic half
/// of its `gossip_fanout`): for a given `(n, d)` they are exact and reproducible, not a statistic.
/// What *is* worth timing sits alongside them — the responder-side local work the paper models as
/// `T_loc`, here the whole drive loop for both peers.
fn reconciliation_cost(c: &mut Criterion) {
    println!(
        "[protocol] full reconciliation, u64 keys. Published RBSR bounds assume a fixed fan-out b; \
         this crate cuts at floor(sqrt(m)) — see #257."
    );
    for &d in DIFFS {
        for &n in SIZES {
            let cost = reconcile(&store(n, &[]), &store(n, &missing_keys(n, d)));
            println!(
                "[protocol] n={n:>9} d={d:>3}: {bytes:>8} B over {ranges:>5} ranges in \
                 {messages} one-way messages (largest {largest} ranges = {largest_bytes} B, \
                 {enumerations} idlists) -> {per_diff:>8} B per differing element",
                bytes = cost.bytes,
                ranges = cost.ranges,
                messages = cost.messages,
                largest = cost.largest_message,
                largest_bytes = cost.largest_message_bytes,
                enumerations = cost.enumerations,
                per_diff = cost.bytes / d,
            );
        }
    }

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("reconciliation_drive");
    group.plot_config(plot_config);
    for &n in SIZES {
        let full = store(n, &[]);
        let holed = store(n, &missing_keys(n, 1));
        group.sample_size(10.max(1_000_000 / n).min(100));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |bencher, _| {
            bencher.iter(|| reconcile(black_box(&full), black_box(&holed)));
        });
    }
    group.finish();
}

criterion_group!(benches, reconciliation_cost);
criterion_main!(benches);

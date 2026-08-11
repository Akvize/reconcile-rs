// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Protocol-level cost of one full RBSR reconciliation, **per refinement policy**: how many
//! messages, advertised ranges, wire bytes, datagrams and local RSOS queries two peers spend to
//! resolve a difference of size `d` in a store of size `n`, and how that changes when the
//! differences cluster instead of scattering.
//!
//! This target exists because the two published cost bounds for range-based set reconciliation —
//! `O(d log n)` communication and `O(log n)` sequential rounds — are stated for the **fixed**
//! branching factor `b` of Algorithm 2 in E. G. Amparore, *RBSR via Range-Summarizable
//! Order-Statistics Stores* (arXiv:2603.19820), and until 2026-08 this crate's default did not use
//! a fixed `b`: `rbsr::SqrtFanOut` cuts every `⌊√m⌋` elements, so neither bound described what
//! actually went on the wire. This target is what changed that. Since
//! [#257](https://github.com/Akvize/reconcile-rs/issues/257) made the policy a swappable seam, the
//! alternatives could be priced against each other rather than estimated, and the default is now
//! `rbsr::FixedFanOut` at `b = 16` — chosen here, in the sweep below, not inherited from
//! Negentropy. `rbsr::EnumerateBelowThreshold` is Algorithm 1 as written, with both parameters.
//!
//! **Read the two traffic columns together.** A policy that splits less advertises fewer ranges but
//! reaches its IDLIST cutoff on wider ranges, and every enumerated element is a *value* on the wire
//! — almost all of them elements the peer already holds. Reporting advertised ranges without
//! enumerated elements would make a large enumeration threshold look free. The third column, one-way
//! messages, is the round-trip count, and every benchmark in this repository runs at RTT ≈ 0
//! ([#280](https://github.com/Akvize/reconcile-rs/issues/280)) — so this target prices bytes
//! correctly and round-trips at zero, which is exactly the axis that separates `√m` from a fixed
//! `b`. Weigh the message column by your own RTT before drawing a conclusion from the byte column.
//!
//! Unlike `bench` (structure micro-benchmarks) and `system` (end-to-end over `ReplicatedMap`), this
//! target drives the protocol driver directly, through the same two crates a downstream consumer
//! would use without the facade: `rsos` for the store and `rbsr` for the round. It needs no feature
//! gate and no runtime — a reconciliation is a pure function of two stores.
//!
//! Reproduction and interpretation: `benches/README.md`. Not run in CI (only compile-checked); run
//! locally with `cargo bench --bench protocol`.

use std::cell::Cell;
use std::hint::black_box;
use std::ops::{Add, RangeBounds};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
};

use rbsr::{
    initial_ranges, protocol_round_with_policy, EnumerateBelowThreshold, EnumerationRange, FanOut,
    FixedFanOut, RangeAggregate, RefinementPolicy, SqrtFanOut,
};
use rsos::{Aggregate, FingerprintTreeMap, Rsos};

/// Store sizes swept by the cost report (log scale). Capped at 10⁶: the point is the growth rate of
/// the exchanged volume, and two 10⁷-entry trees would dominate the benchmark's own runtime with
/// setup rather than measurement.
const SIZES: &[usize] = &[1_000, 10_000, 100_000, 1_000_000];

/// The payload one datagram can carry: the IPv4 datagram ceiling `Replica` targets. A keyed
/// deployment subtracts the authenticator's per-datagram overhead
/// (`send_messages_paced` splits at `BUFFER_SIZE - authenticator.overhead()`), so this is the
/// most optimistic split point.
const MAX_DATAGRAM_PAYLOAD: usize = 65_507;

/// The payload one *IP fragment* can carry on a 1500-byte-MTU path: 1500 less the 20-byte IPv4 and
/// 8-byte UDP headers. Approximate — only the first fragment pays the UDP header — and deliberately
/// so: what matters is the order of magnitude, because losing any one fragment loses the whole
/// datagram.
const MTU_FRAGMENT_PAYLOAD: usize = 1_472;

/// How the `d` differing keys are laid out across the key space — the axis a benchmark that varies
/// only `n` and `d` never sees, and the one where a `√m` fan-out and a fixed `b` behave most
/// differently: a scattered difference forces every subtree to be refined, a clustered one is
/// confined to a single descent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Clustering {
    /// Spread evenly, so the differences land in distinct subranges.
    Scattered,
    /// One contiguous block in the middle of the key space.
    Clustered,
}

impl Clustering {
    fn label(self) -> &'static str {
        match self {
            Clustering::Scattered => "scattered",
            Clustering::Clustered => "clustered",
        }
    }
}

/// The `(difference size, layout)` pairs swept against every store size.
///
/// `d = 1` is the case the published bounds are usually quoted for, and the one a single dropped
/// datagram produces; it appears only as `Scattered` because a single key has no layout. The two
/// larger sizes appear both ways, which is what makes the clustering axis visible.
const DIFFERENCES: &[(usize, Clustering)] = &[
    (1, Clustering::Scattered),
    (10, Clustering::Scattered),
    (10, Clustering::Clustered),
    (100, Clustering::Scattered),
    (100, Clustering::Clustered),
];

/// Branching factors swept by `fan_out_sweep`, doubling from the smallest that refines to well past
/// anything a datagram can hold.
///
/// The interesting range is bounded on both ends by a real constraint rather than by taste. Below,
/// `b = 2` is the minimum (a 1-partition is the identity). Above, the total ranges a `d = 1` descent
/// advertises grow as `b / ln b` while the rounds shrink as `log_b n`, so bytes never explode with
/// `b` — what does grow, linearly, is the *widest single round*, and that is what has to fit a
/// datagram. The sweep runs to 256 so the ceiling is visible in the table rather than argued about.
const FAN_OUTS: &[usize] = &[2, 4, 8, 16, 32, 64, 128, 256];

/// `(store size, difference sizes)` the branching-factor sweep runs at, grouped by store size so the
/// complete store is built once per group.
///
/// Small `n` only needs `d = 1`: the trade `b` governs is between rounds and ranges-per-round, and
/// at 10³ there are too few levels for it to show. The two large sizes carry the `d` axis, because
/// the whole point of choosing `b` is the regime this crate actually runs in.
const SWEEP_CASES: &[(usize, &[usize])] = &[
    (1_000, &[1]),
    (10_000, &[1]),
    (100_000, &[1, 10, 100]),
    (1_000_000, &[1, 10, 100]),
];

/// The policies compared: the shipped default, the rule it replaced, and Algorithm 1 as written.
/// Keeping the retired rule in the comparison is deliberate — it is what the numbers below are a
/// verdict on, and it is still shipped.
fn policies() -> Vec<(&'static str, Box<dyn RefinementPolicy>)> {
    vec![
        ("sqrt (pre-2026-08)", Box::new(SqrtFanOut)),
        (
            "fixed b=16 (default)",
            Box::new(FixedFanOut::new(FanOut::NEGENTROPY)),
        ),
        (
            "paper t=32 b=16",
            Box::new(EnumerateBelowThreshold::new(32, FanOut::NEGENTROPY)),
        ),
    ]
}

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

/// The `d` keys withheld from the second store, laid out according to `clustering`.
fn missing_keys(n: usize, d: usize, clustering: Clustering) -> Vec<u64> {
    match clustering {
        Clustering::Scattered => (1..=d as u64)
            .map(|i| (n as u64 / (d as u64 + 1)) * i)
            .collect(),
        // Centred so the block is not adjacent to either end of the key space, where a partition's
        // outermost child would absorb it for free.
        Clustering::Clustered => {
            let start = (n / 2 - d / 2) as u64;
            (start..start + d as u64).collect()
        }
    }
}

/// The RSOS queries a reconciliation performed — the paper's local-cost model `T_loc` in counts
/// rather than seconds, and the half of a policy's cost that never appears on the wire.
#[derive(Clone, Copy, Debug, Default)]
struct Queries {
    aggregate: usize,
    rank: usize,
    select: usize,
}

impl Add for Queries {
    type Output = Queries;

    fn add(self, other: Queries) -> Queries {
        Queries {
            aggregate: self.aggregate + other.aggregate,
            rank: self.rank + other.rank,
            select: self.select + other.select,
        }
    }
}

/// A read-only RSOS that tallies the three query kinds the protocol driver performs.
///
/// It implements `rsos::Rsos` rather than `rbsr::RsosView` directly, which is not a stylistic
/// choice: `rbsr` blanket-implements `RsosView` for every `Rsos`, so a second, more specific
/// `RsosView` impl in this crate would be a coherence conflict. Wrapping at the `Rsos` level gets
/// the `RsosView` impl for free.
struct Counting<'a, S> {
    inner: &'a S,
    aggregate: Cell<usize>,
    rank: Cell<usize>,
    select: Cell<usize>,
}

impl<'a, S> Counting<'a, S> {
    fn new(inner: &'a S) -> Counting<'a, S> {
        Counting {
            inner,
            aggregate: Cell::new(0),
            rank: Cell::new(0),
            select: Cell::new(0),
        }
    }

    fn queries(&self) -> Queries {
        Queries {
            aggregate: self.aggregate.get(),
            rank: self.rank.get(),
            select: self.select.get(),
        }
    }
}

impl<K, S: Rsos<K>> Rsos<K> for Counting<'_, S> {
    type Value = S::Value;

    fn size(&self) -> usize {
        self.inner.size()
    }

    fn aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate {
        self.aggregate.set(self.aggregate.get() + 1);
        self.inner.aggregate(range)
    }

    fn rank(&self, z: &K) -> usize {
        self.rank.set(self.rank.get() + 1);
        self.inner.rank(z)
    }

    fn select(&self, r: usize) -> &K {
        self.select.set(self.select.get() + 1);
        self.inner.select(r)
    }

    fn enumerate<'b, R: RangeBounds<K> + 'b>(
        &'b self,
        range: R,
    ) -> impl Iterator<Item = (&'b K, &'b Self::Value)> + 'b
    where
        K: Ord + 'b,
        Self::Value: 'b,
    {
        self.inner.enumerate(range)
    }

    fn insert(&mut self, _key: K, _value: Self::Value) -> Option<Self::Value> {
        // The protocol driver reads and never writes (`RsosView` does not even name these two).
        // They exist only because `Rsos` is the seven-operation contract; wrapping a `&S` could not
        // honour them anyway.
        unreachable!("the reconciliation driver never mutates the store")
    }

    fn delete(&mut self, _key: &K) -> Option<Self::Value> {
        unreachable!("the reconciliation driver never mutates the store")
    }
}

/// What one full reconciliation cost, counted rather than timed.
#[derive(Debug, Default)]
struct Cost {
    /// One-way protocol messages, i.e. how many times a batch of active ranges crossed the wire.
    /// Halve it for a round-trip count. This is the column an RTT multiplies.
    messages: usize,
    /// Total `RangeAggregate`s advertised across every message.
    ranges: usize,
    /// Total bincode-encoded bytes of those aggregates — the same encoder the real transport uses,
    /// so this is the payload byte count before authentication framing.
    bytes: usize,
    /// Datagrams those batches become: `send_messages_paced` splits a batch at
    /// `BUFFER_SIZE - authenticator overhead`, so an oversized round costs extra datagrams rather
    /// than failing.
    datagrams: usize,
    /// IP fragments those datagrams become on a 1500-byte-MTU path. Losing any one of them loses
    /// the whole datagram, so this is the number that decides how a round survives a lossy link.
    fragments: usize,
    /// The largest single message, in ranges, and the bytes those ranges encode to.
    largest_message: usize,
    largest_message_bytes: usize,
    /// Ranges handed back for explicit enumeration (the paper's IDLIST outcome), and the elements
    /// those ranges actually contain — the *values* that follow on the wire, of which all but `d`
    /// are elements the peer already holds.
    enumerations: usize,
    enumerated_elements: usize,
    /// Local RSOS queries, summed over both peers.
    queries: Queries,
}

/// Drive both peers to convergence under `policy`, counting what crosses the wire.
///
/// The loop is the protocol's own termination argument: a batch of active ranges is answered by one
/// peer, whose SPLIT children become the next batch for the other peer, until no range is left
/// active. Every mismatching range is either resolved or strictly refined, so this terminates; the
/// guard is a bug net, not a bound.
///
/// Both peers run the *same* policy. They need not — the rule is local and never crosses the wire
/// (`rbsr`'s `peers_running_different_policies_still_converge` pins that) — but a mixed pair would
/// measure neither of the two.
fn reconcile<S>(a: &S, b: &S, policy: &dyn RefinementPolicy) -> Cost
where
    S: Rsos<u64, Value = u64>,
{
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
        cost.datagrams += encoded.len().div_ceil(MAX_DATAGRAM_PAYLOAD).max(1);
        cost.fragments += encoded.len().div_ceil(MTU_FRAGMENT_PAYLOAD).max(1);
        if active.len() > cost.largest_message {
            cost.largest_message = active.len();
            cost.largest_message_bytes = encoded.len();
        }

        let mut children = Vec::new();
        let mut enumerations: Vec<EnumerationRange<u64>> = Vec::new();
        let responder = if responder_is_b { b } else { a };
        protocol_round_with_policy(responder, policy, active, &mut children, &mut enumerations);
        cost.enumerations += enumerations.len();
        // What an IDLIST actually ships. `Enumerate(l, u)` is the paper's own operation, so this is
        // a real cost of the policy, not an artifact of how the caller drives it.
        for range in enumerations {
            cost.enumerated_elements += responder.enumerate(range).count();
        }

        active = children;
        responder_is_b = !responder_is_b;
        assert!(
            cost.messages < 100_000,
            "reconciliation failed to converge — the refinement is not shrinking"
        );
    }
    cost
}

/// Deterministic report of exchanged volume per policy, plus the timed local cost of driving the
/// run.
///
/// The counts are printed rather than timed (like `system`'s `memory_footprint` and the traffic half
/// of its `gossip_fanout`): for a given `(policy, n, d, clustering)` they are exact and reproducible,
/// not a statistic. What *is* worth timing sits alongside them — the responder-side local work the
/// paper models as `T_loc`, here the whole drive loop for both peers.
fn reconciliation_cost(c: &mut Criterion) {
    println!(
        "[protocol] full reconciliation, u64 keys. Refinement policy is a local decision (#257): \
         the wire type carries none, so these are comparable runs of the same protocol.\n\
         [protocol] columns: refinement traffic | widest round | IDLIST (ranges/elements shipped) \
         | local RSOS queries, both peers"
    );
    for &n in SIZES {
        // The complete store is the same for every corpus at this size; only the holed one varies.
        let full = store(n, &[]);
        for &(d, clustering) in DIFFERENCES {
            let holed = store(n, &missing_keys(n, d, clustering));
            println!("[protocol] n={n} d={d} {}", clustering.label());
            for (name, policy) in policies() {
                let (counted_full, counted_holed) = (Counting::new(&full), Counting::new(&holed));
                let mut cost = reconcile(&counted_full, &counted_holed, policy.as_ref());
                cost.queries = counted_full.queries() + counted_holed.queries();
                println!(
                    "[protocol]   {name:<16} {bytes:>9} B / {ranges:>6} ranges / {messages:>3} msgs \
                     / {datagrams:>3} dgrams / {fragments:>5} frags | widest {largest:>5} r \
                     = {largest_bytes:>7} B | idlist {enumerations:>4} r / {elements:>7} elem \
                     | agg {aggregate:>7} rank {rank:>6} sel {select:>6}",
                    bytes = cost.bytes,
                    ranges = cost.ranges,
                    messages = cost.messages,
                    datagrams = cost.datagrams,
                    fragments = cost.fragments,
                    largest = cost.largest_message,
                    largest_bytes = cost.largest_message_bytes,
                    enumerations = cost.enumerations,
                    elements = cost.enumerated_elements,
                    aggregate = cost.queries.aggregate,
                    rank = cost.queries.rank,
                    select = cost.queries.select,
                );
            }
        }
    }

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("reconciliation_drive");
    group.plot_config(plot_config);
    for &n in SIZES {
        let full = store(n, &[]);
        let holed = store(n, &missing_keys(n, 1, Clustering::Scattered));
        group.sample_size(10.max(1_000_000 / n).min(100));
        for (name, policy) in policies() {
            group.bench_with_input(BenchmarkId::new(name, n), &n, |bencher, _| {
                bencher.iter(|| reconcile(black_box(&full), black_box(&holed), policy.as_ref()));
            });
        }
    }
    group.finish();
}

/// Sweep the branching factor `b` on its own, so it can be chosen on evidence rather than inherited
/// from Negentropy.
///
/// Only [`FixedFanOut`] varies here — same enumeration cutoffs as the shipped default, one knob
/// moving — because the question this answers is "if the default becomes a fixed `b`, which one".
///
/// **What to read.** Bytes and rounds pull in opposite directions and neither one alone picks a `b`:
/// a `d = 1` descent visits `~log_b n` levels advertising `~b` ranges each, so ranges grow as
/// `b / ln b` (flat-ish, minimized near `b = 3`) while one-way messages fall as `log_b n`. Since
/// this workspace has no RTT lane ([#280](https://github.com/Akvize/reconcile-rs/issues/280)), the
/// message column cannot be converted into seconds here — weigh it by your own round-trip time.
/// The column that *is* a hard limit is the widest single round: it grows linearly in `b`, and it is
/// what must fit a datagram (65 507 B) and survive fragmentation (~1 472 B per IP fragment, any one
/// of which loses the whole round).
fn fan_out_sweep(c: &mut Criterion) {
    println!(
        "[sweep] FixedFanOut, branching factor only, u64 keys, differences scattered.\n\
         [sweep] bytes and rounds trade against each other; the widest round is the hard ceiling."
    );
    for &(n, diffs) in SWEEP_CASES {
        let full = store(n, &[]);
        for &d in diffs {
            let holed = store(n, &missing_keys(n, d, Clustering::Scattered));
            println!("[sweep] n={n} d={d} scattered");
            for &b in FAN_OUTS {
                let policy = FixedFanOut::new(FanOut::new(b));
                let (counted_full, counted_holed) = (Counting::new(&full), Counting::new(&holed));
                let mut cost = reconcile(&counted_full, &counted_holed, &policy);
                cost.queries = counted_full.queries() + counted_holed.queries();
                println!(
                    "[sweep]   b={b:>4} {bytes:>9} B / {ranges:>6} ranges / {messages:>3} msgs \
                     / {datagrams:>3} dgrams / {fragments:>5} frags | widest {largest:>5} r \
                     = {largest_bytes:>7} B | agg {aggregate:>7} rank {rank:>7} sel {select:>7}",
                    bytes = cost.bytes,
                    ranges = cost.ranges,
                    messages = cost.messages,
                    datagrams = cost.datagrams,
                    fragments = cost.fragments,
                    largest = cost.largest_message,
                    largest_bytes = cost.largest_message_bytes,
                    aggregate = cost.queries.aggregate,
                    rank = cost.queries.rank,
                    select = cost.queries.select,
                );
            }
        }
    }

    // The local half of the same question, timed rather than counted, at the scale the choice is
    // actually made for.
    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("fan_out_sweep_drive");
    group.plot_config(plot_config);
    group.sample_size(20);
    let full = store(1_000_000, &[]);
    let holed = store(
        1_000_000,
        &missing_keys(1_000_000, 1, Clustering::Scattered),
    );
    for &b in FAN_OUTS {
        let policy = FixedFanOut::new(FanOut::new(b));
        group.bench_with_input(BenchmarkId::from_parameter(b), &b, |bencher, _| {
            bencher.iter(|| reconcile(black_box(&full), black_box(&holed), &policy));
        });
    }
    group.finish();
}

criterion_group!(benches, reconciliation_cost, fan_out_sweep);
criterion_main!(benches);

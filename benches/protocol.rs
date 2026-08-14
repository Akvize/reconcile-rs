// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Protocol-level cost of one full RBSR reconciliation, **per refinement policy**: how many total
//! wire bytes, messages, advertised ranges, datagrams and local RSOS queries two peers spend to
//! resolve a difference of size `d` in a store of size `n`, how that changes when the differences
//! cluster instead of scattering, and how it moves with the size of a stored value.
//!
//! This target exists because the two published cost bounds for range-based set reconciliation —
//! `O(d log n)` communication and `O(log n)` sequential rounds — are stated for the **fixed**
//! branching factor `b` of Algorithm 2 in E. G. Amparore, *RBSR via Range-Summarizable
//! Order-Statistics Stores* (arXiv:2603.19820), and `rbsr` makes the fan-out a swappable
//! `RefinementPolicy` — so which bounds apply depends on which policy runs, and what each costs is
//! a measurement rather than a quotation. The default `rbsr::FixedFanOut` at `b = 16` is the
//! paper's constant; `rbsr::SqrtFanOut` cuts every `⌊√m⌋` elements, for which neither published
//! bound holds; `rbsr::EnumerateBelowThreshold` is Algorithm 1 as written, with both parameters.
//! This target prices all three, and sweeps each parameter on its own: `b` in `fan_out_sweep`, `t`
//! in `threshold_sweep`.
//!
//! **One unit: total wire bytes.** A policy that splits less advertises fewer ranges but reaches
//! its IDLIST cutoff on wider ranges, and every enumerated element is a *value* on the wire —
//! almost all of them elements the peer already holds. Refinement bytes and enumerated elements are
//! therefore two halves of one quantity, and comparing policies across the two units cannot settle
//! anything: a large enumeration threshold looks free in the first column and ruinous in the
//! second. Both halves are summed here, in bytes, at four payload sizes `V` (`VALUE_SIZES`) — the
//! axis `system`'s `memory_footprint` already varies. The breakdown is still printed under each
//! total, because it says *why* a policy lands where it does.
//!
//! One-way messages stay a separate column on purpose: no byte total prices a round trip. This
//! target runs at RTT ≈ 0, so weigh that column by your own — at the rate `benches/system.rs`'s
//! injected-RTT lane measures, one RTT per round trip with no hidden multiplier
//! (`benches/README.md`).
//!
//! **Why one drive prices every `V`.** Both peers assign the same value to the same key, so equal
//! key sets have equal aggregates whatever the payload is, and every SKIP/IDLIST/SPLIT decision
//! reads aggregates alone: the *decisions* — messages, ranges, enumerations, elements, queries —
//! are identical at every payload size, and only the per-element wire cost moves. So the drive runs
//! once, over a `u64`-valued store, and each enumerated element is priced by encoding the dated
//! cell the transport really ships for it, `(K, Entry<Timestamp, Vec<u8>>)`
//! (`src/replica.rs`'s `Message::Update`), through the transport's own encoder. That is measured
//! rather than argued: `payload_size_does_not_move_the_trace` drives the same case over a `u64`, an
//! 8-byte and a 4 KB payload and compares, on every shipped policy, before any table is printed.
//! It also buys the 4 KB column at `n = 10⁶`, which materializing 4 GB of payload twice could not.
//!
//! Both byte columns are payload before framing: neither carries the one-byte `Message` variant tag
//! the transport prepends per item, nor the authenticator's per-datagram overhead.
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
use serde::Serialize;

use lww_register::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use lww_register::Entry;
use rbsr::{
    initial_ranges, protocol_round_with_policy, EnumerateBelowThreshold, EnumerationRange, FanOut,
    FixedFanOut, RangeAggregate, RefinementPolicy, SqrtFanOut,
};
use rsos::{Aggregate, FingerprintTreeMap, Rsos};

/// Store sizes swept by the cost report (log scale). Capped at 10⁶: the point is the growth rate of
/// the exchanged volume, and two 10⁷-entry trees would dominate the benchmark's own runtime with
/// setup rather than measurement.
const SIZES: &[usize] = &[1_000, 10_000, 100_000, 1_000_000];

/// Value payload sizes every total is reported at, in bytes: `system`'s `memory_footprint` axis,
/// extended to 4 KB — past that a single value approaches the datagram ceiling (README,
/// "Value-size ceiling"). The axis exists because a policy's two halves are priced against each
/// other *through* it: refinement bytes do not move with `V`, an enumerated element does.
const VALUE_SIZES: [usize; 4] = [8, 64, 512, 4096];

/// The payload one datagram can carry: the IPv4 ceiling, the most optimistic split point — a
/// keyed deployment subtracts the authenticator's overhead.
const MAX_DATAGRAM_PAYLOAD: usize = 65_507;

/// The payload one IP fragment carries on a 1500-byte-MTU path. Approximate on purpose: losing
/// any fragment loses the datagram, so only the order of magnitude matters.
const MTU_FRAGMENT_PAYLOAD: usize = 1_472;

/// When the priced writes happened, in milliseconds since the Unix epoch (2026-08-14). A stamp's
/// two `u64`s are varints, so a zeroed clock would encode in two bytes where a real one takes
/// eighteen — pricing an enumerated element far below what it costs. Fixed, not read from the
/// clock, so the report stays reproducible.
const WRITE_INSTANT_MS: u64 = 1_786_752_000_000;

/// The identity stamping those writes, of the shape `Replica::new` mints
/// (`NodeId::new(rand::random())`) — a full-width value, again because varints make small ones
/// unrepresentative. Fixed for reproducibility.
const NODE_ID: u64 = 0xfeed_face_dead_beef;

/// How the `d` differing keys are laid out: scattered forces every subtree to refine, clustered
/// confines the work to one descent — the axis where `√m` and a fixed `b` differ most.
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

/// The `(difference size, layout)` pairs swept against every store size. `d = 1` — the published
/// bounds' usual case — has no layout, so it appears only as `Scattered`.
const DIFFERENCES: &[(usize, Clustering)] = &[
    (1, Clustering::Scattered),
    (10, Clustering::Scattered),
    (10, Clustering::Clustered),
    (100, Clustering::Scattered),
    (100, Clustering::Clustered),
];

/// Branching factors swept by `fan_out_sweep`. `b = 2` is the floor (a 1-partition is the
/// identity); the ceiling is the widest single round, which grows linearly in `b` and must fit a
/// datagram — hence sweeping to 256, so it is visible rather than argued.
const FAN_OUTS: &[usize] = &[2, 4, 8, 16, 32, 64, 128, 256];

/// Enumeration thresholds swept by `threshold_sweep`, `b` held at the default 16. `t = 1` is the
/// floor (`t = 0` is unrepresentable, and would split a range into itself forever); 256 is eight
/// doublings past it and four past the paper's 32, far enough for the amplification to be a curve
/// rather than a point.
const THRESHOLDS: &[usize] = &[1, 2, 4, 8, 16, 32, 64, 128, 256];

/// `(store size, difference sizes)` for the parameter sweeps, grouped so each store is built
/// once. Small `n` carries only `d = 1`: too few levels for the rounds-vs-ranges trade to show.
const SWEEP_CASES: &[(usize, &[usize])] = &[
    (1_000, &[1]),
    (10_000, &[1]),
    (100_000, &[1, 10, 100]),
    (1_000_000, &[1, 10, 100]),
];

/// The three shipped policies, compared head to head: the default constant `b`, the size-derived
/// `√m` fan-out, and Algorithm 1 as written.
fn policies() -> Vec<(&'static str, Box<dyn RefinementPolicy>)> {
    vec![
        ("sqrt", Box::new(SqrtFanOut)),
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

/// Build a store of `n` sequential entries, omitting `missing`, each key carrying `value(key)`.
/// Sequential keys: the measured quantity depends on rank positions, not key distribution, and
/// stays reproducible without a PRNG.
fn store_of<V: Serialize>(
    n: usize,
    missing: &[u64],
    value: impl Fn(u64) -> V,
) -> FingerprintTreeMap<u64, V> {
    let mut map = FingerprintTreeMap::new();
    for key in 0..n as u64 {
        if !missing.contains(&key) {
            map.insert(key, value(key));
        }
    }
    map
}

/// The store every table is driven over. Its values are `u64` rather than dated cells because the
/// decisions do not depend on them — see the module docs, and
/// `payload_size_does_not_move_the_trace`.
fn store(n: usize, missing: &[u64]) -> FingerprintTreeMap<u64, u64> {
    store_of(n, missing, |key| key.wrapping_mul(2_654_435_761))
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

/// The stamp the entry under `key` carries: one HLC reading per write, at a plausible instant.
fn stamp(key: u64) -> Timestamp {
    Timestamp::new(
        Hlc::new(
            PhysicalTime::from_millis(WRITE_INSTANT_MS + key),
            LogicalCounter::ZERO,
        ),
        NodeId::new(NODE_ID),
    )
}

/// The dated cell one key is stored and shipped as, at payload size `value_bytes`: the register
/// cell `ReplicatedMap` stores (`src/replica.rs`'s `FingerprintTreeMap<K, Entry<Timestamp, V>>`).
///
/// The payload is a `Vec<u8>` rather than a `[u8; V]` because that is what a deployment can
/// actually store: `lww_register::Value` demands `Serialize`, which `serde` implements for arrays
/// only up to 32 elements. It costs the wire a length varint an array would not carry — one byte up
/// to 250, three beyond — which is part of the price, not an artifact of the harness.
fn dated_cell(key: u64, value_bytes: usize) -> Entry<Timestamp, Vec<u8>> {
    Entry::present(stamp(key), vec![key as u8; value_bytes])
}

/// What one enumerated element costs on the wire, one entry per [`VALUE_SIZES`] payload size:
/// `Message::Update`'s payload (`src/replica.rs`), through the transport's own encoder.
///
/// Measured per element rather than derived from a per-entry constant — bincode's varints make the
/// key and the stamp cost what their values happen to cost — and read straight off [`VALUE_SIZES`],
/// so the reported sizes and the priced cells cannot drift apart.
fn element_bytes(key: u64, scratch: &mut Vec<u8>) -> [usize; VALUE_SIZES.len()] {
    VALUE_SIZES.map(|value_bytes| {
        scratch.clear();
        gossip::bincode::encode(&(key, dated_cell(key, value_bytes)), scratch)
            .expect("encoding an entry into an in-memory buffer cannot fail");
        scratch.len()
    })
}

/// The RSOS queries a reconciliation performed — the paper's local-cost model `T_loc` in counts
/// rather than seconds, and the half of a policy's cost that never appears on the wire.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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

/// A read-only RSOS tallying the three query kinds the driver performs.
///
/// Implements `rsos::Rsos`, not `rbsr::RsosView`: the blanket impl makes a second `RsosView` impl
/// a coherence conflict.
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
    /// Halve it for a round-trip count. This is the column an RTT multiplies, and the one no byte
    /// total absorbs.
    messages: usize,
    /// Total `RangeAggregate`s advertised across every message.
    ranges: usize,
    /// Total bincode-encoded bytes of those aggregates — the same encoder the real transport uses.
    /// The refinement half of [`total_bytes`](Cost::total_bytes); it does not move with the payload
    /// size.
    refinement_bytes: usize,
    /// Datagrams the refinement batches become: `send_messages_paced` splits a batch at
    /// `BUFFER_SIZE - authenticator overhead`, so an oversized round costs extra datagrams rather
    /// than failing. Values travel on their own paced path, so they are not counted here.
    datagrams: usize,
    /// IP fragments those datagrams become on a 1500-byte-MTU path. Losing any one of them loses
    /// the whole datagram, so this is the number that decides how a round survives a lossy link.
    fragments: usize,
    /// The largest single refinement message, in ranges, and the bytes those ranges encode to.
    largest_message: usize,
    largest_message_bytes: usize,
    /// Ranges handed back for explicit enumeration (the paper's IDLIST outcome), and the elements
    /// those ranges actually contain — of which all but `d` are elements the peer already holds.
    enumerations: usize,
    enumerated_elements: usize,
    /// What those elements cost on the wire, one entry per [`VALUE_SIZES`] payload size: the value
    /// half of [`total_bytes`](Cost::total_bytes), and the only half `V` moves.
    enumerated_bytes: [usize; VALUE_SIZES.len()],
    /// Local RSOS queries, summed over both peers.
    queries: Queries,
}

impl Cost {
    /// Everything this reconciliation put on the wire, at each [`VALUE_SIZES`] payload size: the
    /// refinement traffic plus the values the IDLIST outcomes ship. One unit, so policies compare.
    ///
    /// Meaningful only for a [`Pricing::Elements`] drive; a [`Pricing::None`] one leaves the value
    /// half at zero.
    fn total_bytes(&self) -> [usize; VALUE_SIZES.len()] {
        self.enumerated_bytes
            .map(|bytes| bytes + self.refinement_bytes)
    }

    /// What this reconciliation *decided*, as opposed to what those decisions encoded to — what
    /// [`payload_size_does_not_move_the_trace`] compares across payload types.
    fn decisions(&self) -> Decisions {
        Decisions {
            messages: self.messages,
            ranges: self.ranges,
            enumerations: self.enumerations,
            enumerated_elements: self.enumerated_elements,
            queries: self.queries,
        }
    }
}

/// The payload-independent half of a [`Cost`]: every outcome the driver reached, none of the bytes
/// they encoded to — `datagrams`/`fragments` sit on the byte side, being ceilings over
/// [`Cost::refinement_bytes`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Decisions {
    messages: usize,
    ranges: usize,
    enumerations: usize,
    enumerated_elements: usize,
    queries: Queries,
}

/// Whether a drive prices the elements it enumerates.
///
/// The printed tables do; the timed drives do not — encoding a 4 KB value per enumerated element
/// would put this harness's own encoder inside the paper's `T_loc`, which is a measure of local
/// *store* work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pricing {
    /// Fill [`Cost::enumerated_bytes`] by encoding every element the transport would ship.
    Elements,
    /// Count elements, price none.
    None,
}

/// Drive both peers to convergence under `policy`, counting what crosses the wire. Every
/// mismatching range is resolved or strictly refined, so the loop terminates; the guard is a bug
/// net.
///
/// Both peers run the same policy — they need not, but a mixed pair would measure neither.
fn reconcile<S: Rsos<u64>>(a: &S, b: &S, policy: &dyn RefinementPolicy, pricing: Pricing) -> Cost {
    let mut cost = Cost::default();
    let mut active: Vec<RangeAggregate<u64>> = initial_ranges(a);
    // `initial_ranges` came from `a`, so `b` answers first, and the responder alternates from there.
    let mut responder_is_b = true;
    let mut encoded = Vec::new();
    let mut scratch = Vec::new();

    while !active.is_empty() {
        encoded.clear();
        for segment in &active {
            gossip::bincode::encode(segment, &mut encoded)
                .expect("encoding a RangeAggregate into an in-memory buffer cannot fail");
        }
        cost.messages += 1;
        cost.ranges += active.len();
        cost.refinement_bytes += encoded.len();
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
            for (&key, _) in responder.enumerate(range) {
                cost.enumerated_elements += 1;
                if pricing == Pricing::Elements {
                    let bytes = element_bytes(key, &mut scratch);
                    for (total, element) in cost.enumerated_bytes.iter_mut().zip(bytes) {
                        *total += element;
                    }
                }
            }
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

/// The premise of the whole value-size axis, checked instead of asserted: one drive can price every
/// payload size because no decision reads the payload.
///
/// Same keys, three value types — the `u64` every table is driven over, and the dated cells at both
/// ends of [`VALUE_SIZES`] — so the comparison covers the substitution the report actually makes.
/// Decisions must match exactly. Refinement *bytes* are held to a tolerance instead, because a
/// different payload gives a different fingerprint and bincode spends four bytes fewer on a limb
/// that happens to fall below 2³²: an equality assertion would be sound about one run in a hundred
/// thousand, and the quantity it would be wrong about is a handful of bytes in tens of thousands.
fn payload_size_does_not_move_the_trace() {
    const N: usize = 10_000;
    const D: usize = 10;
    /// Refinement-byte drift a differing fingerprint may cause. Two orders of magnitude above what
    /// the varint arithmetic above can produce at this `n`, and far below anything a changed
    /// decision could hide in.
    const TOLERANCE: f64 = 0.001;

    let missing = missing_keys(N, D, Clustering::Scattered);
    let plain = (store(N, &[]), store(N, &missing));
    // Both ends of the axis, since a payload that moved the trace would move it most where it is
    // widest.
    let dated = [VALUE_SIZES[0], VALUE_SIZES[VALUE_SIZES.len() - 1]].map(|value_bytes| {
        (
            value_bytes,
            store_of(N, &[], |key| dated_cell(key, value_bytes)),
            store_of(N, &missing, |key| dated_cell(key, value_bytes)),
        )
    });

    let mut worst_drift = 0.0f64;
    for (name, policy) in policies() {
        let reference = counted_reconcile(&plain.0, &plain.1, policy.as_ref());
        for (value_bytes, full, holed) in &dated {
            let cost = counted_reconcile(full, holed, policy.as_ref());
            assert_eq!(
                cost.decisions(),
                reference.decisions(),
                "{name}: a {value_bytes} B payload changed the refinement trace — one drive \
                 cannot price every value size"
            );
            let drift = (cost.refinement_bytes as f64 - reference.refinement_bytes as f64).abs()
                / reference.refinement_bytes as f64;
            assert!(
                drift <= TOLERANCE,
                "{name}: a {value_bytes} B payload moved the refinement traffic by {:.3} % \
                 ({} B against {} B) — more than a fingerprint's varint width can explain",
                drift * 100.0,
                cost.refinement_bytes,
                reference.refinement_bytes
            );
            worst_drift = worst_drift.max(drift);
        }
    }
    println!(
        "[protocol] payload independence verified at n={N} d={D} scattered, every shipped policy: \
         identical decisions over u64 and {:?} B values, refinement bytes within {:.3} % \
         (tolerated: {:.3} %)",
        dated.map(|(value_bytes, _, _)| value_bytes),
        worst_drift * 100.0,
        TOLERANCE * 100.0
    );
}

/// One `V=… total` cell per payload size.
fn totals(cost: &Cost) -> String {
    VALUE_SIZES
        .iter()
        .zip(cost.total_bytes())
        .map(|(v, total)| format!("V={v:<4} {total:>10}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The same, each total also read against `baseline`'s — the column a sweep is decided on.
fn totals_against(cost: &Cost, baseline: &Cost) -> String {
    VALUE_SIZES
        .iter()
        .zip(cost.total_bytes())
        .zip(baseline.total_bytes())
        .map(|((v, total), base)| {
            format!(
                "V={v:<4} {total:>10} {ratio:>5.2}x",
                ratio = total as f64 / base as f64
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// What an element would have to cost for `cost`'s extra enumeration to pay for the refinement it
/// saves: `(refinement saved) / (extra elements shipped)`, in bytes per element.
///
/// The threshold question in one number, and in the same unit as the element price printed by
/// [`print_element_price`] — an enumeration threshold is worth having exactly where this sits
/// *above* that price. `None` when the policy ships no extra elements, so nothing is traded.
fn break_even_bytes(cost: &Cost, baseline: &Cost) -> Option<f64> {
    let extra = cost
        .enumerated_elements
        .saturating_sub(baseline.enumerated_elements);
    (extra > 0)
        .then(|| (baseline.refinement_bytes as f64 - cost.refinement_bytes as f64) / extra as f64)
}

/// The measured price of one enumerated element, at both ends of the swept key space — the floor
/// every [`break_even_bytes`] is read against, and the reason it is a floor: the key, the stamp and
/// the framing are spent before the payload contributes a byte.
fn print_element_price() {
    let mut scratch = Vec::new();
    for key in [0, *SIZES.last().expect("SIZES is never empty") as u64 - 1] {
        let priced = element_bytes(key, &mut scratch);
        println!(
            "[protocol] one enumerated element, key {key}: {priced:?} B at V={VALUE_SIZES:?} B \
             — {overhead} B of key, stamp and framing before the payload",
            overhead = priced[0] - VALUE_SIZES[0],
        );
    }
}

/// The refinement/IDLIST breakdown under a total: why the policy landed there.
fn breakdown(cost: &Cost) -> String {
    format!(
        "refine {bytes:>9} B / {ranges:>6} r / {messages:>3} msgs / {datagrams:>3} dgrams \
         / {fragments:>5} frags | widest {largest:>5} r = {largest_bytes:>7} B \
         | idlist {enumerations:>4} r / {elements:>7} elem \
         | agg {aggregate:>7} rank {rank:>7} sel {select:>7}",
        bytes = cost.refinement_bytes,
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
    )
}

/// One priced reconciliation, with both peers' local query counts folded in: the table path.
fn counted_reconcile<S: Rsos<u64>>(a: &S, b: &S, policy: &dyn RefinementPolicy) -> Cost {
    let (counted_a, counted_b) = (Counting::new(a), Counting::new(b));
    let mut cost = reconcile(&counted_a, &counted_b, policy, Pricing::Elements);
    cost.queries = counted_a.queries() + counted_b.queries();
    cost
}

/// Exchanged volume per policy, printed rather than timed — exact and reproducible for a given
/// `(policy, n, d, clustering)` — alongside the timed drive loop, the paper's `T_loc`.
fn reconciliation_cost(c: &mut Criterion) {
    payload_size_does_not_move_the_trace();
    print_element_price();
    println!(
        "[protocol] full reconciliation, u64 keys. Refinement policy is a local decision: \
         the wire type carries none, so these are comparable runs of the same protocol.\n\
         [protocol] first line: total wire bytes (refinement + enumerated values) per value size; \
         second: what makes it up, and the round-trip count no total prices."
    );
    for &n in SIZES {
        // The complete store is the same for every corpus at this size; only the holed one varies.
        let full = store(n, &[]);
        for &(d, clustering) in DIFFERENCES {
            let holed = store(n, &missing_keys(n, d, clustering));
            println!("[protocol] n={n} d={d} {}", clustering.label());
            for (name, policy) in policies() {
                let cost = counted_reconcile(&full, &holed, policy.as_ref());
                println!("[protocol]   {name:<20} {}", totals(&cost));
                println!("[protocol]   {:<20} {}", "", breakdown(&cost));
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
                bencher.iter(|| {
                    reconcile(
                        black_box(&full),
                        black_box(&holed),
                        policy.as_ref(),
                        Pricing::None,
                    )
                });
            });
        }
    }
    group.finish();
}

/// Sweep the branching factor `b` alone, [`FixedFanOut`] only, so a default can be chosen on
/// evidence.
///
/// **What to read.** Ranges grow as `b / ln b` (minimized near `b = 3`) while one-way messages
/// fall as `log_b n`; this target runs at RTT ≈ 0, so the message column has to be weighed by your
/// own round-trip time — one RTT per round trip, per `system`'s injected-RTT lane. The hard limit is
/// the widest single round, linear in `b`, which must fit a datagram and survive fragmentation.
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
                let cost = counted_reconcile(&full, &holed, &policy);
                println!("[sweep]   b={b:<4} {}", totals(&cost));
                println!("[sweep]   {:<6} {}", "", breakdown(&cost));
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
            bencher.iter(|| reconcile(black_box(&full), black_box(&holed), &policy, Pricing::None));
        });
    }
    group.finish();
}

/// Sweep the enumeration threshold `t` alone, [`EnumerateBelowThreshold`] at the default `b = 16`,
/// against [`FixedFanOut`] at that same `b` — today's default, and the only baseline the question
/// "should `t` exist at all?" can be answered against.
///
/// **What to read.** `t` buys refinement bytes with values: raising it stops the descent earlier,
/// and everything it stops on ships whole, peer-held elements included. The two move in opposite
/// directions in the same unit, so each row is its total against the baseline's, at every payload
/// size — a `t` earns its place only where that ratio is below 1. The row's `break-even` is the
/// same trade as a single number: the element price at which it would come out even, to be read
/// against the element price printed by `reconciliation_cost`.
///
/// `t` is not a continuous knob. A range's span walks the ladder `n / b^k`, so every `t` between
/// two rungs picks the same rung and costs exactly the same — the plateaus in the table are the
/// ladder, not noise.
fn threshold_sweep(c: &mut Criterion) {
    println!(
        "[threshold] EnumerateBelowThreshold, enumeration threshold only, b=16 throughout, \
         u64 keys, differences scattered.\n\
         [threshold] each row is total wire bytes and its ratio to FixedFanOut(16), today's \
         default; below 1.00x, `t` pays for itself."
    );
    for &(n, diffs) in SWEEP_CASES {
        let full = store(n, &[]);
        for &d in diffs {
            let holed = store(n, &missing_keys(n, d, Clustering::Scattered));
            println!("[threshold] n={n} d={d} scattered");
            let baseline = counted_reconcile(&full, &holed, &FixedFanOut::new(FanOut::NEGENTROPY));
            println!("[threshold]   b=16, no t   {}", totals(&baseline));
            println!("[threshold]   {:<11} {}", "", breakdown(&baseline));
            for &t in THRESHOLDS {
                let policy = EnumerateBelowThreshold::new(t, FanOut::NEGENTROPY);
                let cost = counted_reconcile(&full, &holed, &policy);
                println!(
                    "[threshold]   t={t:<9} {} | break-even {}",
                    totals_against(&cost, &baseline),
                    match break_even_bytes(&cost, &baseline) {
                        Some(bytes) => format!("{bytes:>8.1} B/elem"),
                        // No extra element shipped: this `t` reaches the default's own cutoffs.
                        None => "       — same elements".to_string(),
                    }
                );
                println!("[threshold]   {:<11} {}", "", breakdown(&cost));
            }
        }
    }

    // The local half, timed: enumerating a range is `T_loc` too, and it grows with `t`. At the
    // difference size where `t` bites — a lone missing element never reaches the threshold.
    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("threshold_sweep_drive");
    group.plot_config(plot_config);
    group.sample_size(10);
    let full = store(1_000_000, &[]);
    let holed = store(
        1_000_000,
        &missing_keys(1_000_000, 100, Clustering::Scattered),
    );
    for &t in THRESHOLDS {
        let policy = EnumerateBelowThreshold::new(t, FanOut::NEGENTROPY);
        group.bench_with_input(BenchmarkId::from_parameter(t), &t, |bencher, _| {
            bencher.iter(|| reconcile(black_box(&full), black_box(&holed), &policy, Pricing::None));
        });
    }
    group.finish();
}

criterion_group!(benches, reconciliation_cost, fan_out_sweep, threshold_sweep);
criterion_main!(benches);

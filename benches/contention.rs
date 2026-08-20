// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `K`-writer contention: write throughput vs writer count `N`, for `FingerprintTreeMap` and for
//! plain `BTreeMap`, each behind one shared `parking_lot::RwLock` of the exact shape
//! `src/replica.rs` uses for its `map` field (`Arc<RwLock<FingerprintTreeMap<K, V>>>`).
//!
//! **Why this exists (#445, #359).** The RSOS contract (`rsos/src/fingerprint_tree_map.rs`) must
//! answer `Aggregate(l, u)` in `O(log n)`, which means every insert updates the composable summary
//! on every node from the leaf to the root — a write to the hottest node in the tree, on every
//! insert, by construction. Today that cost is invisible: `src/replicated_map.rs` already
//! serialises every writer behind one global `RwLock`, so the root write costs nothing beyond the
//! lock itself. This target isolates the two: the `FingerprintTreeMap` arm pays the lock *and* the
//! root-path aggregate maintenance; the `BTreeMap` arm pays the same lock and the same insert shape
//! with no aggregate to maintain. The delta between the two arms, at each `N`, is the RSOS
//! contract's own share of the write cost — the number #359 asked for.
//!
//! **What is, and is not, measured.** Both arms insert into a map pre-filled to [`PREFILL`] entries
//! (a representative tree depth — an empty map has no root path worth contending on), then `N`
//! threads each insert their own disjoint block of fresh keys, one `write()` acquisition per key,
//! exactly the shape `Replica::just_insert`/gossip receipt takes today. Pre-fill and thread setup
//! happen outside the timed region; only the concurrent insert phase is timed. This is a **lock
//! contention** benchmark, not a lock-free redesign or a COW prototype — both are #271/#273/#274.
//!
//! **Comparability caveat (#281).** Both arms run in the same process, on the same hardware, over
//! the same harness, in the same run — the only comparison this target supports is arm against arm,
//! at a given `N`, on the machine that produced the numbers. Absolute ops/s are not portable across
//! machines; the *ratio* between arms at a given `N` is the load-bearing number.
//!
//! Reproduction and results: `benches/README.md`. Not run in CI (only compile-checked); run locally
//! with `cargo bench --bench contention`.

use std::hint::black_box;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    Throughput,
};
use parking_lot::RwLock;

use reconcile::FingerprintTreeMap;

/// Writer-thread counts swept. `1, 2, 4` are below this machine's core count, `8, 16` push past it
/// deliberately — contention past the core count is exactly the regime a lock-free redesign (#271)
/// would target, so the sweep needs to show where it starts, not stop at the core count.
const WRITER_COUNTS: &[usize] = &[1, 2, 4, 8, 16];

/// Entries each writer inserts per sample. Large enough that thread-spawn/join overhead (a few µs
/// per thread) is a small fraction of the timed region even at the smallest `N`.
const OPS_PER_WRITER: usize = 20_000;

/// Entries the map is pre-filled to before writers start, outside the timed region. Gives the
/// root path a non-trivial depth (`log_16 100_000 ≈ 5`) — an empty map has no aggregate maintenance
/// worth contending on, which would understate the RSOS arm's cost.
const PREFILL: usize = 100_000;

/// One arm's shared target: a map behind the same `parking_lot::RwLock` shape
/// `src/replica.rs`'s `map: Arc<RwLock<FingerprintTreeMap<K, V>>>` uses.
trait ContentionTarget: Send + Sync {
    /// Insert `key` under the write lock — one lock acquisition per call, matching one gossip
    /// receipt or one local write.
    fn insert(&self, key: u64);
}

struct FingerprintArm(RwLock<FingerprintTreeMap<u64, u64>>);

impl ContentionTarget for FingerprintArm {
    fn insert(&self, key: u64) {
        self.0.write().insert(key, key);
    }
}

struct BTreeArm(RwLock<std::collections::BTreeMap<u64, u64>>);

impl ContentionTarget for BTreeArm {
    fn insert(&self, key: u64) {
        self.0.write().insert(key, key);
    }
}

fn prefilled_fingerprint_arm() -> FingerprintArm {
    let mut map = FingerprintTreeMap::<u64, u64>::new();
    for key in 0..PREFILL as u64 {
        map.insert(key, key);
    }
    FingerprintArm(RwLock::new(map))
}

fn prefilled_btree_arm() -> BTreeArm {
    let mut map = std::collections::BTreeMap::new();
    for key in 0..PREFILL as u64 {
        map.insert(key, key);
    }
    BTreeArm(RwLock::new(map))
}

/// Run `n` writer threads, each inserting [`OPS_PER_WRITER`] fresh, disjoint keys into `target`
/// (keyed past [`PREFILL`] so no writer's insert collides with the pre-fill or with another
/// writer's block), starting together via a barrier so the timed region is genuinely concurrent
/// rather than staggered by thread-spawn latency. Returns the wall-clock time of the concurrent
/// phase alone.
fn timed_concurrent_insert(target: &(impl ContentionTarget + ?Sized), n: usize) -> Duration {
    let barrier = Barrier::new(n);
    let start = Instant::now();
    thread::scope(|scope| {
        for t in 0..n {
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let base = PREFILL as u64 + (t * OPS_PER_WRITER) as u64;
                for i in 0..OPS_PER_WRITER as u64 {
                    target.insert(base + i);
                }
            });
        }
    });
    start.elapsed()
}

/// Throughput in ops/s for one `(arm, n)` sample: `n * OPS_PER_WRITER` inserts over the timed
/// concurrent phase.
fn throughput_ops_per_sec(elapsed: Duration, n: usize) -> f64 {
    (n * OPS_PER_WRITER) as f64 / elapsed.as_secs_f64()
}

/// Printed report: throughput vs `N` for both arms, and the delta between them at each `N` — the
/// number #359 asked for, stated explicitly rather than left for a reader to compute from two
/// separate tables. One sample per `N` (not Criterion's statistical sampling): this table is meant
/// to be read directly and copied into `benches/README.md`, and a single large-enough sample
/// (`OPS_PER_WRITER` = 20 000 per writer) is stable enough for that; the Criterion groups below give
/// the same measurement statistical treatment for anyone who wants it.
fn print_contention_summary() {
    println!(
        "[contention] {OPS_PER_WRITER} inserts/writer, map pre-filled to {PREFILL} entries — \
         throughput (ops/s) and the FingerprintTreeMap-vs-BTreeMap delta at each N:"
    );
    println!(
        "[contention] {:>7} | {:>14} | {:>14} | {:>8} | {:>8}",
        "writers", "fingerprint/s", "btree/s", "ratio", "delta/s"
    );
    for &n in WRITER_COUNTS {
        let fp = prefilled_fingerprint_arm();
        let fp_elapsed = timed_concurrent_insert(&fp, n);
        let fp_throughput = throughput_ops_per_sec(fp_elapsed, n);

        let bt = prefilled_btree_arm();
        let bt_elapsed = timed_concurrent_insert(&bt, n);
        let bt_throughput = throughput_ops_per_sec(bt_elapsed, n);

        println!(
            "[contention] {n:>7} | {fp_throughput:>14.0} | {bt_throughput:>14.0} | \
             {ratio:>7.2}x | {delta:>8.0}",
            ratio = fp_throughput / bt_throughput,
            delta = bt_throughput - fp_throughput,
        );
    }
}

/// Timed Criterion groups for both arms, over the same `N` sweep, with Criterion's own statistical
/// sampling and `Throughput::Elements` so `target/criterion/report/index.html` carries the same
/// numbers [`print_contention_summary`] prints, with confidence intervals.
fn writer_contention(c: &mut Criterion) {
    print_contention_summary();

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("writer_contention");
    group.plot_config(plot_config);

    for &n in WRITER_COUNTS {
        group.throughput(Throughput::Elements((n * OPS_PER_WRITER) as u64));
        // Fewer samples at high N: each sample is already N * OPS_PER_WRITER inserts, and Criterion
        // needs a fresh, freshly pre-filled map per sample (the timed region must start from the
        // same tree depth every time), which itself costs O(PREFILL).
        group.sample_size(10);

        group.bench_with_input(
            BenchmarkId::new("fingerprint_tree_map", n),
            &n,
            |bencher, &n| {
                bencher.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let arm = prefilled_fingerprint_arm();
                        total += timed_concurrent_insert(black_box(&arm), n);
                    }
                    total
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("btree_map", n), &n, |bencher, &n| {
            bencher.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let arm = prefilled_btree_arm();
                    total += timed_concurrent_insert(black_box(&arm), n);
                }
                total
            });
        });
    }
    group.finish();
}

criterion_group!(benches, writer_contention);
criterion_main!(benches);

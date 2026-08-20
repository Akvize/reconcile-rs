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
//! **What #455 changed.** #359 reported one run per `(N, arm)` and compared two tables by eye. Two
//! things replace that here:
//!
//! 1. **A counted result**, printed under `--cfg reconcile_internal_testing` and independent of
//!    any machine:
//!    `rsos::counters` reports how many cached aggregates an insert maintains. That number does not
//!    move between a laptop and a 128-core server, so a reader can check it without the hardware.
//! 2. **A statistically treated wall-clock result**: [`TRIALS`] repeated trials per `(N, arm)`,
//!    arms **paired** within a trial, their order alternated between trials, and every
//!    `(N, trial)` of the sweep executed in one shuffled schedule — reported as means with
//!    percentile-bootstrap intervals (`benches/stats/mod.rs`).
//! 3. **A statistic that answers the actual question.** #359 read the fp/btree *ratio*, which is a
//!    quotient of two terms that both grow with `N` and so cannot say which moved. Both arms sit
//!    behind the same lock, so `1/X_arm = S_arm + H(N)` and the difference of reciprocal
//!    throughputs cancels the shared lock term: `Δ = 1/X_fp − 1/X_btree = S_fp − S_btree`, the
//!    contract's own per-insert cost. Δ is what the report leads with (#457).
//!
//! Throughput stays wall-clock on purpose: lock waiting *is* elapsed time, and there is no counted
//! proxy for it. The counted line prices the contract's own work; the timed lines price what the
//! lock does to it.
//!
//! **What is, and is not, measured.** Both arms insert into a map pre-filled to [`PREFILL`] entries
//! (a representative tree depth — an empty map has no root path worth contending on), then `N`
//! threads each insert their own disjoint block of fresh keys, one `write()` acquisition per key,
//! exactly the shape `Replica::just_insert`/gossip receipt takes today. Pre-fill and thread setup
//! happen outside the timed region; only the concurrent insert phase is timed. This is a **lock
//! contention** benchmark, not a lock-free redesign or a COW prototype — both are #271/#273/#274.
//!
//! **Comparability caveat (#281).** Both arms run in the same process, on the same hardware, over
//! the same harness, in the same run — the only comparison the *timed* half supports is arm against
//! arm, at a given `N`, on the machine that produced the numbers. Absolute ops/s are not portable
//! across machines. The *counted* half carries no such caveat, which is the point of having it.
//!
//! **Sweeping other hardware (#456).** Every parameter is overridable from the environment, so a
//! machine with more cores than this repository's CI runner needs no source edit:
//!
//! ```sh
//! CONTENTION_WRITERS=1,2,4,8,16,32,64,128 CONTENTION_TRIALS=30 cargo bench --bench contention
//! ```
//!
//! **The experimental unit is the invocation, not the trial.** Trials inside one process share a
//! machine phase — a co-tenant's load, a thermal state, one allocator's layout — so an interval
//! computed from them measures within-process dispersion and is silent about the drift between
//! processes, which on a shared machine is the larger term. `CONTENTION_RAW=1` prints one
//! comma-separated line per trial (`[contention-raw]` prefix) so several invocations can be pooled
//! and the statistics redone downstream; `benches/README.md` carries the recipe. Anything published
//! from a single invocation is a pilot, not a result.
//!
//! Reproduction and results: `benches/README.md`. Not run in CI (only compile-checked); run locally
//! with `cargo bench --bench contention`.

use std::hint::black_box;
use std::str::FromStr;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    Throughput,
};
use parking_lot::RwLock;
use rand::{seq::SliceRandom, SeedableRng};

use reconcile::FingerprintTreeMap;

mod stats;

use stats::{diff_ci, excludes_zero, summarize, Summary};

/// Writer-thread counts swept by default. `1, 2, 4` are below this machine's core count, `8, 16`
/// push past it deliberately — contention past the core count is exactly the regime a lock-free
/// redesign (#271) would target, so the sweep needs to show where it starts, not stop at the core
/// count. Override with `CONTENTION_WRITERS` (#456).
const WRITER_COUNTS: &[usize] = &[1, 2, 4, 8, 16];

/// Entries each writer inserts per trial. Large enough that thread-spawn/join overhead (a few µs
/// per thread) is a small fraction of the timed region even at the smallest `N`.
const OPS_PER_WRITER: usize = 20_000;

/// Entries the map is pre-filled to before writers start, outside the timed region. Gives the
/// root path a non-trivial depth (`log₆ 100 000 ≈ 6`) — an empty map has no aggregate maintenance
/// worth contending on, which would understate the RSOS arm's cost.
const PREFILL: usize = 100_000;

/// Trials per `(N, arm)` retained for the statistics. #455 asks for 20–30; 30 is the top of that
/// band and still costs seconds, since one trial is already `N * OPS_PER_WRITER` inserts.
const TRIALS: usize = 30;

/// Paired trials run and discarded before the sweep proper, at its largest `N` (the most thread
/// creation and allocation of any point). Absorbs process-startup effects — first-touch page
/// faults, CPU frequency ramp — which would otherwise be charged to whichever trial the schedule
/// happens to draw first.
const WARMUP: usize = 3;

/// Seed for the trial schedule's shuffle. Fixed, so a run is reproducible as an experiment: the
/// *measurements* vary, the *design* does not.
const SCHEDULE_SEED: u64 = 20_260_820;

/// Read `name` from the environment, falling back to `default`.
///
/// # Panics
///
/// If `name` is set but unparseable — a typo in a sweep parameter must not be silently ignored,
/// leaving a run that quietly measured the default.
fn env_or<T: FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Err(_) => default,
        Ok(raw) => raw
            .parse()
            .unwrap_or_else(|_| panic!("{name}={raw:?} could not be parsed")),
    }
}

/// The `N` sweep: `CONTENTION_WRITERS` as a comma-separated list, else [`WRITER_COUNTS`].
///
/// # Panics
///
/// If the variable is set but malformed.
fn writer_counts() -> Vec<usize> {
    match std::env::var("CONTENTION_WRITERS") {
        Err(_) => WRITER_COUNTS.to_vec(),
        Ok(raw) => {
            raw.split(',')
                .map(|field| {
                    field.trim().parse().unwrap_or_else(|_| {
                        panic!("CONTENTION_WRITERS field {field:?} is not a count")
                    })
                })
                .collect()
        }
    }
}

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

fn prefilled_fingerprint_arm(prefill: usize) -> FingerprintArm {
    let mut map = FingerprintTreeMap::<u64, u64>::new();
    for key in 0..prefill as u64 {
        map.insert(key, key);
    }
    FingerprintArm(RwLock::new(map))
}

fn prefilled_btree_arm(prefill: usize) -> BTreeArm {
    let mut map = std::collections::BTreeMap::new();
    for key in 0..prefill as u64 {
        map.insert(key, key);
    }
    BTreeArm(RwLock::new(map))
}

/// Run `n` writer threads, each inserting `ops` fresh, disjoint keys into `target` (keyed past
/// `prefill` so no writer's insert collides with the pre-fill or with another writer's block),
/// starting together via a barrier so the timed region is genuinely concurrent rather than
/// staggered by thread-spawn latency. Returns the wall-clock time of the concurrent phase alone.
fn timed_concurrent_insert(
    target: &(impl ContentionTarget + ?Sized),
    n: usize,
    ops: usize,
    prefill: usize,
) -> Duration {
    let barrier = Barrier::new(n);
    let start = Instant::now();
    thread::scope(|scope| {
        for t in 0..n {
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let base = prefill as u64 + (t * ops) as u64;
                for i in 0..ops as u64 {
                    target.insert(base + i);
                }
            });
        }
    });
    start.elapsed()
}

/// Throughput in ops/s for one trial: `n * ops` inserts over the timed concurrent phase.
fn throughput_ops_per_sec(elapsed: Duration, n: usize, ops: usize) -> f64 {
    (n * ops) as f64 / elapsed.as_secs_f64()
}

/// The retained trials for one writer count: both arms and, per trial, their ratio.
struct Point {
    n: usize,
    fingerprint: Vec<f64>,
    btree: Vec<f64>,
    /// `fingerprint / btree` **within a trial**. Pairing matters: a machine-wide disturbance during
    /// trial `t` moves both arms, and dividing inside the trial cancels it. A ratio of the two
    /// separately-computed means would keep that noise instead.
    ratio: Vec<f64>,
    /// The same ratios, split by which arm ran first in the trial. Alternating the order removes
    /// the bias an order effect would put in the mean, but it converts that effect into *spread*:
    /// if running second is systematically cheaper, the retained ratios become bimodal. Splitting
    /// them is how [`print_order_effect`] tells a real order effect from ordinary noise, rather
    /// than leaving an unexplained coefficient of variation in the table.
    ratio_fingerprint_first: Vec<f64>,
    ratio_btree_first: Vec<f64>,
    /// `1/X_fp − 1/X_btree` **within a trial**, in nanoseconds of system-wide time per insert.
    ///
    /// Both arms sit behind the same lock, so each arm's seconds-per-insert is its own critical
    /// section plus whatever that lock costs per acquisition at this `N`:
    /// `1/X_arm = S_arm + H(N)`. The lock term is common to the two arms, so **the difference
    /// cancels it**: `Δ = S_fp − S_btree`, the RSOS contract's own per-insert cost with the lock
    /// divided out. That makes Δ, not the ratio, the quantity that answers "does contention make
    /// the contract itself more expensive" — the ratio moves when *either* term moves and cannot
    /// say which (#457).
    delta_ns: Vec<f64>,
}

/// One paired trial at writer count `n`: both arms, back to back, in the order `fingerprint_first`
/// asks for. Returns `(fingerprint ops/s, btree ops/s)`.
fn paired_trial(n: usize, ops: usize, prefill: usize, fingerprint_first: bool) -> (f64, f64) {
    let measure_fingerprint = || {
        let arm = prefilled_fingerprint_arm(prefill);
        let elapsed = timed_concurrent_insert(black_box(&arm), n, ops, prefill);
        throughput_ops_per_sec(elapsed, n, ops)
    };
    let measure_btree = || {
        let arm = prefilled_btree_arm(prefill);
        let elapsed = timed_concurrent_insert(black_box(&arm), n, ops, prefill);
        throughput_ops_per_sec(elapsed, n, ops)
    };
    if fingerprint_first {
        let fingerprint = measure_fingerprint();
        (fingerprint, measure_btree())
    } else {
        let btree = measure_btree();
        (measure_fingerprint(), btree)
    }
}

/// Run `trials` paired trials for every writer count, in **one randomized schedule** across the
/// whole sweep.
///
/// Why not a loop per `N`. Running all of one `N`'s trials consecutively makes the block of
/// wall-clock they occupy part of the treatment: a co-tenant spike, a thermal excursion or a page
/// cache eviction lasting tens of seconds lands entirely on whichever `N` was running, and shows up
/// as a property of that `N`. Two 30-trial sweeps built that way disagreed here by more than either
/// one's confidence interval admitted — the intervals were measuring within-block noise while
/// between-block drift, the larger term, went unrepresented. Interleaving every `(N, trial)` in
/// shuffled order spreads any such episode across all `N`, which converts that bias into variance
/// the intervals then report honestly.
///
/// Arm order within a trial still alternates rather than being drawn at random, so each `N` gets
/// each order exactly half the time — see [`print_order_effect`].
fn run_sweep(counts: &[usize], trials: usize, ops: usize, prefill: usize) -> Vec<Point> {
    let mut points: Vec<Point> = counts
        .iter()
        .map(|&n| Point {
            n,
            fingerprint: Vec::with_capacity(trials),
            btree: Vec::with_capacity(trials),
            ratio: Vec::with_capacity(trials),
            ratio_fingerprint_first: Vec::new(),
            ratio_btree_first: Vec::new(),
            delta_ns: Vec::with_capacity(trials),
        })
        .collect();

    // Startup absorption, before anything is retained: the largest `N` spawns the most threads and
    // touches the most fresh pages, so it warms what the rest of the sweep will reuse.
    if let Some(&widest) = counts.iter().max() {
        for trial in 0..WARMUP {
            paired_trial(widest, ops, prefill, trial % 2 == 0);
        }
    }

    let mut schedule: Vec<(usize, usize)> = (0..points.len())
        .flat_map(|index| (0..trials).map(move |trial| (index, trial)))
        .collect();
    schedule.shuffle(&mut rand::rngs::StdRng::seed_from_u64(SCHEDULE_SEED));

    let raw = std::env::var("CONTENTION_RAW").is_ok();
    if raw {
        println!(
            "[contention-raw] writers,fingerprint_ops_per_sec,btree_ops_per_sec,fingerprint_first"
        );
    }

    for (index, trial) in schedule {
        let point = &mut points[index];
        let fingerprint_first = trial % 2 == 0;
        let (fingerprint, btree) = paired_trial(point.n, ops, prefill, fingerprint_first);
        if raw {
            println!(
                "[contention-raw] {},{fingerprint:.1},{btree:.1},{fingerprint_first}",
                point.n
            );
        }
        point.fingerprint.push(fingerprint);
        point.btree.push(btree);
        let ratio = fingerprint / btree;
        point.ratio.push(ratio);
        if fingerprint_first {
            point.ratio_fingerprint_first.push(ratio);
        } else {
            point.ratio_btree_first.push(ratio);
        }
        const NANOS_PER_SEC: f64 = 1e9;
        point
            .delta_ns
            .push(NANOS_PER_SEC * (1.0 / fingerprint - 1.0 / btree));
    }
    points
}

/// The per-`N` table: both arms and the paired ratio, each with its bootstrap interval.
fn print_throughput_table(points: &[Point], trials: usize, ops: usize, prefill: usize) {
    println!(
        "[contention] {trials} trials per (N, arm), {ops} inserts/writer, map pre-filled to \
         {prefill} entries."
    );
    println!(
        "[contention] Mean with a 95% percentile-bootstrap interval. `delta` is \
         1/X_fp - 1/X_btree: the contract's own per-insert cost, with the shared lock term \
         cancelled (#457)."
    );
    println!(
        "[contention] {:>7} | {:>29} | {:>29} | {:>25} | {:>6} | {:>22}",
        "writers",
        "fingerprint ops/s",
        "btree ops/s",
        "ratio (paired, fp/btree)",
        "median",
        "delta ns/insert"
    );
    for point in points {
        let fingerprint = summarize(&point.fingerprint);
        let btree = summarize(&point.btree);
        let ratio = summarize(&point.ratio);
        let delta = summarize(&point.delta_ns);
        println!(
            "[contention] {n:>7} | {fp_mean:>9.0} [{fp_lo:.0}, {fp_hi:.0}] | \
             {bt_mean:>9.0} [{bt_lo:.0}, {bt_hi:.0}] | {r_mean:>6.3} [{r_lo:.3}, {r_hi:.3}] | \
             {r_median:>6.3} | {d_mean:>6.0} [{d_lo:.0}, {d_hi:.0}]",
            n = point.n,
            fp_mean = fingerprint.mean,
            fp_lo = fingerprint.lo,
            fp_hi = fingerprint.hi,
            bt_mean = btree.mean,
            bt_lo = btree.lo,
            bt_hi = btree.hi,
            r_mean = ratio.mean,
            r_lo = ratio.lo,
            r_hi = ratio.hi,
            r_median = ratio.median,
            d_mean = delta.mean,
            d_lo = delta.lo,
            d_hi = delta.hi,
        );
    }
}

/// The claim #359 made and #455 asks to test properly: does the fp/btree ratio *widen* as writer
/// count grows — that is, does the contract's share of the cost grow with contention?
///
/// Two comparisons, both as bootstrap intervals on a difference of means rather than as an
/// eyeballed overlap of two intervals (overlap is not a test; an interval on the difference is).
fn print_ratio_trend(points: &[Point]) {
    let Some(baseline) = points.first() else {
        return;
    };
    println!(
        "[contention] Does the ratio move with N? Bootstrap interval on the difference of paired \
         ratios; an interval excluding 0 is a real move at 95%."
    );

    println!(
        "[contention] vs N={} (the uncontended point — the contract's cost alone):",
        baseline.n
    );
    for point in points.iter().skip(1) {
        let difference = diff_ci(&point.ratio, &baseline.ratio);
        let verdict = if !excludes_zero(&difference) {
            "indistinguishable"
        } else if difference.mean > 0.0 {
            "DILUTED by contention"
        } else {
            "WIDENED by contention"
        };
        println!(
            "[contention] {:>34} {:>+8.3} [{:+.3}, {:+.3}]  {}",
            format!("ratio(N={}) - ratio(N={})", point.n, baseline.n),
            difference.mean,
            difference.lo,
            difference.hi,
            verdict,
        );
    }

    println!(
        "[contention] And the sharper question -- does the contract's OWN cost grow, with the lock \
         term divided out (delta = 1/X_fp - 1/X_btree)?"
    );
    for point in points.iter().skip(1) {
        let difference = diff_ci(&point.delta_ns, &baseline.delta_ns);
        println!(
            "[contention] {:>34} {:>+8.0} [{:+.0}, {:+.0}] ns  {}",
            format!("delta(N={}) - delta(N={})", point.n, baseline.n),
            difference.mean,
            difference.lo,
            difference.hi,
            if !excludes_zero(&difference) {
                "indistinguishable"
            } else if difference.mean > 0.0 {
                "the contract costs MORE under contention"
            } else {
                "the contract costs LESS under contention"
            },
        );
    }

    // Among contended points only: N=1 has no lock waiting at all, so including it would confound
    // "the ratio changes once a lock is contended" with "the ratio changes as contention deepens".
    let contended: Vec<&Point> = points.iter().filter(|point| point.n > 1).collect();
    if let (Some(first), Some(last)) = (contended.first(), contended.last()) {
        if first.n != last.n {
            let difference = diff_ci(&last.ratio, &first.ratio);
            println!(
                "[contention] Across contended N only, ratio(N={}) - ratio(N={}): \
                 {:+.3} [{:+.3}, {:+.3}] -- {}",
                last.n,
                first.n,
                difference.mean,
                difference.lo,
                difference.hi,
                if excludes_zero(&difference) {
                    "the ratio does move as contention deepens"
                } else {
                    "no detectable trend as contention deepens"
                },
            );
        }
    }

    // Interval geometry, which #455 asks to see alongside the tests above. Reported as a fact about
    // the intervals, never as a substitute for the difference tests: overlapping intervals do not
    // imply the means agree.
    let ratios: Vec<(usize, Summary)> = points
        .iter()
        .map(|point| (point.n, summarize(&point.ratio)))
        .collect();
    let disjoint: Vec<String> = ratios
        .iter()
        .enumerate()
        .flat_map(|(i, (n, a))| {
            ratios[i + 1..]
                .iter()
                .filter(move |(_, b)| a.disjoint_from(b))
                .map(move |(m, _)| format!("{n}/{m}"))
        })
        .collect();
    println!(
        "[contention] Ratio intervals that do not overlap: {}",
        if disjoint.is_empty() {
            "none".to_string()
        } else {
            disjoint.join(", ")
        }
    );
}

/// Validate the harness, not the subject: does running first or second systematically change an
/// arm's throughput?
///
/// Alternating the order keeps such an effect out of the mean, so this cannot invalidate the table
/// above — but an effect large enough to detect belongs in the write-up, because it is most of the
/// dispersion the `cv` column reports and a reader would otherwise read that dispersion as
/// measurement noise.
fn print_order_effect(points: &[Point]) {
    println!(
        "[contention] Order effect (harness check): mean paired ratio when the fingerprint arm ran \
         first, minus when it ran second."
    );
    for point in points {
        if point.ratio_fingerprint_first.is_empty() || point.ratio_btree_first.is_empty() {
            continue;
        }
        let difference = diff_ci(&point.ratio_fingerprint_first, &point.ratio_btree_first);
        println!(
            "[contention] {:>34} {:>+8.3} [{:+.3}, {:+.3}]  {}",
            format!("N={}", point.n),
            difference.mean,
            difference.lo,
            difference.hi,
            if excludes_zero(&difference) {
                "order matters -- alternation is load-bearing"
            } else {
                "no detectable order effect"
            },
        );
    }
}

/// The machine-independent half (#454's "stands on its own outside this repo").
///
/// Reports how many cached aggregates one insert maintains — the work the RSOS contract mandates
/// and a plain `BTreeMap`, doing the same descent with no summary to keep, does not do at all. Run
/// single-threaded, untimed, outside any lock: the number is deterministic, so one pass
/// characterizes every writer count, and nothing here can perturb the timed phases above.
#[cfg(reconcile_internal_testing)]
fn print_counted_summary(prefill: usize) {
    use rsos::counters;

    let mut map = FingerprintTreeMap::<u64, u64>::new();
    for key in 0..prefill as u64 {
        map.insert(key, key);
    }

    const PROBE: u64 = 4_096;
    let before = counters::snapshot();
    for i in 0..PROBE {
        map.insert(prefill as u64 + i, i);
    }
    let fresh = (counters::snapshot() - before).aggregate_updates;

    let before = counters::snapshot();
    for i in 0..PROBE {
        map.insert(i, i + 1);
    }
    let overwrite = (counters::snapshot() - before).aggregate_updates;

    println!(
        "[contention] Counted (machine-independent), map of {prefill} entries, {PROBE} probes:"
    );
    println!(
        "[contention] {:>34} {:.2}   (BTreeMap control: 0.00, by construction)",
        "aggregate updates / fresh insert",
        fresh as f64 / PROBE as f64,
    );
    println!(
        "[contention] {:>34} {:.2}   -- one per level of the key's root path",
        "aggregate updates / overwrite",
        overwrite as f64 / PROBE as f64,
    );
}

#[cfg(not(reconcile_internal_testing))]
fn print_counted_summary(_prefill: usize) {
    println!(
        "[contention] Counted half skipped: rebuild with \
         `RUSTFLAGS='--cfg reconcile_internal_testing'` for the machine-independent \
         aggregate-update counts."
    );
}

/// Printed report: the counted result, then throughput vs `N` for both arms with intervals, then
/// the explicit test of whether the ratio moves with `N`. Meant to be read directly and copied into
/// `benches/README.md`; the Criterion groups below plot the same measurement.
fn print_contention_report() {
    let trials = env_or("CONTENTION_TRIALS", TRIALS);
    let ops = env_or("CONTENTION_OPS", OPS_PER_WRITER);
    let prefill = env_or("CONTENTION_PREFILL", PREFILL);

    print_counted_summary(prefill);

    let points = run_sweep(&writer_counts(), trials, ops, prefill);

    print_throughput_table(&points, trials, ops, prefill);
    print_ratio_trend(&points);
    print_order_effect(&points);
}

/// Timed Criterion groups for both arms, over the same `N` sweep, with Criterion's own sampling and
/// `Throughput::Elements` so `target/criterion/report/index.html` plots the same measurement the
/// report above states. The report, not this group, is what `benches/README.md` quotes: it pairs the
/// arms within a trial and puts an interval on their ratio, which Criterion — measuring each
/// benchmark id independently — cannot do.
fn writer_contention(c: &mut Criterion) {
    print_contention_report();

    let ops = env_or("CONTENTION_OPS", OPS_PER_WRITER);
    let prefill = env_or("CONTENTION_PREFILL", PREFILL);

    let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
    let mut group = c.benchmark_group("writer_contention");
    group.plot_config(plot_config);

    for n in writer_counts() {
        group.throughput(Throughput::Elements((n * ops) as u64));
        // Fewer samples at high N: each sample is already N * ops inserts, and Criterion needs a
        // fresh, freshly pre-filled map per sample (the timed region must start from the same tree
        // depth every time), which itself costs O(prefill).
        group.sample_size(10);

        group.bench_with_input(
            BenchmarkId::new("fingerprint_tree_map", n),
            &n,
            |bencher, &n| {
                bencher.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let arm = prefilled_fingerprint_arm(prefill);
                        total += timed_concurrent_insert(black_box(&arm), n, ops, prefill);
                    }
                    total
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("btree_map", n), &n, |bencher, &n| {
            bencher.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let arm = prefilled_btree_arm(prefill);
                    total += timed_concurrent_insert(black_box(&arm), n, ops, prefill);
                }
                total
            });
        });
    }
    group.finish();
}

criterion_group!(benches, writer_contention);
criterion_main!(benches);

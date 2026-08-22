// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Nonparametric summary statistics for repeated benchmark trials (#455).
//!
//! # Why bootstrap rather than a `t` interval
//!
//! A `t` interval assumes the sampling distribution of the mean is normal. Throughput samples are
//! bounded below by zero and have a long left tail — one descheduled writer halves a trial, nothing
//! symmetrically doubles it — so that assumption is the one thing the data will not supply. The
//! [percentile bootstrap][b] assumes only that the trials are exchangeable draws from whatever
//! distribution the machine produces, which is exactly what a repeated-trials harness guarantees by
//! construction.
//!
//! [b]: https://doi.org/10.1214/aos/1176344552
//!
//! # Determinism
//!
//! Resampling is seeded from [`SEED`], never from entropy: the same trial data must yield the same
//! interval, or a published figure cannot be checked against a re-run of the analysis. The
//! *measurement* is non-deterministic (that is what the intervals are for); the *statistics over
//! it* are not.

use rand::{Rng, SeedableRng};

/// Resamples per bootstrap. 10 000 is the usual floor for a percentile interval — enough that the
/// interval's own Monte-Carlo error is far below the spread it reports.
const RESAMPLES: usize = 10_000;

/// Two-sided interval coverage.
const CONFIDENCE: f64 = 0.95;

/// Fixed resampling seed: see the module docs on determinism.
const SEED: u64 = 20_260_820;

/// One sample's location and spread.
#[derive(Clone, Copy, Debug)]
pub struct Summary {
    /// Arithmetic mean.
    pub mean: f64,
    /// Median. Reported beside the mean because a throughput ratio is right-skewed — a single
    /// descheduled trial drags the mean and leaves the median where it was — so a gap between the
    /// two is itself a reader's signal about the sample.
    pub median: f64,
    /// Lower bound of the [`CONFIDENCE`] percentile-bootstrap interval on the mean.
    pub lo: f64,
    /// Upper bound of the same interval.
    pub hi: f64,
}

impl Summary {
    /// Whether this interval and `other`'s are disjoint.
    ///
    /// Non-overlap implies a difference; overlap implies **nothing** (two 95% intervals can overlap
    /// while the difference of means is still significant). Use [`diff_ci`] to test a difference —
    /// this is only for reporting the interval geometry #455 asks to see.
    pub fn disjoint_from(&self, other: &Summary) -> bool {
        self.hi < other.lo || other.hi < self.lo
    }
}

fn mean(sample: &[f64]) -> f64 {
    sample.iter().sum::<f64>() / sample.len() as f64
}

/// The `q`-quantile of an already-sorted slice, by nearest rank.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let rank = (q * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Draw one bootstrap resample of `sample` and return its mean.
fn resample_mean<R: Rng>(sample: &[f64], rng: &mut R) -> f64 {
    let mut total = 0.0;
    for _ in 0..sample.len() {
        total += sample[rng.gen_range(0..sample.len())];
    }
    total / sample.len() as f64
}

fn percentile_interval(mut means: Vec<f64>) -> (f64, f64) {
    means.sort_by(f64::total_cmp);
    let tail = (1.0 - CONFIDENCE) / 2.0;
    (quantile(&means, tail), quantile(&means, 1.0 - tail))
}

/// Mean, percentile-bootstrap interval and coefficient of variation for one sample.
///
/// # Panics
///
/// If `sample` is empty — a summary of no trials is a caller error, not a value.
pub fn summarize(sample: &[f64]) -> Summary {
    assert!(!sample.is_empty(), "cannot summarize an empty sample");
    let mut rng = rand::rngs::StdRng::seed_from_u64(SEED);
    let point = mean(sample);
    let means = (0..RESAMPLES)
        .map(|_| resample_mean(sample, &mut rng))
        .collect();
    let (lo, hi) = percentile_interval(means);
    let mut sorted = sample.to_vec();
    sorted.sort_by(f64::total_cmp);
    Summary {
        mean: point,
        median: quantile(&sorted, 0.5),
        lo,
        hi,
    }
}

/// Percentile-bootstrap interval on the **difference of means**, `a − b`, resampling each sample
/// independently.
///
/// This is the test to quote when asking whether two points differ: unlike comparing two intervals
/// by eye, an interval excluding zero is a difference at the stated confidence.
///
/// # Panics
///
/// If either sample is empty.
pub fn diff_ci(a: &[f64], b: &[f64]) -> Summary {
    assert!(
        !a.is_empty() && !b.is_empty(),
        "cannot difference an empty sample"
    );
    let mut rng = rand::rngs::StdRng::seed_from_u64(SEED);
    let means = (0..RESAMPLES)
        .map(|_| resample_mean(a, &mut rng) - resample_mean(b, &mut rng))
        .collect();
    let (lo, hi) = percentile_interval(means);
    let point = mean(a) - mean(b);
    Summary {
        mean: point,
        median: f64::NAN,
        lo,
        hi,
    }
}

/// Whether an interval excludes zero — the difference is significant at [`CONFIDENCE`].
pub fn excludes_zero(summary: &Summary) -> bool {
    summary.lo > 0.0 || summary.hi < 0.0
}

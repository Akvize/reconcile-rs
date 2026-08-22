// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Construction and [`RefinementPolicy`](super::RefinementPolicy) for
//! [`EnumerateBelowThreshold`](super::EnumerateBelowThreshold) — Algorithm 1 of
//! arXiv:2603.19820 as written, with its own enumeration cutoff (`|X ∩ [l, u)| ≤ t`) rather than
//! the `shared_cutoffs` the other two shipped policies use.

use super::{Comparison, Decision, FanOut, RefinementPolicy, SplitStride};

/// **IDLIST when `|X ∩ [l, u)| ≤ t`, `SPLITBYRANK(b)` otherwise** — Algorithm 1 of
/// arXiv:2603.19820 as written, replacing this crate's cutoffs as well as its fan-out.
///
/// `t` trades refinement bytes for *values*: a range of `t` local elements ships wholesale,
/// including everything the peer already has.
///
/// # Shipped, not the default — but the verdict is conditional (#468)
///
/// Both halves of that trade, totalled in one unit against [`FixedFanOut`](super::FixedFanOut) at
/// the same `b`, over `t` = 1…256 and value payloads of 8 B…4 KB (`benches/protocol.rs`'s
/// `threshold_sweep`). At n = 10⁵, d = 100 scattered:
///
/// | | |
/// |---|---|
/// | what an element must cost for the paper's `t` = 32 to break even | 15.0 B |
/// | what one costs, as this workspace's caller puts it on the wire | ≥ 30 B: a varint key, a 19-byte `Timestamp`, two framing bytes, then the payload |
///
/// The floor sits above the break-even before the payload contributes a byte, so no `t` recovers
/// what it spends. Over every measured `(n, d)` the best swept value saves 4 %, all of it at an
/// 8-byte payload, and beats the default nowhere from 64 B up; `t` = 32 runs 0.98–1.52× the
/// default's total bytes at 8 B and 5.2–36× at 4 KB.
///
/// **That is a verdict about bytes, and it does not carry to the columns a round trip is paid in.**
/// Against the same baseline, over the eight `(n, d)` `threshold_sweep` covers, `t` = 2b = 32:
///
/// | column | `t` = 2b against the default |
/// |---|---|
/// | refinement bytes | 0.54–0.87× — a win at every `(n, d)` |
/// | advertised ranges | 0.55–0.87× — a win at every `(n, d)` |
/// | one-way messages | 1 to 3 fewer, at every `(n, d)`: one descent level less |
/// | total wire bytes | 0.98–1.52× at `V` = 8 B, 5.2–36× at 4 KB — a loss almost everywhere |
///
/// This is also, exactly, what Negentropy ships: its `splitRange` enumerates as soon as
/// `numElems < 2 · b`, i.e. `t = 2b − 1`, which costs the same as `t = 2b` at every swept case
/// (the span walks the `m / b^k` ladder, so neither `t` picks a different rung). Its counted
/// descent — fewer ranges in fewer messages at the same nominal `b` — is that cutoff and nothing
/// else: `benches/README.md`, "The Negentropy anchor".
///
/// So the choice is bandwidth against latency, and both crossovers are measured rather than
/// argued:
///
/// | crossover | measured figure |
/// |---|---|
/// | the payload `V` below which `t` = 2b wins on **total bytes** | 13.5 B at n = 10³, ≤ 3.4 B at n ≥ 10⁵, and negative — no such payload — at three of the eight cases |
/// | the **RTT** above which the round trips it saves outweigh the bytes it adds, at 1 Gb/s | 0.0–0.5 ms at `V` = 8 B; 0.2–1.6 ms at 4 KB with `d` = 1; 96–108 ms at 4 KB with `d` = 100 |
///
/// **Reach for it when the elements are cheap or the link is dear**: below the first crossover it
/// wins on bytes outright — a set-shaped store (`V = ()`) or a few-byte payload, five of the eight
/// cases here. Above it, it still wins the wall clock past the second crossover — every swept case
/// at 8-byte values, any payload while `d` stays small. **Keep the default when values are large
/// *and* differences are many**: extra elements cost more transmission time than the round trips
/// they buy, and `d`, not `n`, decides it. Both figures assume a lossless link at line rate; under
/// loss the binding term is `reconcile_interval` per lost datagram instead (`SOTA.md` §2.2).
///
/// It ships because the arithmetic, not the conclusion, is what generalizes: a narrower
/// conflict-resolution stamp, a set-shaped store (`V = ()`) or keys dearer than values move the
/// floor, and `t` is a caller's parameter to re-measure against it.
///
/// `t` is a step function rather than a dial: a span walks the ladder `m / b^k`, so every `t`
/// between two rungs picks the same rung and costs exactly the same.
///
/// [`Default`] is the paper's experimental configuration. `t = 0` is raised to `1`, which would
/// otherwise split a range into itself forever.
///
/// ```
/// use rbsr::{Comparison, Decision, EnumerateBelowThreshold, FanOut, RefinementPolicy};
/// use rsos::{Aggregate, Fingerprint};
///
/// let policy = EnumerateBelowThreshold::new(32, FanOut::NEGENTROPY);
///
/// // At or below `t`: IDLIST, whatever the peer's range holds.
/// let at_threshold = Comparison::new(
///     Aggregate::new(32, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(64, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
/// assert_eq!(policy.decide(at_threshold), Decision::Enumerate);
///
/// // One element above `t`: SPLIT instead.
/// let above_threshold = Comparison::new(
///     Aggregate::new(33, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(64, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
/// assert!(matches!(policy.decide(above_threshold), Decision::Split(_)));
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnumerateBelowThreshold {
    threshold: usize,
    fan_out: FanOut,
}

impl EnumerateBelowThreshold {
    /// arXiv:2603.19820 §6's experimental parameters: `t = 32`, `b = 16`.
    pub const PAPER: EnumerateBelowThreshold = EnumerateBelowThreshold {
        threshold: 32,
        fan_out: FanOut::NEGENTROPY,
    };

    /// Enumerate ranges of at most `threshold` local elements, split the rest into at most
    /// `fan_out` children. `0` is raised to `1`.
    pub const fn new(threshold: usize, fan_out: FanOut) -> EnumerateBelowThreshold {
        EnumerateBelowThreshold {
            threshold: if threshold == 0 { 1 } else { threshold },
            fan_out,
        }
    }

    /// `t`: the largest local subset this policy enumerates rather than splits. Never zero.
    pub const fn threshold(&self) -> usize {
        self.threshold
    }

    /// `b`: the branching factor this policy splits at.
    pub const fn fan_out(&self) -> FanOut {
        self.fan_out
    }
}

impl Default for EnumerateBelowThreshold {
    fn default() -> EnumerateBelowThreshold {
        EnumerateBelowThreshold::PAPER
    }
}

impl RefinementPolicy for EnumerateBelowThreshold {
    fn decide(&self, comparison: Comparison) -> Decision {
        if comparison.agrees() {
            // SKIP: `f_X = f_Y`, on the whole aggregate rather than the fingerprint alone.
            Decision::Skip
        } else if comparison.span() <= self.threshold {
            // IDLIST: `|X ∩ [l, u)| ≤ t`, which subsumes `shared_cutoffs` since `t ≥ 1`.
            Decision::Enumerate
        } else {
            // SPLIT: `span > t ≥ 1`, so the stride is below the span and really refines.
            Decision::Split(SplitStride::for_fan_out(comparison.span(), self.fan_out))
        }
    }
}

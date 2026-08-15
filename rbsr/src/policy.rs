// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The refinement-policy seam: [`RefinementPolicy`], the [`Comparison`] it is shown, the
//! [`Decision`] it returns, and the three shipped instantiations.

use rsos::Aggregate;

/// How wide a [`Decision::Split`] cuts: elements **per child range**.
///
/// The primitive, not the fan-out `b` of Algorithm 2 (arXiv:2603.19820) — a stride round-trips
/// through a child count only when it divides the span, so [`SqrtFanOut`]'s cuts would move.
/// [`for_fan_out`](Self::for_fan_out) derives one from a [`FanOut`].
///
/// A zero stride emits no children and would hang the protocol; every constructor raises it to
/// [`ONE`](Self::ONE).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SplitStride(usize);

impl SplitStride {
    /// One element per child. Over a span of one or fewer this is the degenerate split — see
    /// [`Decision::Split`].
    pub const ONE: SplitStride = SplitStride(1);

    /// Cut every `elements` keys. `0` is raised to `1` — see the type-level note.
    pub const fn per_child(elements: usize) -> SplitStride {
        SplitStride(if elements == 0 { 1 } else { elements })
    }

    /// `⌈span / b⌉`: the stride cutting `span` elements into **at most** `fan_out` children —
    /// integer division loses one whenever `span` is not a multiple of the stride.
    pub fn for_fan_out(span: usize, fan_out: FanOut) -> SplitStride {
        SplitStride::per_child(span.div_ceil(fan_out.get()))
    }

    /// The stride, as a plain count of elements. Never zero.
    pub const fn get(self) -> usize {
        self.0
    }
}

/// The paper's branching factor `b` (Algorithm 2, arXiv:2603.19820).
///
/// A fan-out of one is the identity partition and would never terminate; [`new`](Self::new) raises
/// `0` and `1` to `2`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FanOut(usize);

impl FanOut {
    /// `b = 16`: what Negentropy ships and arXiv:2603.19820 §6 measures against.
    pub const NEGENTROPY: FanOut = FanOut(16);

    /// `b = 2`: the smallest fan-out that refines.
    pub const BINARY: FanOut = FanOut(2);

    /// A branching factor. `0` and `1` are raised to `2` — see the type-level note.
    pub const fn new(fan_out: usize) -> FanOut {
        FanOut(if fan_out < 2 { 2 } else { fan_out })
    }

    /// The branching factor, as a plain count of children. Never below two.
    pub const fn get(self) -> usize {
        self.0
    }
}

/// Everything a [`RefinementPolicy`] is shown about one active range: two [`Aggregate`]s reduced
/// to what a policy may soundly read, plus a counter.
///
/// No keys, no bounds, no store — a policy decides *how* to refine a range, never *which*.
///
/// # Law: no fingerprint-derived decisions (#352)
///
/// The skip rule's soundness bound unions a per-comparison collision probability over the ranges
/// an execution compares — legal only because those ranges are cut by `Select`
/// ([`RsosView::select`](crate::RsosView::select)), i.e. by rank, a function of the data alone. A
/// policy that derives a split stride (or any other
/// decision) from a fingerprint byte compiles and converges, but reintroduces the oracle
/// dependence the rank split exists to avoid, voiding that bound silently: no error, no test
/// failure, just an unprovable protocol. [`span`](Self::span) and
/// [`remote_size`](Self::remote_size) are `size()`-only and therefore always safe; `Comparison`
/// carries no accessor that returns a fingerprint or a full [`Aggregate`], so the violation is
/// structurally unspellable rather than merely discouraged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Comparison {
    local: Aggregate,
    remote: Aggregate,
    children_emitted: usize,
}

impl Comparison {
    /// Build a comparison. Public so a policy can be unit-tested without a driver.
    pub const fn new(local: Aggregate, remote: Aggregate, children_emitted: usize) -> Comparison {
        Comparison {
            local,
            remote,
            children_emitted,
        }
    }

    /// `|X ∩ [l, u)|`: **local** elements covered — what `t` is compared against, and what a
    /// [`Decision::Split`] cuts, since a split is by local rank.
    pub const fn span(&self) -> usize {
        self.local.size()
    }

    /// `|Y ∩ [l, u)|`: **remote** elements covered, as advertised. Unauthenticated peer input —
    /// readable, never to be assumed true.
    pub const fn remote_size(&self) -> usize {
        self.remote.size()
    }

    /// Whether the range is already resolved.
    ///
    /// Compares the **whole** aggregate, never the fingerprint alone (`ARCHITECTURE.md` §5
    /// invariant 3). Owned here so no policy can re-derive it wrongly.
    pub fn agrees(&self) -> bool {
        self.local == self.remote
    }

    /// Child ranges already emitted this round: the round-budget seam
    /// (`SOTA.md` §2.4 P3-9).
    ///
    /// Counted in ranges, not bytes — this crate owns no encoding. No shipped policy reads it;
    /// [`RefinementPolicy`] carries a worked capping example.
    pub const fn children_emitted(&self) -> usize {
        self.children_emitted
    }
}

/// What a [`RefinementPolicy`] decides for one active range: Algorithm 1's three outcomes
/// (arXiv:2603.19820 §4), and nothing else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// **SKIP** — the range leaves the active family.
    ///
    /// Returning it for a range that *disagrees* abandons that difference: recoverable under a
    /// periodic driver (`reconcile` is one), permanent data loss under a one-shot drive.
    Skip,

    /// **IDLIST** — enumerate the range locally and ship its contents to the peer.
    ///
    /// This crate's IDLIST is one-directional, so when the peer advertised a **non-empty** range
    /// the driver also bounces the parent back advertised as empty, forcing the peer's own IDLIST.
    /// Required for soundness, and derived by the driver so a policy cannot omit it.
    Enumerate,

    /// **SPLIT** — replace the range by a balanced family of children, cut by local rank
    /// (`SPLITBYRANK`), each re-advertised with this peer's aggregate.
    ///
    /// The driver owns Proposition 4.1's partition invariant whatever stride is chosen
    /// (`ARCHITECTURE.md` §5 invariant 9); it cannot own *progress*. A stride at or above
    /// [`Comparison::span`] emits one child equal to the parent — legitimate, used by every
    /// shipped policy for a lone local element, and terminating only because the peer refines it.
    Split(SplitStride),
}

/// The rule that turns one range comparison into one [`Decision`].
///
/// A **purely local decision, never a wire contract** (`ARCHITECTURE.md` §3.1): peers running
/// different policies converge. A policy must therefore never be advertised or negotiated.
///
/// # The shipped policies
///
/// | policy | enumeration cutoff | fan-out |
/// |---|---|---|
/// | [`FixedFanOut`] (**the default**, `b = 16`) | four hand-picked special cases | a constant `b` |
/// | [`SqrtFanOut`] | the same four | `⌊√m⌋` elements per child, so `Θ(√m)` children |
/// | [`EnumerateBelowThreshold`] | the paper's `\|X ∩ [l, u)\| ≤ t` | a constant `b` |
///
/// Costs: `benches/protocol.rs`. Default and the evidence for it: `PROGRESS.md`.
///
/// # Implementing your own
///
/// `decide` takes `&self`; per-round state arrives through [`Comparison`]. This one caps a round's
/// children, deferring the rest to the next anti-entropy cycle:
///
/// ```
/// use rbsr::{Comparison, Decision, RefinementPolicy, SqrtFanOut};
/// use rsos::{Aggregate, Fingerprint};
///
/// /// At most `max` children per round. Sound only under a periodic driver.
/// struct Budgeted {
///     max: usize,
/// }
///
/// impl RefinementPolicy for Budgeted {
///     fn decide(&self, comparison: Comparison) -> Decision {
///         match SqrtFanOut.decide(comparison) {
///             Decision::Split(_) if comparison.children_emitted() >= self.max => Decision::Skip,
///             decision => decision,
///         }
///     }
/// }
///
/// let mismatch = Comparison::new(
///     Aggregate::new(100, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(100, Fingerprint([2, 0, 0, 0])),
///     512,
/// );
/// assert_eq!(Budgeted { max: 256 }.decide(mismatch), Decision::Skip);
/// ```
pub trait RefinementPolicy {
    /// Classify one active range, once per range per round.
    fn decide(&self, comparison: Comparison) -> Decision;
}

/// The cutoffs [`SqrtFanOut`] and [`FixedFanOut`] share, where a cut by rank cannot help: the peer
/// holds nothing, we hold nothing, or both hold exactly one element. `None` when the fan-out
/// actually matters. [`EnumerateBelowThreshold`] reaches the same outcomes through `t`.
fn shared_cutoffs(comparison: Comparison) -> Option<Decision> {
    let local = comparison.span();
    let remote = comparison.remote_size();
    if comparison.agrees() {
        Some(Decision::Skip)
    } else if remote == 0 {
        Some(Decision::Enumerate)
    } else if local == 0 {
        Some(Decision::Split(SplitStride::ONE))
    } else if local == 1 && remote == 1 {
        Some(Decision::Enumerate)
    } else if local == 1 {
        // One local element cannot be cut by rank: let the peer, which holds more, cut.
        Some(Decision::Split(SplitStride::ONE))
    } else {
        None
    }
}

/// Cut every `⌊√m⌋` elements: `Θ(√m)` children, still a rank-balanced partition.
///
/// Enumeration cutoffs are [`FixedFanOut`]'s (`shared_cutoffs`), plus a lone local element facing
/// a larger remote range, which is re-advertised for the peer to cut.
///
/// **Cost.** A size-derived stride makes the first SPLIT of a whole-store round emit `~√n`
/// children whatever `d` is: communication is `Θ(√n)`, not `O(d log n)`, and the paper's
/// `T_loc = O(hL + bhI + K)` does not apply. It buys depth — `Θ(log log n)` rounds. Competitive
/// only as `d` approaches `√n`; measured in `benches/protocol.rs`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SqrtFanOut;

impl RefinementPolicy for SqrtFanOut {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        // `f32` and truncation are part of the rule: past f32's mantissa `f64` would disagree.
        let stride = (comparison.span() as f32).sqrt() as usize;
        Decision::Split(SplitStride::per_child(stride))
    }
}

/// **The default policy**: `SPLITBYRANK(O_X, l, u, b)` at a constant `b`, with this crate's
/// enumeration cutoffs.
///
/// A constant `b` is what makes the family's published bounds apply: `O(d log n)` communication,
/// `Θ(log_b n)` rounds, `T_loc = O(hL + bhI + K)`.
///
/// [`Default`] is [`FanOut::NEGENTROPY`]. `b` trades three quantities that bottom out separately —
/// bytes and local work follow `b / ln b`, one-way messages fall as `log_b n` to a floor, and the
/// widest round grows linearly in `b` and must fit a datagram. Swept over 2…256 by
/// `benches/protocol.rs`'s `fan_out_sweep`; the chosen value is in `PROGRESS.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedFanOut {
    fan_out: FanOut,
}

impl FixedFanOut {
    /// A policy splitting into at most `fan_out` children per range.
    pub const fn new(fan_out: FanOut) -> FixedFanOut {
        FixedFanOut { fan_out }
    }

    /// The branching factor this policy splits at.
    pub const fn fan_out(&self) -> FanOut {
        self.fan_out
    }
}

impl Default for FixedFanOut {
    fn default() -> FixedFanOut {
        FixedFanOut::new(FanOut::NEGENTROPY)
    }
}

impl RefinementPolicy for FixedFanOut {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        Decision::Split(SplitStride::for_fan_out(comparison.span(), self.fan_out))
    }
}

/// **IDLIST when `|X ∩ [l, u)| ≤ t`, `SPLITBYRANK(b)` otherwise** — Algorithm 1 of
/// arXiv:2603.19820 as written, replacing this crate's cutoffs as well as its fan-out.
///
/// `t` trades refinement bytes for *values*: a range of `t` local elements ships wholesale,
/// including everything the peer already has.
///
/// # Shipped, but not the default
///
/// Both halves of that trade, totalled in one unit against [`FixedFanOut`] at the same `b`, over
/// `t` = 1…256 and value payloads of 8 B…4 KB (`benches/protocol.rs`'s `threshold_sweep`). At
/// n = 10⁵, d = 100 scattered:
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
/// In bytes, that is. What they buy is round trips — one fewer descent level — and a round trip has
/// a price too, so on a link fast and far enough a threshold can win the wall clock it loses on
/// bytes. Both crossovers, and why the default answers the byte question: `PROGRESS.md`.
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

/// Blanket forwarding, so a policy behind a smart pointer is itself a policy.
impl<P: RefinementPolicy + ?Sized> RefinementPolicy for &P {
    fn decide(&self, comparison: Comparison) -> Decision {
        (**self).decide(comparison)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rsos::Fingerprint;

    /// Two aggregates of the given sizes guaranteed not to agree, plus a fresh round budget.
    fn mismatch(local: usize, remote: usize) -> Comparison {
        Comparison::new(
            Aggregate::new(local, Fingerprint([1, 0, 0, 0])),
            Aggregate::new(remote, Fingerprint([2, 0, 0, 0])),
            0,
        )
    }

    /// How many children a stride emits over a span, mirroring the driver's loop.
    fn children(span: usize, stride: SplitStride) -> usize {
        span.div_ceil(stride.get()).max(1)
    }

    #[test]
    fn agreeing_aggregates_are_skipped_by_every_policy() {
        let aggregate = Aggregate::new(1_000, Fingerprint([9, 9, 9, 9]));
        let agreed = Comparison::new(aggregate, aggregate, 0);
        assert_eq!(SqrtFanOut.decide(agreed), Decision::Skip);
        assert_eq!(FixedFanOut::default().decide(agreed), Decision::Skip);
        assert_eq!(
            EnumerateBelowThreshold::PAPER.decide(agreed),
            Decision::Skip
        );
    }

    /// Matching fingerprints with mismatched sizes must not be read as agreement.
    #[test]
    fn matching_fingerprint_with_wrong_size_does_not_agree() {
        let comparison = Comparison::new(Aggregate::new(2, Fingerprint::ZERO), Aggregate::ZERO, 0);
        assert!(!comparison.agrees());
        assert_ne!(SqrtFanOut.decide(comparison), Decision::Skip);
    }

    #[test]
    fn sqrt_fan_out_emits_root_m_children() {
        for span in [100usize, 400, 2_500, 1_000_000] {
            let Decision::Split(stride) = SqrtFanOut.decide(mismatch(span, span)) else {
                panic!("a mismatching range of {span} elements must split");
            };
            assert_eq!(stride.get(), (span as f32).sqrt() as usize);
            let emitted = children(span, stride);
            let root = (span as f64).sqrt() as usize;
            assert!(
                emitted >= root / 2 && emitted <= root * 2,
                "span={span}: {emitted} children, expected ~√span = {root}"
            );
        }
    }

    /// The child count must stop growing with the range.
    #[test]
    fn fixed_fan_out_is_constant_in_the_range_size() {
        let policy = FixedFanOut::default();
        for span in [100usize, 400, 2_500, 1_000_000] {
            let Decision::Split(stride) = policy.decide(mismatch(span, span)) else {
                panic!("a mismatching range of {span} elements must split");
            };
            assert!(
                children(span, stride) <= policy.fan_out().get(),
                "span={span}: {} children exceeds b={}",
                children(span, stride),
                policy.fan_out().get()
            );
            assert!(stride.get() < span, "span={span}: the split must refine");
        }
    }

    #[test]
    fn algorithm1_enumerates_at_or_below_the_threshold_and_splits_above() {
        let policy = EnumerateBelowThreshold::new(32, FanOut::NEGENTROPY);
        for span in [0usize, 1, 31, 32] {
            assert_eq!(policy.decide(mismatch(span, 64)), Decision::Enumerate);
        }
        let Decision::Split(stride) = policy.decide(mismatch(33, 64)) else {
            panic!("a range above the threshold must split");
        };
        assert!(stride.get() < 33);
    }

    /// Neither a zero stride nor a fan-out of one is representable, so neither can hang the
    /// protocol.
    #[test]
    fn degenerate_parameters_are_unrepresentable() {
        assert_eq!(SplitStride::per_child(0), SplitStride::ONE);
        assert_eq!(
            SplitStride::for_fan_out(0, FanOut::BINARY),
            SplitStride::ONE
        );
        assert_eq!(FanOut::new(0), FanOut::BINARY);
        assert_eq!(FanOut::new(1), FanOut::BINARY);
        assert_eq!(
            EnumerateBelowThreshold::new(0, FanOut::BINARY).threshold(),
            1
        );
    }

    #[test]
    fn for_fan_out_never_exceeds_the_requested_branching_factor() {
        for span in [2usize, 3, 5, 9, 10, 17, 1_000, 999_983] {
            for b in [2usize, 3, 16, 64] {
                let fan_out = FanOut::new(b);
                let stride = SplitStride::for_fan_out(span, fan_out);
                assert!(
                    children(span, stride) <= b,
                    "span={span}, b={b}: {} children",
                    children(span, stride)
                );
                assert!(stride.get() < span, "span={span}, b={b}: must refine");
            }
        }
    }
}

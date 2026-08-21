// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The refinement-policy seam: [`RefinementPolicy`], the [`Comparison`] it is shown, the
//! [`Decision`] it returns, and the three shipped instantiations.
//!
//! Split across siblings by concern: `params` owns the two tunable width primitives
//! ([`SplitStride`], [`FanOut`]); `comparison` owns what a policy is shown ([`Comparison`]);
//! `cutoffs` owns the enumeration-cutoff logic [`SqrtFanOut`] and [`FixedFanOut`] share;
//! `sqrt_fan_out`, `fixed_fan_out` and `enumerate_below_threshold` each own one shipped policy's
//! [`RefinementPolicy`] impl; `forwarding` owns the blanket `&P` impl;
//! `fingerprint_derived_split` (`cfg(reconcile_internal_testing)`) owns the test-only oracle-dependent
//! probe. This file keeps the public type definitions (their module location is their
//! `cargo public-api`-visible path — see AGENTS.md §11) plus the module doc above every sibling
//! shares.

use rsos::Aggregate;

mod comparison;
mod cutoffs;
mod enumerate_below_threshold;
#[cfg(reconcile_internal_testing)]
mod fingerprint_derived_split;
mod fixed_fan_out;
mod forwarding;
mod params;
mod sqrt_fan_out;

/// How wide a [`Decision::Split`] cuts: elements **per child range**.
///
/// The primitive, not the fan-out `b` of Algorithm 2 (arXiv:2603.19820) — a stride round-trips
/// through a child count only when it divides the span, so [`SqrtFanOut`]'s cuts would move.
/// [`for_fan_out`](Self::for_fan_out) derives one from a [`FanOut`].
///
/// A zero stride emits no children and would hang the protocol; every constructor raises it to
/// [`ONE`](Self::ONE).
///
/// ```
/// use rbsr::{FanOut, SplitStride};
///
/// // 10 elements split into at most 3 children needs a stride of 4: 4, 4, then 2.
/// assert_eq!(SplitStride::for_fan_out(10, FanOut::new(3)).get(), 4);
///
/// // A stride of zero would never advance, so it is raised to one instead of hanging the protocol.
/// assert_eq!(SplitStride::per_child(0), SplitStride::ONE);
/// ```
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SplitStride(usize);

/// The paper's branching factor `b` (Algorithm 2, arXiv:2603.19820).
///
/// A fan-out of one is the identity partition and would never terminate; [`new`](Self::new) raises
/// `0` and `1` to `2`.
///
/// ```
/// use rbsr::FanOut;
///
/// // Both degenerate inputs are raised to the smallest fan-out that actually refines.
/// assert_eq!(FanOut::new(0), FanOut::new(2));
/// assert_eq!(FanOut::new(1), FanOut::BINARY);
/// assert_eq!(FanOut::NEGENTROPY.get(), 16);
/// ```
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FanOut(usize);

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
///
/// ```
/// use rbsr::Comparison;
/// use rsos::{Aggregate, Fingerprint};
///
/// // Same fingerprint, different size: `agrees()` must not be fooled by a fingerprint collision --
/// // it reads the whole `Aggregate`, never the fingerprint alone.
/// let mismatch = Comparison::new(
///     Aggregate::new(2, Fingerprint::ZERO),
///     Aggregate::new(3, Fingerprint::ZERO),
///     0,
/// );
/// assert!(!mismatch.agrees());
/// assert_eq!(mismatch.span(), 2);
/// assert_eq!(mismatch.remote_size(), 3);
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Comparison {
    local: Aggregate,
    remote: Aggregate,
    children_emitted: usize,
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
    /// (`ARCHITECTURE.md` §5 invariant 10). A stride at or above [`Comparison::span`] emits one
    /// child equal to the parent — legitimate, used by every shipped policy for a lone local
    /// element (`span() <= 1`), terminating only because the peer refines it. For `span() > 1`
    /// that argument is [`RefinementPolicy`]'s progress law, not the driver's to assume: a
    /// `Split` that violates it is converted to an [`Enumerate`](Decision::Enumerate) instead of
    /// reaching the fan-out below (`ARCHITECTURE.md` §5 invariant 13, #420).
    Split(SplitStride),
}

/// The rule that turns one range comparison into one [`Decision`].
///
/// A **purely local decision, never a wire contract** (`ARCHITECTURE.md` §3.1): peers running
/// different policies converge. A policy must therefore never be advertised or negotiated.
///
/// # Law: eventual progress (#420)
///
/// Whenever [`Comparison::span`] is greater than one, `decide` must return a
/// [`Decision::Split`] whose stride is strictly less than the span — a real cut, not the
/// single-child identity split [`Decision::Split`]'s docs carve out for `span() <= 1`. Every
/// shipped policy holds this (pinned by `tests/shipped_policies_always_progress.rs`); #356's
/// `FingerprintDerivedSplit` probe does not, and hung on ~99.5% of drives because of it — a
/// content-determined stride can land a range on a fixed point that never shrinks.
///
/// Breaking the law no longer hangs the driver: `protocol_round_with_policy` converts a
/// non-progressing `Split` into an `Enumerate` rather than trusting a plugged-in policy to hold
/// this itself (`ARCHITECTURE.md` §5 invariant 13). That makes the law non-fatal to violate, not
/// free to — a policy that violates it still pays for every such range in an immediate IDLIST
/// instead of the split it asked for.
///
/// # The shipped policies
///
/// | policy | enumeration cutoff | fan-out |
/// |---|---|---|
/// | [`FixedFanOut`] (**the default**, `b = 16`) | four hand-picked special cases | a constant `b` |
/// | [`SqrtFanOut`] | the same four | `⌊√m⌋` elements per child, so `Θ(√m)` children |
/// | [`EnumerateBelowThreshold`] | the paper's `\|X ∩ [l, u)\| ≤ t` | a constant `b` |
///
/// Costs: `benches/protocol.rs`. Default and the evidence for it: `SOTA.md` §2.2.
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

/// Cut every `⌊√m⌋` elements: `Θ(√m)` children, still a rank-balanced partition.
///
/// Enumeration cutoffs are [`FixedFanOut`]'s (`cutoffs::shared_cutoffs`), plus a lone local
/// element facing a larger remote range, which is re-advertised for the peer to cut.
///
/// **Cost.** A size-derived stride makes the first SPLIT of a whole-store round emit `~√n`
/// children whatever `d` is: communication is `Θ(√n)`, not `O(d log n)`, and the paper's
/// `T_loc = O(hL + bhI + K)` does not apply. It buys depth — `Θ(log log n)` rounds. Competitive
/// only as `d` approaches `√n`; measured in `benches/protocol.rs`.
///
/// ```
/// use rbsr::{Comparison, Decision, RefinementPolicy, SqrtFanOut};
/// use rsos::{Aggregate, Fingerprint};
///
/// let comparison = Comparison::new(
///     Aggregate::new(400, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(400, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
/// let Decision::Split(stride) = SqrtFanOut.decide(comparison) else {
///     panic!("a mismatching range must split");
/// };
/// // The stride is the square root of the span, so -- unlike `FixedFanOut` -- the child count
/// // grows with the range instead of staying capped at a constant `b`.
/// assert_eq!(stride.get(), 20);
/// ```
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SqrtFanOut;

/// **The default policy**: `SPLITBYRANK(O_X, l, u, b)` at a constant `b`, with this crate's
/// enumeration cutoffs.
///
/// A constant `b` is what makes the family's published bounds apply: `O(d log n)` communication,
/// `Θ(log_b n)` rounds, `T_loc = O(hL + bhI + K)`.
///
/// [`Default`] is [`FanOut::NEGENTROPY`]. `b` trades three quantities that bottom out separately —
/// bytes and local work follow `b / ln b`, one-way messages fall as `log_b n` to a floor, and the
/// widest round grows linearly in `b` and must fit a datagram. Swept over 2…256 by
/// `benches/protocol.rs`'s `fan_out_sweep`; the chosen value's evidence is in `SOTA.md` §2.2.
///
/// ```
/// use rbsr::{Comparison, Decision, FanOut, FixedFanOut, RefinementPolicy};
/// use rsos::{Aggregate, Fingerprint};
///
/// let small = Comparison::new(
///     Aggregate::new(100, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(100, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
/// let large = Comparison::new(
///     Aggregate::new(1_000_000, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(1_000_000, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
///
/// // The child count stays at or under `b` however wide the range is -- unlike `SqrtFanOut`,
/// // whose child count grows with it.
/// for comparison in [small, large] {
///     let Decision::Split(stride) = FixedFanOut::default().decide(comparison) else {
///         panic!("a mismatching range must split");
///     };
///     let children = comparison.span().div_ceil(stride.get());
///     assert!(children <= FanOut::NEGENTROPY.get());
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedFanOut {
    fan_out: FanOut,
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
/// bytes. Both crossovers, and why the default answers the byte question: `SOTA.md` §2.2.
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

/// **Test-only probe (#356), `cfg(reconcile_internal_testing)`-gated.** Deliberately violates the law
/// [`Comparison`]'s docs state: it derives its split stride from the **local** aggregate's
/// fingerprint instead of from the range alone, reintroducing the oracle dependence rank-cut
/// refinement exists to avoid — the index set this produces is no longer a deterministic function
/// of the data alone.
///
/// Since #352, `Comparison`'s public API carries no accessor returning a fingerprint, so this
/// cannot be built from outside the crate; it exists only under `cfg(reconcile_internal_testing)`, so a
/// measurement harness in `rbsr/tests/` can still reach it. Never a shipped policy — see this
/// crate's `tests/oracle_dependent_split_vs_the_union_bound.rs` for what it measures.
///
/// Enumeration cutoffs match [`FixedFanOut`]'s (`cutoffs::shared_cutoffs`), so the only variable
/// this isolates is *how the split stride is chosen*, never *when* a range is enumerated instead
/// of split.
#[cfg(reconcile_internal_testing)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FingerprintDerivedSplit;

#[cfg(test)]
mod tests;

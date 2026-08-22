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
//! Split across siblings by concern: `params` owns [`SplitStride`]/[`FanOut`]; `cutoffs` owns the
//! enumeration-cutoff logic [`SqrtFanOut`] and [`FixedFanOut`] share; `sqrt_fan_out`,
//! `fixed_fan_out` and `enumerate_below_threshold` each own one shipped policy's definition and
//! [`RefinementPolicy`] impl; `forwarding` owns the blanket `&P` impl; `fingerprint_derived_split`
//! (`cfg(reconcile_internal_testing)`) owns the test-only oracle-dependent probe's definition and
//! impl. Each is `pub use`-d back into this file, so `cargo public-api`'s reported path (AGENTS.md
//! §11) stays `rbsr::TypeName` regardless of which sibling defines it. `comparison` owns
//! [`Comparison`]'s construction and accessors only: the definition stays in this file so its
//! private fields stay reachable from sibling oracle-dependent probes, not just descendants of
//! `comparison`. This file otherwise keeps [`Decision`] and [`RefinementPolicy`] themselves, plus
//! the module doc every sibling shares.

use rsos::Aggregate;

mod comparison;
#[cfg(reconcile_internal_testing)]
mod constant_stride_split;
mod cutoffs;
mod enumerate_below_threshold;
#[cfg(reconcile_internal_testing)]
mod fingerprint_derived_split;
mod fixed_fan_out;
mod forwarding;
mod params;
#[cfg(reconcile_internal_testing)]
mod span_hashed_stride_split;
#[cfg(reconcile_internal_testing)]
mod span_relative_fingerprint_split;
mod sqrt_fan_out;

#[cfg(reconcile_internal_testing)]
pub use constant_stride_split::ConstantStrideSplit;
pub use enumerate_below_threshold::EnumerateBelowThreshold;
#[cfg(reconcile_internal_testing)]
pub use fingerprint_derived_split::FingerprintDerivedSplit;
pub use fixed_fan_out::FixedFanOut;
pub use params::{FanOut, SplitStride};
#[cfg(reconcile_internal_testing)]
pub use span_hashed_stride_split::{SpanHashedStrideSplit, STRIDE_SPREAD};
#[cfg(reconcile_internal_testing)]
pub use span_relative_fingerprint_split::SpanRelativeFingerprintSplit;
pub use sqrt_fan_out::SqrtFanOut;

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
/// # A fourth policy considered, and not shipped (#318)
///
/// [`span`](Comparison::span)/[`remote_size`](Comparison::remote_size) bound the differing
/// elements in a range from below, so a fan-out keyed off their delta could widen exactly where
/// that bound is nonzero. It never is on the regime that matters: whenever the two peers hold the
/// same key set, `span() == remote_size()` at every depth. A value change doesn't move where a key
/// sits, so this holds for any number of disagreeing keys, not only the single conflict
/// `rbsr/tests/balance_under_position_map.rs`'s
/// `a_key_ordered_conflict_is_balanced_because_the_positions_tie` drives to a fixed point and
/// pins. That is the divergence an LWW register produces continuously (`SOTA.md` §2.1); the regime
/// the delta *does* see — a dropped write, a cold sync, a partition heal — is the rarer one, and
/// there the shipped default already sits within single digits of [`SqrtFanOut`]
/// (`benches/protocol.rs`, `SOTA.md` §2.2). A delta-derived policy would therefore reproduce the
/// default on every steady-state round and differ from it only in a regime where the win is
/// already small. Not built.
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

#[cfg(test)]
mod tests;

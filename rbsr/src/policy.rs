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
//! This module is private — everything public here is re-exported from the crate root, so any
//! documentation a *user* needs belongs on an item, not in this header, which rustdoc never
//! renders.

use rsos::Aggregate;

/// How wide a [`Decision::Split`] cuts: the number of elements **each child range covers**.
///
/// The paper parameterizes the dual quantity — `SPLITBYRANK(O_X, l, u, b)` (Algorithm 2 of
/// E. G. Amparore, arXiv:2603.19820) takes the *number of children* `b` — so both spellings are
/// constructors here: [`per_child`](Self::per_child) states the stride directly,
/// [`for_fan_out`](Self::for_fan_out) derives it from a [`FanOut`].
///
/// The stride is the primitive rather than the fan-out because it is the one that survives the
/// round trip. This crate's historical rule is "cut every `⌊√m⌋` elements", whose child *count*
/// (`⌈m / ⌊√m⌋⌉`) does not in general map back to the same cut positions — expressing it as a
/// fan-out would silently move the cuts, which is exactly what [`SqrtFanOut`] must not do.
///
/// A stride of zero would emit no children at all and hang the protocol, so it is unrepresentable:
/// every constructor raises it to [`ONE`](Self::ONE), the smallest stride that still refines.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SplitStride(usize);

impl SplitStride {
    /// One element per child: the finest cut, `m` children for a range of `m` elements.
    ///
    /// Applied to a range of one element or fewer this is the **degenerate split** — a single
    /// child equal to the parent, re-advertised with this peer's own aggregate. That emits no
    /// local progress and is sound only because the peer holds more there and will refine it; see
    /// the note on [`Decision::Split`].
    pub const ONE: SplitStride = SplitStride(1);

    /// Cut every `elements` keys. `0` is raised to `1` — see the type-level note.
    pub const fn per_child(elements: usize) -> SplitStride {
        SplitStride(if elements == 0 { 1 } else { elements })
    }

    /// The stride that cuts a range of `span` elements into **at most** `fan_out` children:
    /// `⌈span / b⌉`, so every child but the last carries exactly that many elements and the last
    /// absorbs the remainder.
    ///
    /// "At most" rather than "exactly": integer division loses a child whenever `span` is not a
    /// multiple of the stride (`span = 9`, `b = 4` gives a stride of 3 and therefore 3 children,
    /// not 4). Def. 3.8's balanced-partition property — children of near-equal rank width — holds
    /// either way, and it is the property Proposition 4.1's correctness argument uses.
    pub fn for_fan_out(span: usize, fan_out: FanOut) -> SplitStride {
        SplitStride::per_child(span.div_ceil(fan_out.get()))
    }

    /// The stride, as a plain count of elements. Never zero.
    pub const fn get(self) -> usize {
        self.0
    }
}

/// The paper's branching factor `b`: how many child ranges a `SPLITBYRANK` produces (Algorithm 2
/// of arXiv:2603.19820; Negentropy, the paper's production comparison point, uses `b = 16`).
///
/// A fan-out of one is unrepresentable, and not for tidiness: a "1-partition" is the identity, so a
/// policy returning it would replace every range by itself and the protocol would never terminate.
/// [`new`](Self::new) raises it to `2`, the smallest fan-out that actually refines.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FanOut(usize);

impl FanOut {
    /// The branching factor Negentropy ships and arXiv:2603.19820's §6 experiments compare
    /// against: `b = 16`.
    pub const NEGENTROPY: FanOut = FanOut(16);

    /// Binary refinement, `b = 2`: the deepest recursion this crate can be asked for, and the
    /// smallest fan-out that refines at all.
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

/// Everything a [`RefinementPolicy`] is shown about one active range.
///
/// Deliberately narrow: two [`Aggregate`]s and a counter. No keys, no bounds, no store — a policy
/// decides *how* to refine a range, never *which* range, and giving it the bounds would invite
/// key-dependent policies that the two peers could not both apply to the same range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Comparison {
    local: Aggregate,
    remote: Aggregate,
    children_emitted: usize,
}

impl Comparison {
    /// Build a comparison. The protocol driver does this per active range; it is public so a
    /// policy can be unit-tested without one.
    pub const fn new(local: Aggregate, remote: Aggregate, children_emitted: usize) -> Comparison {
        Comparison {
            local,
            remote,
            children_emitted,
        }
    }

    /// `A(X ∩ [l, u))`: this peer's own aggregate over the range.
    pub const fn local(&self) -> Aggregate {
        self.local
    }

    /// `A(Y ∩ [l, u))`: the aggregate the peer advertised for the same range — Def. 3.6's
    /// comparison value, taken at face value. It is unauthenticated peer input: a policy may read
    /// it, but must not assume it describes a set the peer really holds.
    pub const fn remote(&self) -> Aggregate {
        self.remote
    }

    /// `|X ∩ [l, u)|`: how many **local** elements the range covers.
    ///
    /// This is the quantity the paper's enumeration threshold `t` is compared against, and the
    /// number of elements a [`Decision::Split`] has to cut — a split is by *local* rank, so the
    /// peer's count says nothing about where the cuts land.
    pub const fn span(&self) -> usize {
        self.local.size()
    }

    /// Whether the two aggregates agree, i.e. whether the range is already resolved.
    ///
    /// **Compares the whole aggregate, never the fingerprint alone.** A range fingerprint combines
    /// per-element lifts by addition modulo 2²⁵⁶ (`rsos::fingerprint`), so a non-empty range can
    /// legitimately fingerprint to `ZERO` and two different ranges can fingerprint equally;
    /// deciding on the fingerprint alone would alias them and cause silent, permanent divergence.
    /// The check lives here so no policy has to re-derive it — and so none can get it wrong.
    pub fn agrees(&self) -> bool {
        self.local == self.remote
    }

    /// How many child ranges this round has already emitted, before this range was considered.
    ///
    /// This is the round-budget seam. The children of one round travel together in one batch, so
    /// their number is what decides whether the batch fits a datagram — a policy that cannot see
    /// the running total cannot cap its own contribution to it, which is what makes the `√n`
    /// fan-out's datagram cost structural rather than incidental
    /// ([#257](https://github.com/Akvize/reconcile-rs/issues/257), design consideration 2; the
    /// unbounded-output gap is `SOTA.md` §2.4 P3-9). The count is deliberately in *ranges* rather
    /// than bytes: this crate owns no encoding, so bytes are not a quantity it can honestly report.
    ///
    /// No shipped policy reads it yet — capping a round's fan-out trades bytes for round-trips, and
    /// this workspace has no round-trip measurement to price that trade with (every benchmark runs
    /// at RTT ≈ 0, [#280](https://github.com/Akvize/reconcile-rs/issues/280)). See
    /// [`RefinementPolicy`] for a worked capping policy.
    pub const fn children_emitted(&self) -> usize {
        self.children_emitted
    }
}

/// What a [`RefinementPolicy`] decides for one active range: the three outcomes of Algorithm 1
/// (arXiv:2603.19820 §4), and nothing else.
///
/// The set is closed on purpose. A protocol round has exactly three ways to dispose of a range —
/// resolve it, send its contents, or refine it — and every rule this crate has ever implemented is
/// one of the three. There is deliberately no "send our contents *and* make the peer send theirs"
/// variant: that is what [`Enumerate`](Self::Enumerate) already does whenever the peer advertised a
/// non-empty range, which is the only case where it would be *unsound* not to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// **SKIP** — the range is resolved and leaves the active family.
    ///
    /// Correct when the two aggregates agree ([`Comparison::agrees`]). Returning it for a range
    /// that *disagrees* abandons that difference: sound under periodic anti-entropy, where the next
    /// cycle starts over from the outer range and will find it again, and a silent, permanent data
    /// loss under a one-shot drive. `reconcile` is the former; a policy that skips deliberately
    /// should say so in its own documentation.
    Skip,

    /// **IDLIST** — hand the range to the caller to enumerate (Def. 3.9's `Enumerate(l, u)`) and
    /// ship its local contents to the peer.
    ///
    /// If the peer advertised a **non-empty** range, the driver additionally bounces the parent
    /// back advertised as empty, so the peer takes its own IDLIST branch and enumerates its side to
    /// us. That is not an optimization but a soundness requirement: this crate's IDLIST is
    /// one-directional (the paper's is answered by the receiver), so a range disposed of by a bare
    /// enumeration would resolve without the peer's elements ever crossing. The driver derives the
    /// bounce from the peer's advertised size, which is why the two cases are one variant here and
    /// a policy cannot pick the unsound one.
    Enumerate,

    /// **SPLIT** — replace the range by a balanced family of children, cut by local rank
    /// (Algorithm 2's `SPLITBYRANK`), each re-advertised with this peer's aggregate over it.
    ///
    /// The driver owns the invariant Proposition 4.1 needs — the children are pairwise disjoint and
    /// their union is the parent — whatever stride is chosen. What it cannot own is *progress*: a
    /// stride at or above [`Comparison::span`] emits a single child equal to the parent, which
    /// refines nothing locally. That degenerate form is legitimate and used by every shipped policy
    /// (a range holding one local element cannot be cut by rank, so it is re-advertised for the
    /// peer — which holds more there — to cut instead), but it terminates only because the *other*
    /// peer refines it. Two peers that both bounce the same range forever will do exactly that.
    Split(SplitStride),
}

/// The rule that turns one range comparison into one [`Decision`] — the seam this crate's two
/// tuning knobs live behind.
///
/// # Why this is a seam at all
///
/// The refinement policy is a **purely local decision, not a wire contract**. A peer answers
/// whatever segmentation it is asked about, the wire type carries no policy, and Proposition 4.1's
/// soundness argument uses only that a SPLIT's children are pairwise disjoint with union the parent
/// — which the driver guarantees regardless of policy. So two peers running *different* policies
/// still converge, and a policy can be swapped, mixed or A/B-compared without a protocol break.
/// That is unusually cheap ground for experimentation, and the reason this is a trait rather than a
/// constant.
///
/// It is also the reason a policy must **never** be advertised or negotiated: putting it on the
/// wire would turn a free experiment into a protocol break
/// ([#257](https://github.com/Akvize/reconcile-rs/issues/257), design consideration 1).
///
/// # The shipped policies
///
/// | policy | enumeration cutoff | fan-out |
/// |---|---|---|
/// | [`SqrtFanOut`] (the default) | three hand-picked special cases | `⌊√m⌋` elements per child, so `Θ(√m)` children |
/// | [`FixedFanOut`] | same three special cases | a constant `b` |
/// | [`EnumerateBelowThreshold`] | the paper's `\|X ∩ [l, u)\| ≤ t` | a constant `b` |
///
/// Which one is the default, and why it is not the cheapest one on the wire, is on [`SqrtFanOut`].
///
/// # Implementing your own
///
/// `decide` takes `&self`, so a policy is shared, not owned per round; state that varies within a
/// round arrives through [`Comparison`] instead. This example caps how many child ranges one round
/// may emit — the round-budget case [`Comparison::children_emitted`] exists for — by resolving
/// over-budget ranges to the next anti-entropy cycle rather than refining them now:
///
/// ```
/// use rbsr::{Comparison, Decision, RefinementPolicy, SqrtFanOut};
/// use rsos::{Aggregate, Fingerprint};
///
/// /// At most `max` children per round; anything past that waits for the next cycle.
/// ///
/// /// Sound only under a *periodic* driver, which starts over from the outer range every cycle
/// /// and will rediscover the deferred difference. A one-shot drive would lose it.
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
    /// Classify one active range. Called once per range per round, after the driver has computed
    /// the local aggregate and validated the range's bounds.
    fn decide(&self, comparison: Comparison) -> Decision;
}

/// The three exchange-forcing cutoffs [`SqrtFanOut`] and [`FixedFanOut`] share, i.e. everything
/// they decide before the fan-out rule they differ on. Returns `None` when the range is a genuine
/// SPLIT of two or more local elements, which is the only case where the fan-out matters.
///
/// Each of the three is a case where a cut by rank is either impossible or wasteful:
///
/// - **the peer holds nothing here** — everything we hold is the local symmetric difference, so
///   there is nothing left to narrow down: enumerate it;
/// - **we hold nothing here** — there is nothing to cut *by*; `SplitStride::ONE` over an empty span
///   re-advertises the range with our (empty) aggregate, so the peer takes the branch above on its
///   side and enumerates it to us;
/// - **both sides hold exactly one element** — they differ, so each owes the other precisely one
///   element and comparing further cannot save a byte: enumerate, which also asks the peer to.
///
/// The paper reaches the same outcomes through a single threshold `t` instead
/// ([`EnumerateBelowThreshold`]).
fn shared_cutoffs(comparison: Comparison) -> Option<Decision> {
    let local = comparison.span();
    let remote = comparison.remote().size();
    if comparison.agrees() {
        Some(Decision::Skip)
    } else if remote == 0 {
        Some(Decision::Enumerate)
    } else if local == 0 {
        Some(Decision::Split(SplitStride::ONE))
    } else if local == 1 && remote == 1 {
        Some(Decision::Enumerate)
    } else if local == 1 {
        // A single local element cannot be cut by rank: re-advertise the range with our real
        // aggregate and let the peer, which holds more here, do the cutting.
        Some(Decision::Split(SplitStride::ONE))
    } else {
        None
    }
}

/// **This crate's default policy**: cut every `⌊√m⌋` elements, so a range of `m` elements is
/// replaced by `Θ(√m)` children.
///
/// # What it is
///
/// Byte-for-byte the behaviour this crate has always had, now named. The fan-out grows with the
/// range instead of staying constant, and the enumeration cutoffs are three hand-picked special
/// cases rather than the paper's threshold `t` — each one a range a cut by rank cannot usefully
/// narrow:
///
/// - **the peer holds nothing here** — everything we hold is the local symmetric difference, so
///   there is nothing left to locate: [`Enumerate`](Decision::Enumerate);
/// - **we hold nothing here** — there is nothing to cut *by*, so the range is re-advertised with
///   our (empty) aggregate and the peer enumerates it to us;
/// - **both sides hold exactly one element** — they differ, so each owes the other precisely one
///   element and comparing further cannot save a byte: enumerate, which also asks the peer to.
///
/// A fourth case falls out of the third: a lone local element facing a larger remote range is
/// re-advertised rather than enumerated, because one element cannot be split by rank and the peer
/// is the side that can. [`FixedFanOut`] keeps all four and changes only the fan-out;
/// [`EnumerateBelowThreshold`] replaces all four with the paper's single threshold.
///
/// # Why it is the default, when the measurement says it should not be
///
/// Because it is the behaviour every existing cluster already runs, and this seam was built to make
/// changing it a deliberate, reviewable decision rather than a side effect of introducing the seam.
/// It is *not* the default because it is the better rule. On the evidence below it is not.
///
/// Because the stride is derived from the range's own size, the *first* SPLIT of a whole-store
/// round emits ~√n children **whatever the difference size is**: communication is `Θ(√n)`, not the
/// family's `O(d log n)`. The compensation was supposed to sit in the other column — repeated
/// square-rooting bottoms out in `Θ(log log n)` rounds against a fixed `b`'s `Θ(log_b n)` — and
/// `benches/protocol.rs` now prices both columns instead of assuming one. Locating a **single**
/// missing element (`u64` keys, differences scattered):
///
/// | n | this policy | | [`FixedFanOut`] `b = 16` | |
/// |---:|---:|---:|---:|---:|
/// | | bytes | messages | bytes | messages |
/// | 10³ | 2 041 | 6 | 1 701 | 6 |
/// | 10⁴ | 5 395 | 8 | 2 195 | 6 |
/// | 10⁵ | 16 553 | 6 | 2 789 | 6 |
/// | 10⁶ | **53 046** | 8 | **3 834** | 8 |
///
/// **The compensation does not materialize in the range this crate targets.** `Θ(log log n)` beats
/// `Θ(log_16 n)` asymptotically, but at `n = 10⁶` the iterated square root bottoms out in ~4 levels
/// and `log_16 10⁶ ≈ 5`: the two are the same number, and the measured message counts agree. The
/// separation only reaches a factor of two around `n ≈ 10¹²`, far outside what a fully-replicated
/// in-memory store holds. So the `√m` rule pays ~14× the bytes, ~13× the local RSOS queries and
/// ~5× the IP fragments at `n = 10⁶` and buys nothing back that is observable at that size.
///
/// The paper's local cost `T_loc` is the widest gap of all, and it is pure local CPU — no network,
/// so no RTT caveat applies to it. `reconciliation_drive` times the whole two-peer drive at `d = 1`:
///
/// | n | this policy | [`FixedFanOut`] `b = 16` | ratio |
/// |---:|---:|---:|---:|
/// | 10³ | 12.9 µs | 8.2 µs | 1.6× |
/// | 10⁴ | 43.8 µs | 18.2 µs | 2.4× |
/// | 10⁵ | 460 µs | 25.2 µs | 18× |
/// | 10⁶ | **2.10 ms** | **45.0 µs** | **47×** |
///
/// Steeper than the 13× query-count ratio because the queries themselves get dearer: a `√n` fan-out
/// `select`s at ~1 000 spread-out ranks and aggregates over ~1 000 wide ranges per round, touching
/// far more of the tree than a narrow descent, and grows the output `Vec` to ~42 kB of
/// `RangeAggregate` against ~3 kB.
///
/// Two caveats keep this from being a one-line verdict. Every benchmark here runs at RTT ≈ 0
/// ([#280](https://github.com/Akvize/reconcile-rs/issues/280)), so the message column is a *count*
/// rather than a latency — equal counts do mean equal round-trips, but a policy that lost on that
/// column could not be priced. And the gap narrows sharply as the difference grows and scatters: at
/// `d = 100` over 10⁶ elements the two policies are within 7 % of each other, because ~√n ranges
/// stop being overhead once the difference genuinely needs that many. The `√m` rule is worst
/// exactly in the small-`d` regime RBSR exists for.
///
/// What does **not** carry over from the paper is its local-cost bound `T_loc = O(hL + bhI + K)`,
/// whose `bhI` term assumes a constant `b`. Under [`FixedFanOut`] or [`EnumerateBelowThreshold`]
/// that bound is quotable again; under this policy it is not.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SqrtFanOut;

impl RefinementPolicy for SqrtFanOut {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        // NOTE: span ≥ 2 here, so the stride is at least 1 and the split really refines.
        //
        // `f32`, not `f64`, and truncating rather than rounding: this expression *is* the
        // historical rule, and the wire bytes of every existing cluster depend on it giving the
        // same answer. The two differ for spans past f32's 24-bit mantissa.
        let stride = (comparison.span() as f32).sqrt() as usize;
        Decision::Split(SplitStride::per_child(stride))
    }
}

/// The paper's constant branching factor `b`, with this crate's enumeration cutoffs: everything
/// [`SqrtFanOut`] does, except that a SPLIT emits at most `b` children whatever the range's size.
///
/// This is the minimal one-knob change from the default — it swaps the fan-out rule and nothing
/// else — which is what makes it the honest comparison point for the bytes-versus-round-trips trade
/// described on [`SqrtFanOut`]. Communication returns to the family's `O(d log n)` and the paper's
/// `T_loc = O(hL + bhI + K)` becomes quotable. The `Θ(log_b n)` sequential rounds it pays for that
/// are, at `b = 16` and `n ≤ 10⁶`, the same number of rounds the default pays: see the measured
/// table on [`SqrtFanOut`].
///
/// # Choosing `b`
///
/// [`Default`] is [`FanOut::NEGENTROPY`] (`b = 16`), the value Negentropy ships and
/// arXiv:2603.19820 §6 measures against — and, once swept, the value the measurement also lands on.
/// `benches/protocol.rs`'s `fan_out_sweep` runs `b` from 2 to 256; at `n = 10⁶`, `d = 1`:
///
/// | `b` | bytes | one-way messages | widest round | `T_loc` |
/// |---:|---:|---:|---:|---:|
/// | 2 | 2 061 | 22 | 96 B | 25.1 µs |
/// | 4 | **1 960** | 12 | 202 B | **21.8 µs** |
/// | 8 | 2 613 | 10 | 414 B | 33.0 µs |
/// | 16 | 3 834 | 8 | 802 B | 48.5 µs |
/// | 32 | 5 021 | **6** | 1 614 B | 73.3 µs |
/// | 64 | 9 668 | 6 | 3 238 B | 172 µs |
/// | 256 | 25 880 | 6 | 12 982 B | 975 µs |
///
/// The two columns bottom out in different places, so neither picks `b` alone. A `d = 1` descent
/// visits `~log_b n` levels advertising `~b` ranges each, so bytes and local work follow `b / ln b`
/// — minimized near `b = 3` — while messages fall as `log_b n` until they hit a floor of 6 that no
/// larger `b` improves on. Past that floor (`b ≥ 32` at `n = 10⁶`, `b ≥ 16` at `n = 10⁵`) every extra
/// unit of `b` is paid for and buys nothing.
///
/// `b = 16` is the value that is **never worse than [`SqrtFanOut`] on the round-trip axis** in any
/// configuration measured — `n` from 10³ to 10⁶, `d` from 1 to 100, scattered or clustered — while
/// cutting bytes 13.8×, local CPU ~45× and the widest single round 63× at `n = 10⁶`, `d = 1`.
/// `b = 32` saves one further round-trip at `n = 10⁶` only, and pays ~31 % more bytes and ~51 % more
/// CPU for it — including at `n = 10⁵`, where it saves nothing at all.
///
/// If bandwidth rather than latency is the binding constraint — an in-process or unix-socket peer,
/// a metered WAN link — `b = 4` is the optimum on both bytes and CPU, at two extra round-trips.
/// The break-even sits near an RTT of 8 µs at 1 Gb/s, so on anything that is actually a network,
/// `b = 16` is the better end of that trade.
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
/// arXiv:2603.19820 as written, with both of its parameters.
///
/// The name states the knob that distinguishes it; the fan-out half is a constant `b`, exactly
/// [`FixedFanOut`]'s. The two policies above keep this crate's hand-picked enumeration cutoffs and
/// vary only that fan-out. This one replaces the cutoffs too, so it is the point of comparison for
/// "what do our hand-picked special cases actually cost", not just "what does `√m` cost".
///
/// The threshold is where the two shapes differ most: `t` trades refinement round-trips for
/// *values*. A range of `t` local elements is shipped wholesale, and all but the differing ones are
/// elements the peer already has — so a large `t` can move far more bytes than it saves, in a
/// column the refinement-traffic count does not show. `benches/protocol.rs` reports enumerated
/// elements alongside advertised ranges for exactly this reason.
///
/// [`Default`] is the paper's own experimental configuration, `t = 32` and `b = 16`.
///
/// A threshold of zero is raised to one: with `t = 0`, two peers each holding a single differing
/// element in a range would split it into itself forever, neither ever enumerating.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnumerateBelowThreshold {
    threshold: usize,
    fan_out: FanOut,
}

impl EnumerateBelowThreshold {
    /// The parameters arXiv:2603.19820 §6 runs its experiments with: `t = 32`, `b = 16`.
    pub const PAPER: EnumerateBelowThreshold = EnumerateBelowThreshold {
        threshold: 32,
        fan_out: FanOut::NEGENTROPY,
    };

    /// A policy enumerating ranges of at most `threshold` local elements and splitting the rest
    /// into at most `fan_out` children. `threshold` of `0` is raised to `1` — see the type-level
    /// note.
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
            // IDLIST: `|X ∩ [l, u)| ≤ t`. This also subsumes the peer-is-empty and both-sides-hold-
            // one cases that `shared_cutoffs` names separately, since `t ≥ 1`.
            Decision::Enumerate
        } else {
            // SPLIT: `SPLITBYRANK(O_X, l, u, b)`. `span > t ≥ 1`, so the stride is below the span
            // and the range really is refined.
            Decision::Split(SplitStride::for_fan_out(comparison.span(), self.fan_out))
        }
    }
}

/// Blanket forwarding so a `&P`, a `Box<P>` or a `&dyn RefinementPolicy` is itself a policy — the
/// driver takes `&P` with `P: ?Sized`, so this exists for callers holding a policy behind a smart
/// pointer rather than for the driver.
impl<P: RefinementPolicy + ?Sized> RefinementPolicy for &P {
    fn decide(&self, comparison: Comparison) -> Decision {
        (**self).decide(comparison)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rsos::Fingerprint;

    /// Two aggregates of the given sizes that are guaranteed *not* to agree, plus a fresh round
    /// budget. The fingerprints are arbitrary and distinct; what matters is that no policy may
    /// conclude SKIP.
    fn mismatch(local: usize, remote: usize) -> Comparison {
        Comparison::new(
            Aggregate::new(local, Fingerprint([1, 0, 0, 0])),
            Aggregate::new(remote, Fingerprint([2, 0, 0, 0])),
            0,
        )
    }

    /// How many children a stride emits over a span, mirroring the driver's loop: every child but
    /// the last carries `stride` elements and the last absorbs the remainder.
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

    /// Matching fingerprints with mismatched sizes must not be read as agreement — the aliasing
    /// hazard `Comparison::agrees` exists to close.
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

    /// The one-knob difference from the default: the child count stops growing with the range.
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

    /// The two knobs are validated by their types, not by their callers: neither a zero stride
    /// (which emits no children) nor a fan-out of one (which replaces a range by itself) is
    /// representable, so neither can hang the protocol.
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

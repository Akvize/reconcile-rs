// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`SplitStride`] and [`FanOut`]: the two tunable width primitives a [`RefinementPolicy`] chooses
//! between — a child count (`FanOut`, the paper's `b`) and the per-child element count that
//! realizes it (`SplitStride`).
//!
//! [`RefinementPolicy`]: super::RefinementPolicy

/// How wide a [`Decision::Split`](super::Decision::Split) cuts: elements **per child range**.
///
/// The primitive, not the fan-out `b` of Algorithm 2 (arXiv:2603.19820) — a stride round-trips
/// through a child count only when it divides the span, so [`SqrtFanOut`](super::SqrtFanOut)'s
/// cuts would move. [`for_fan_out`](Self::for_fan_out) derives one from a [`FanOut`].
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

impl SplitStride {
    /// One element per child. Over a span of one or fewer this is the degenerate split — see
    /// [`Decision::Split`](super::Decision::Split).
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

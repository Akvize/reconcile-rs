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

use super::{FanOut, SplitStride};

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

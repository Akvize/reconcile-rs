// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The sliding out-of-order acceptance window, relative to a peer's high-water sequence number.

use super::WINDOW_SIZE;

/// Fixed-size out-of-order acceptance window: bit `i` records that `high_water - i` was accepted,
/// for `i < WINDOW_SIZE`.
///
/// Bit 0 set — the high-water sequence itself — is an invariant every mutator here maintains, so
/// no caller has to.
#[derive(Debug)]
pub(super) struct SlidingBitmap([u64; (WINDOW_SIZE / 64) as usize]);

impl SlidingBitmap {
    /// A fresh window with only the high-water sequence itself marked.
    pub(super) fn new() -> Self {
        let mut bitmap = SlidingBitmap([0u64; (WINDOW_SIZE / 64) as usize]);
        bitmap.mark(0);
        bitmap
    }

    /// Mark `offset` positions behind the high-water mark as seen. Precondition:
    /// `offset < WINDOW_SIZE`.
    pub(super) fn mark(&mut self, offset: u64) {
        let word = (offset / 64) as usize;
        let bit = offset % 64;
        self.0[word] |= 1 << bit;
    }

    /// Whether `offset` has been marked seen. Same precondition as [`mark`](Self::mark).
    pub(super) fn is_marked(&self, offset: u64) -> bool {
        let word = (offset / 64) as usize;
        let bit = offset % 64;
        self.0[word] & (1 << bit) != 0
    }

    /// Advance the high-water mark by `delta`, discarding bits past `WINDOW_SIZE - 1`, and mark
    /// the new high-water sequence as seen.
    pub(super) fn advance(&mut self, delta: u64) {
        if delta >= WINDOW_SIZE {
            *self = Self::new();
            return;
        }
        let word_shift = (delta / 64) as usize;
        let bit_shift = (delta % 64) as u32;
        let words = (WINDOW_SIZE / 64) as usize;

        if bit_shift == 0 {
            for i in (0..words).rev() {
                self.0[i] = if i >= word_shift {
                    self.0[i - word_shift]
                } else {
                    0
                };
            }
        } else {
            for i in (0..words).rev() {
                let lo = if i >= word_shift {
                    self.0[i - word_shift] << bit_shift
                } else {
                    0
                };
                let hi = if i > word_shift {
                    self.0[i - word_shift - 1] >> (64 - bit_shift)
                } else {
                    0
                };
                self.0[i] = lo | hi;
            }
        }
        self.mark(0);
    }
}

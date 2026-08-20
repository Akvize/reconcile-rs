// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `rsos`: a Range-Summarizable Order-Statistics Store.
//!
//! Amparore, *Range-Based Set Reconciliation via Range-Summarizable Order-Statistics Stores*
//! (arXiv:2603.19820): [`Rsos`] is Def. 3.9's seven-operation contract, [`Aggregate`] the Def. 3.5
//! bundled aggregate `A(S) = (|S|, Σ(S))`, [`lift`] the Def. 3.4 lifting function into `M` =
//! [`Fingerprint`]. [`FingerprintTreeMap`] is this crate's realization: in-memory, `ArrayVec`-node
//! B-tree of order 6, meeting Thm. 5.2's `O(h)` bounds.
//!
//! [`lift`]'s element bound is [`Serialize`](serde::Serialize), not [`Hash`](std::hash::Hash) —
//! bytes come from this crate's [canonical encoding](encoding).
//!
//! Def. 3.4 instantiated for a *map*: `U` is the key set, `V` is the payload the lift consults, so
//! `lift(&k, &v)` is total on `X` because a map assigns one value per key. A set-only RSOS is
//! `V = ()`. Two values under one key therefore differ in fingerprint but agree in count.
//!
//! Positioning, competitors, and the open design axes (generic summary monoid, persistence):
//! `SOTA.md` §2.2/§2.4, `ARCHITECTURE.md` §7.
//!
//! | Def. 3.9 operation | [`Rsos`] trait method | [`FingerprintTreeMap`] inherent method |
//! |---|---|---|
//! | `size()` | [`Rsos::size`] | [`len`](FingerprintTreeMap::len) *(+ [`is_empty`](FingerprintTreeMap::is_empty))* |
//! | `Aggregate(l, u)` | [`Rsos::aggregate`] | [`aggregate`](FingerprintTreeMap::aggregate) |
//! | `Rank(z)` | [`Rsos::rank`] | [`rank`](FingerprintTreeMap::rank) |
//! | `Select(r)` | [`Rsos::select`] | [`select`](FingerprintTreeMap::select) |
//! | `Enumerate(l, u)` | [`Rsos::enumerate`] | [`range`](FingerprintTreeMap::range) |
//! | `Insert(k, v)` | [`Rsos::insert`] | [`insert`](FingerprintTreeMap::insert) |
//! | `Delete(k)` | [`Rsos::delete`] | [`remove`](FingerprintTreeMap::remove) |
//!
//! The trait speaks the paper's vocabulary, the inherent API Rust's; where the two collide on a
//! name Rust already owns ([`len`](FingerprintTreeMap::len), `remove`,
//! [`range`](std::collections::BTreeMap::range) vs [`Iterator::enumerate`]), Rust wins on the
//! inherent surface.
//!
//! `U = K`: the paper's replica state `X ⊆ U` is this crate's key set, and the lift is total on it
//! because a map assigns exactly one value per key — `V` is the payload the lift consults, not a
//! second element dimension. A set-only RSOS is the degenerate case `V = ()`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod aggregate;
// Public only under `--cfg reconcile_internal_testing` (#330, AGENTS.md §6), which a dependent's
// build cannot set — so this is a seam no consumer can reach and 1.0 never freezes. See the
// module's own docs.
#[cfg(reconcile_internal_testing)]
pub mod counters;
#[cfg(not(reconcile_internal_testing))]
mod counters;
pub mod encoding;
pub mod fingerprint;
pub mod fingerprint_tree_map;
pub mod fingerprint_tree_map_iter;
mod rsos_trait;

pub use aggregate::Aggregate;
pub use fingerprint::{digest, lift, Fingerprint};
pub use fingerprint_tree_map::{Entry, FingerprintTreeMap, ItemRange};
pub use fingerprint_tree_map_iter::{IntoIter, IntoKeys, IntoValues, Iter, Keys, Values};
pub use rsos_trait::Rsos;

// Re-exported so a third party building a `lift`-compatible `Fingerprint` from raw bytes (via
// `Fingerprint::from_le_bytes` and `encoding::Sink for blake3::Hasher`) never needs its own,
// independently-versioned `blake3` dependency.
pub use blake3;

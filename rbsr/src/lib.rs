// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `rbsr`: Range-Based Set Reconciliation (Meyer, arXiv:2212.13567, IEEE SRDS 2023; over an RSOS
//! backend, Amparore, arXiv:2603.19820 §4).
//!
//! Mechanism only — no sockets, no encoding, no scheduling, no notion of a value.
//! [`initial_ranges`] emits the outer range `(−∞, +∞)`; [`protocol_round`] answers one round of
//! active ranges; the caller alternates rounds between the peers until nothing is left.
//!
//! | Paper (§4, Def. 3.3, Algorithm 1) | This crate |
//! |---|---|
//! | active ranges | `Vec<`[`RangeAggregate`]`>` — [`initial_ranges`]' return, [`protocol_round`]'s `active_ranges` |
//! | outer range | [`initial_ranges`], fixed to `(−∞, +∞)` |
//! | protocol round | one [`protocol_round`] call |
//! | SKIP | the range is written to neither output vector |
//! | IDLIST | an [`EnumerationRange`] in `enumeration_ranges`, enumerated by the caller ([`rsos::Rsos::enumerate`]) |
//! | SPLIT / child ranges | [`RangeAggregate`]s in `child_ranges`, each carrying this peer's [`rsos::Aggregate`] |
//! | unresolved | what the caller has yet to feed back into [`protocol_round`] |
//! | symmetric difference `Δ(X, Y)` (Def. 3.3) | the union of everything reported through `enumeration_ranges` on both sides |
//! | local symmetric difference `Δ_{l,u}(X, Y)` | one [`EnumerationRange`] `(l, u)` |
//! | balanced `b`-partition (Def. 3.8), cut by `Rank`/`Select` | the fan-out inside [`protocol_round`], cut by [`RsosView::select`], width chosen by a [`RefinementPolicy`] |
//! | comparison value `f_Y` (Def. 3.6) | a [`RangeAggregate`]'s whole [`rsos::Aggregate`] — equality on `(fingerprint, size)`, never the fingerprint alone |
//! | Algorithm 1's `t` and `b` | the two knobs of a [`RefinementPolicy`] |
//!
//! The default [`FixedFanOut`] takes `b` as written (16, Negentropy's) and replaces `t` with four
//! special cases listed on [`SqrtFanOut`], so the family's published bounds describe it. Dropping
//! `t` is a measured choice rather than an omission: an enumerated element costs more on this wire
//! than the refinement any threshold saves — [`EnumerateBelowThreshold`].
//!
//! **The refinement policy is local and never negotiated** (`ARCHITECTURE.md` §3.1): peers running
//! different policies converge, so swapping one is a behaviour change, never a wire break.
//! [`protocol_round_with_policy`] takes the seam; [`FixedFanOut`] (default), [`SqrtFanOut`]
//! (`⌊√m⌋`, `Θ(√n)` communication, `Θ(log log n)` rounds) and [`EnumerateBelowThreshold`]
//! (Algorithm 1 as written) ship, priced against each other in `benches/protocol.rs`.
//!
//! [`RsosView`] is four of Def. 3.9's five queries -- `Enumerate` stays with the caller, see the
//! IDLIST row above -- blanket-implemented for
//! every [`rsos::Rsos`], so any backend works with no per-type code here.
//!
//! ```
//! use rsos::FingerprintTreeMap;
//! use rbsr::{initial_ranges, protocol_round};
//!
//! let mut a = FingerprintTreeMap::new();
//! let mut b = FingerprintTreeMap::new();
//! for i in 0..20 {
//!     a.insert(i, i);
//!     b.insert(i, i);
//! }
//! a.insert(999, 999); // only `a` has this key
//!
//! // Alternate rounds between the two sides until nothing is left to resolve -- this is the
//! // driving loop a transport layer (like `reconcile`'s gossip adapter) runs over the wire.
//! let mut active = initial_ranges(&a);
//! let (mut responder, mut advertiser) = (&b, &a);
//! let mut enumerated = 0;
//! while !active.is_empty() {
//!     let mut children = Vec::new();
//!     let mut enumerations = Vec::new();
//!     let outcome = protocol_round(responder, active, &mut children, &mut enumerations);
//!     enumerated += outcome.enumerated();
//!     active = children;
//!     std::mem::swap(&mut responder, &mut advertiser);
//! }
//!
//! // The one-element difference was found.
//! assert!(enumerated > 0);
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod policy;
mod protocol;
mod rsos_view;

#[cfg(reconcile_internal_testing)]
pub use policy::FingerprintDerivedSplit;
pub use policy::{
    Comparison, Decision, EnumerateBelowThreshold, FanOut, FixedFanOut, RefinementPolicy,
    SplitStride, SqrtFanOut,
};
pub use protocol::{
    initial_ranges, protocol_round, protocol_round_with_policy, EnumerationRange, RangeAggregate,
    RoundOutcome,
};
pub use rsos_view::RsosView;

// Re-exported so a caller building a `RangeAggregate` (whose `aggregate` field is an
// `rsos::Aggregate`) never needs its own, independently-versioned dependency on `rsos` — the same
// reasoning #297 applied one crate up.
pub use rsos;

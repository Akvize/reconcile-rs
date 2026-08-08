// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `rbsr`: Range-Based Set Reconciliation.
//!
//! # What RBSR is
//!
//! Two peers each hold a set of keys and want to learn, with as few round-trips and as little
//! traffic as possible, exactly where the two sets differ — without shipping either set wholesale.
//! RBSR does this by exchanging *range aggregates*: a peer advertises a range paired with the
//! bundled `(fingerprint, size)` aggregate over it ([`RangeAggregate`]); the receiver recomputes
//! the same aggregate over its own copy of that range; ranges that agree are resolved, ranges whose
//! contents must be sent outright are reported as [`EnumerationRange`]s, and everything in between
//! is split into child ranges and bounced back for another round. Traffic is proportional to the
//! size of the *difference*, not the size of the sets.
//!
//! The algorithm and the name are from A. Meyer, *Range-Based Set Reconciliation*
//! (arXiv:2212.13567, IEEE SRDS 2023). [`initial_ranges`] emits the outer range covering
//! `(−∞, +∞)`; [`protocol_round`] performs one protocol round; a caller alternates rounds between
//! the two peers until nothing is left to compare. This crate is the mechanism only — it owns no
//! sockets, no encoding, no scheduling, and no notion of what a "value" is.
//!
//! # Paper vocabulary ↔ this crate
//!
//! The protocol layer is described in §4 ("RBSR over RSOS") of E. G. Amparore, *Range-Based Set
//! Reconciliation via Range-Summarizable Order-Statistics Stores* (arXiv:2603.19820), the same
//! paper this workspace cites for Def. 3.4/3.5/3.9. The table below maps its terms onto the items
//! here — the same service [`rsos::FingerprintTreeMap`]'s documentation performs for Def. 3.9's
//! seven operations.
//!
//! | Paper (§4, Def. 3.3, Algorithm 1) | This crate |
//! |---|---|
//! | **active ranges** — "a finite family of pairwise disjoint active ranges", the ranges still to be answered | one `Vec<`[`RangeAggregate`]`>`: the value returned by [`initial_ranges`], and the `active_ranges` argument of [`protocol_round`] |
//! | **outer range** — "one application-defined outer range covering the part of the universe to be reconciled" | [`initial_ranges`], which fixes it to the whole universe, `(−∞, +∞)` |
//! | **protocol round** — "each peer applying Algorithm 1 to the active ranges it is asked to answer" | one call to [`protocol_round`]; a caller alternates them between the two peers |
//! | **SKIP** — comparison values match, the range is resolved | [`protocol_round`] writes the range to neither output vector |
//! | **IDLIST** — "one peer explicitly sends the ordered contents of its local subset on `[l, u)`" | an [`EnumerationRange`] pushed to [`protocol_round`]'s `enumeration_ranges`; the caller enumerates it (Def. 3.9's `Enumerate`, [`rsos::Rsos::enumerate`]) and ships the contents |
//! | **child ranges** / **SPLIT** — the parent is "replaced by a balanced family of child ranges" | [`RangeAggregate`]s pushed to [`protocol_round`]'s `child_ranges`, each carrying this peer's [`rsos::Aggregate`] over it, to be bounced back |
//! | **resolved** | a range that leaves the active family: SKIPped, or enumerated and compared by the receiver |
//! | **unresolved** — "the active ranges always form a partition of the still unresolved portion" | whatever is still in flight: what the caller has yet to feed back into [`protocol_round`] |
//! | **symmetric difference** `Δ(X, Y) := (X \ Y) ∪ (Y \ X)` (Def. 3.3) | what a full run computes: the union of everything reported through `enumeration_ranges` on both sides |
//! | **local symmetric difference** `Δ_{l,u}(X, Y)` — `Δ` restricted to `[l, u)` | what a single [`EnumerationRange`] `(l, u)` stands for on the peer that emitted it |
//! | **balanced `b`-partition** (Def. 3.8), cut by `Rank`/`Select` (Algorithm 2) | the fan-out inside [`protocol_round`], cut by [`RsosView::select`] — but with a `√n` step rather than a fixed `b`; see below |
//! | **comparison value** `f_Y = fp(A(Y ∩ [l, u)))` (Def. 3.6) | the [`rsos::Aggregate`] carried by a [`RangeAggregate`] — the *whole* aggregate, not a hash of it: equality is decided on `(fingerprint, size)`, never on the fingerprint alone |
//!
//! **This crate instantiates the protocol; it is not a transcription of Algorithm 1.** Two decision
//! rules deliberately differ — there is no enumeration threshold `t`, and the SPLIT fan-out grows
//! as `√n` instead of a fixed branching factor `b`. Both are spelled out, with what they do and do
//! not preserve of the paper's guarantees, on [`protocol_round`] itself. Anyone
//! comparing this code against the paper line by line should read that note first.
//!
//! # Generic over any RSOS backend
//!
//! RBSR needs only four read-only queries against the local set, and this crate takes them as a
//! trait, [`RsosView`], rather than as one concrete data structure. Those four queries are exactly
//! four of the seven operations of a **Range-Summarizable Order-Statistics Store (RSOS)**, Def. 3.9
//! of E. G. Amparore, *Range-Based Set Reconciliation via Range-Summarizable Order-Statistics
//! Stores* (arXiv:2603.19820) — the paper that frames RBSR as an algorithm over *any* RSOS backend
//! rather than over one specific tree. The sibling [`rsos`] crate's root documentation covers that
//! framing, the full seven-operation contract, and its citations in full.
//!
//! [`RsosView`] is blanket-implemented for every [`rsos::Rsos`] implementor, so any RSOS backend —
//! [`rsos::FingerprintTreeMap`] today, a persistent or content-addressed store tomorrow — works with
//! this crate for free, with no per-type implementation here.

// The entire crate is implemented in safe Rust; this turns any `unsafe` block into a hard
// compile error.
#![forbid(unsafe_code)]

mod protocol;
mod rsos_view;

pub use protocol::{initial_ranges, protocol_round, EnumerationRange, RangeAggregate};
pub use rsos_view::RsosView;

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
//! the same aggregate over its own copy of that range; ranges that agree are pruned, ranges that
//! clearly differ are reported as
//! [`DiffRange`]s, and everything in between is split into sub-ranges and bounced back for another
//! round. Traffic is proportional to the size of the *difference*, not the size of the sets.
//!
//! The algorithm and the name are from A. Meyer, *Range-Based Set Reconciliation*
//! (arXiv:2212.13567, IEEE SRDS 2023). [`start_diff`] emits the bootstrap segment covering
//! `(−∞, +∞)`; [`diff_round`] performs one refinement round; a driver alternates rounds between the
//! two peers until nothing is left to compare. This crate is the mechanism only — it owns no
//! sockets, no encoding, no scheduling, and no notion of what a "value" is.
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

mod diff;
mod rsos_view;

pub use diff::{diff_round, start_diff, DiffRange, RangeAggregate};
pub use rsos_view::RsosView;

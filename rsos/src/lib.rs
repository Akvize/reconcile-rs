// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `rsos`: a Range-Summarizable Order-Statistics Store.
//!
//! # What an RSOS is
//!
//! An RSOS is an *abstract interface*, not one particular data structure. The term and its
//! seven-operation contract come from E. G. Amparore, *Range-Based Set Reconciliation via
//! Range-Summarizable Order-Statistics Stores* (arXiv:2603.19820):
//!
//! - **Def. 3.9** defines the RSOS contract itself — `size`, `Aggregate`, `Rank`, `Select`,
//!   `Enumerate`, `Insert`, `Delete` — which this crate's [`Rsos`] trait implements literally,
//!   method-for-method. Being an interface, RSOS admits multiple realizations: the paper itself
//!   discusses both an in-memory augmented tree and AELMDB, a persistent, content-addressed
//!   backend, as two realizations of the *same* abstraction, chosen for different deployment
//!   constraints (in-memory speed vs. durability/content-addressing). This crate ships exactly
//!   one realization, [`FingerprintTree`] — an in-memory augmented tree, closer in spirit to the
//!   paper's first example.
//! - **Def. 3.5** defines the *bundled aggregate* `A(S) = (|S|, Σ(S))` — and defines it as a
//!   monoid `A := (ℕ×M, ⊗, (0, 0_M))`, not as a loose pair: one query answering both "how many
//!   elements" and "what is their combined summary" in a single pass, rather than two independent
//!   queries. [`Aggregate`] is that monoid as a named type, and
//!   [`FingerprintTree::aggregate`] (and [`Rsos::aggregate`], its trait form) realize the query
//!   directly — the tree already caches `tree_size` and `tree_hash` together, in lockstep, on
//!   every node, so bundling them into one walk is a genuine implementation of Def. 3.5, not just
//!   an API convenience.
//! - **Thm. 5.2** gives the complexity bounds an aggregate-augmented B+-tree realization achieves:
//!   `Rank`/`Select` in `O(h)`, `Aggregate` in `O(Bh) = O(h)` under bounded page size,
//!   `Enumerate` in `O(h + k)`, `Insert`/`Delete` in `O(h)` (`h` = tree height, `B`/page size
//!   bounded by the node capacity). [`FingerprintTree`] is a from-scratch `ArrayVec`-node B-tree
//!   of order 6 and matches these bounds.
//!
//! # `FingerprintTree`: this crate's one realization
//!
//! Neither the 2023 Meyer paper that originated this per-node-cached-subtree-fingerprint
//! structure (arXiv:2212.13567, and its informal companion repository,
//! `github.com/AljoschaMeyer/set-reconciliation`) nor the 2026 RSOS paper above gives this exact
//! synthesis (a self-balancing B-tree caching a per-subtree fingerprint *and* size) a name of its
//! own — Meyer's own text describes it only as "a self-balancing binary search tree" caching a
//! per-node subtree fingerprint. `FingerprintTree` is not invented in isolation here: an
//! independent third-party implementation citing Meyer's thesis,
//! `github.com/earthstar-project/range-reconcile` (TypeScript), converged on exactly this name for
//! exactly this structure. `*BTree`/`*Augmented*`-flavored names were deliberately avoided: this
//! structure has zero dependency on `std::collections::BTreeMap` — it's a from-scratch
//! `ArrayVec`-node B-tree — so a `BTree`-flavored name would misleadingly imply kinship with the
//! std type that doesn't exist.
//!
//! `FingerprintTree` implements [`Rsos<K, V>`], delegating each trait method to the matching
//! inherent method, and *also* keeps its own pre-existing, Rust-idiomatic inherent API (`len`,
//! `insert`, `get_range`, `is_empty`, iterators, ...) side by side — the trait is one more way to
//! call it, not the only way.
//!
//! # Honest gaps (out of scope here, future work)
//!
//! - **No generic-monoid summary.** The bundled aggregate is hardwired to [`Fingerprint`]: a
//!   256-bit BLAKE3 accumulator combined by addition (an abelian group, so also a commutative
//!   monoid), not a caller-supplied arbitrary commutative monoid. The Meyer/Willow-ecosystem
//!   reference implementation (`earthstar-project/range-reconcile`) names the right extension
//!   point for this future work `BYOLiftingMonoid` (`lift`/`combine`/`neutral`) — "lifting monoid"
//!   is the term to reach for if/when this crate grows a generic summary type; not implemented
//!   now.
//! - **No persistence.** [`FingerprintTree`] is purely in-memory; a durable, content-addressed
//!   realization (what the 2026 paper's AELMDB is) is not attempted here.
//! - One design choice worth being explicit and positive about: [`Fingerprint`] is a *full*
//!   256-bit BLAKE3 accumulator, never truncated. This sits on the sound side of a tradeoff the
//!   2026 paper's §6.1 discusses explicitly: Negentropy's comparator truncates SHA-256 to 128
//!   bits, which the paper describes as only "probabilistically sound" rather than
//!   information-theoretically exact. `rsos` does not make that tradeoff.

pub mod aggregate;
pub mod fingerprint;
pub mod hrtree;
pub mod hrtree_iter;
mod rsos_trait;

pub use aggregate::Aggregate;
pub use fingerprint::{hash, Fingerprint};
pub use hrtree::FingerprintTree;
pub use hrtree_iter::{IntoIter, IntoKeys, IntoValues, Iter, Keys, Values};
pub use rsos_trait::Rsos;

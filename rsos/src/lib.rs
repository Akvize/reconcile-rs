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
//!   one realization, [`FingerprintTreeMap`] — an in-memory augmented tree, closer in spirit to the
//!   paper's first example.
//! - **Def. 3.5** defines the *bundled aggregate* `A(S) = (|S|, Σ(S))` — and defines it as a
//!   monoid `A := (ℕ×M, ⊗, (0, 0_M))`, not as a loose pair: one query answering both "how many
//!   elements" and "what is their combined summary" in a single pass, rather than two independent
//!   queries. [`Aggregate`] is that monoid as a named type, and
//!   [`FingerprintTreeMap::aggregate`] (and [`Rsos::aggregate`], its trait form) realize the query
//!   directly — every node caches its subtree's [`Aggregate`] as one value, so bundling the two
//!   halves into one walk is a genuine implementation of Def. 3.5, not just an API convenience.
//! - **Def. 3.4** defines the *lifting function* `lift: U → M` from an element to the summary
//!   monoid `M`. [`lift`] is that function, and [`Fingerprint`] is that `M`.
//! - **Thm. 5.2** gives the complexity bounds an aggregate-augmented B+-tree realization achieves:
//!   `Rank`/`Select` in `O(h)`, `Aggregate` in `O(Bh) = O(h)` under bounded page size,
//!   `Enumerate` in `O(h + k)`, `Insert`/`Delete` in `O(h)` (`h` = tree height, `B`/page size
//!   bounded by the node capacity). [`FingerprintTreeMap`] is a from-scratch `ArrayVec`-node B-tree
//!   of order 6 and matches these bounds.
//!
//! # `FingerprintTreeMap`: this crate's one realization
//!
//! ## Where the name comes from
//!
//! Neither the 2023 Meyer paper that originated this per-node-cached-subtree-fingerprint
//! structure (arXiv:2212.13567, and its informal companion repository,
//! `github.com/AljoschaMeyer/set-reconciliation`) nor the 2026 RSOS paper above gives this exact
//! synthesis (a self-balancing B-tree caching a per-subtree fingerprint *and* size) a name of its
//! own — Meyer's own text describes it only as "a self-balancing binary search tree" caching a
//! per-node subtree fingerprint. The `FingerprintTree` stem is not invented in isolation here: an
//! independent third-party implementation citing Meyer's thesis,
//! `github.com/earthstar-project/range-reconcile` (TypeScript), converged on exactly that name for
//! exactly this structure. `*BTree`/`*Augmented*`-flavored names were deliberately avoided: this
//! structure has zero dependency on `std::collections::BTreeMap` — it's a from-scratch
//! `ArrayVec`-node B-tree — so a `BTree`-flavored name would misleadingly imply kinship with the
//! std type that doesn't exist.
//!
//! The `Map` suffix is this crate's own addition to that sourced stem, and it is what Rust
//! convention requires rather than decoration. `FingerprintTreeMap` decomposes as `Fingerprint` +
//! `TreeMap` exactly as std's `BTreeMap` decomposes as `B` + `TreeMap`: the qualifier says what
//! kind of tree, the suffix says what kind of container. This *is* an associative K→V container —
//! `get`, `insert(k, v)`, `values()`, `iter()` over pairs — so dropping the suffix would name a
//! map after a set. The same convention is already held elsewhere in this workspace
//! (`ReplicatedMap`). Only the container-kind suffix was added; the sourced stem is untouched.
//!
//! `FingerprintTreeMap` implements [`Rsos<K, V>`], delegating each trait method to the matching
//! inherent method, and *also* keeps its own pre-existing, Rust-idiomatic inherent API (`len`,
//! `insert`, `get_range`, `is_empty`, iterators, ...) side by side — the trait is one more way to
//! call it, not the only way.
//!
//! ## Why a *map* is the right correspondence to Def. 3.4/3.9 — not a deviation
//!
//! The paper writes the replica state as a set `X ⊆ U` and the lift as a function `U → M`. This
//! crate is generic over a key type *and* a value type, and its lift is
//! [`lift(&k, &v)`](lift) — `(K, V) → M`. That reads at first like a widening of the paper's
//! signature. It is not; it is the correct instantiation of it.
//!
//! **X is the set of keys.** Def. 3.9's operations are all key operations: `Rank`, `Select` and
//! `Enumerate` order and address elements by key, `Insert`/`Delete` are keyed, and the range
//! `[l, u)` an `Aggregate` covers is a range of keys. So `U = K` and `X` is the key set.
//!
//! The lift must then be a total function *of the key alone* — and it is, precisely because the
//! store is a map: a map assigns exactly one value per key, so `k ↦ lift(k, self[k])` is
//! well-defined for every `k ∈ X`. The map is what makes the lift total on `X`. `V` is not a
//! second dimension of the element space; it is the payload the lift consults, and
//! `insert(k, v)` is the paper's `Insert(x)` supplied with the data the lift needs to summarize
//! `x`. A set-only RSOS is the degenerate case `V = ()`.
//!
//! This also explains why two different values under the same key make the range fingerprints
//! differ while the element counts agree — the intended behavior for a *last-write-wins map*
//! being reconciled, and the reason `reconcile` can converge on values and not merely on key
//! membership.
//!
//! # Honest gaps (out of scope here, future work)
//!
//! - **No generic-monoid summary.** The bundled aggregate is hardwired to [`Fingerprint`]: a
//!   256-bit BLAKE3 accumulator combined by addition (an abelian group, so also a commutative
//!   monoid), not a caller-supplied arbitrary commutative monoid. `M` is fixed, in other words,
//!   even though [`lift`] into it already carries the paper's name. The Meyer/Willow-ecosystem
//!   reference implementation (`earthstar-project/range-reconcile`) names the right extension
//!   point for this future work `BYOLiftingMonoid` (`lift`/`combine`/`neutral`) — "lifting monoid"
//!   is the term to reach for if/when this crate grows a generic summary type; not implemented
//!   now. Note this is a gap in *`M`*, not in the element space: the `(K, V)` lift is the correct
//!   Def. 3.4 instantiation, as argued above.
//! - **No persistence.** [`FingerprintTreeMap`] is purely in-memory; a durable, content-addressed
//!   realization (what the 2026 paper's AELMDB is) is not attempted here.
//! - One design choice worth being explicit and positive about: [`Fingerprint`] is a *full*
//!   256-bit BLAKE3 accumulator, never truncated. This sits on the sound side of a tradeoff the
//!   2026 paper's §6.1 discusses explicitly: Negentropy's comparator truncates SHA-256 to 128
//!   bits, which the paper describes as only "probabilistically sound" rather than
//!   information-theoretically exact. `rsos` does not make that tradeoff.

pub mod aggregate;
pub mod fingerprint;
pub mod fingerprint_tree_map;
pub mod fingerprint_tree_map_iter;
mod rsos_trait;

pub use aggregate::Aggregate;
pub use fingerprint::{lift, Fingerprint};
pub use fingerprint_tree_map::FingerprintTreeMap;
pub use fingerprint_tree_map_iter::{IntoIter, IntoKeys, IntoValues, Iter, Keys, Values};
pub use rsos_trait::Rsos;

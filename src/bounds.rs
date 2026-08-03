// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Generic-bound bundles (see docs/ARCHITECTURE.md §3.3).
//!
//! The reconciliation machinery repeats the same multi-bound constraints on every key and value
//! type parameter. [`Key`] and [`Value`] bundle those *data* bounds (Clone/Debug/Hash/…/`'static`)
//! once, with blanket impls, so implementation sites can read `impl<K: Key, V: Value>` instead of
//! spelling the full list out each time.
//!
//! These bundles cover only the data bounds. *Entry semantics* (tombstone, timestamp, merge,
//! projection) are not bounds at all: they are inherent methods on
//! [`Entry`](crate::reconcilable::Entry) / [`State`](crate::reconcilable::State), so they travel
//! with the concrete domain type rather than with the `V` bound (see `docs/ARCHITECTURE.md` §3.3).
//!
//! The [`Hash`] bound carries a correctness requirement: per-element fingerprints hash the key and
//! value into one stream with no separator (see [`fingerprint::hash`](crate::fingerprint::hash)),
//! so a custom `Hash` impl on a key or value type **must be self-delimiting** or two distinct
//! elements can collide across the key/value boundary and the replicas silently fail to converge.
//! The standard library's impls (integers, `str`/`String`, slices/`Vec`) already satisfy this.

use std::fmt::Debug;
use std::hash::Hash;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Bundle of the data bounds required of a key type throughout the reconciliation machinery.
///
/// A blanket impl makes any type that satisfies the listed bounds a `Key` automatically, so this
/// never has to be implemented by hand. It does not add or remove any concrete bound; it is purely
/// a shorthand for the repeated `Clone + Debug + Hash + Ord + Send + Sync + Serialize +
/// DeserializeOwned + 'static` list.
pub trait Key:
    Clone + Debug + Hash + Ord + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

impl<T> Key for T where
    T: Clone + Debug + Hash + Ord + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

/// Bundle of the data bounds required of a value type throughout the reconciliation machinery.
///
/// A blanket impl makes any type that satisfies the listed bounds a `Value` automatically, so this
/// never has to be implemented by hand. It does not add or remove any concrete bound; it is purely
/// a shorthand for the repeated `Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned
/// + 'static` list. Unlike [`Key`] it needs no ordering: values are ordered by the stamp on their
/// [`Entry`](crate::reconcilable::Entry), never by their own content.
///
/// It also needs no `PartialEq`. Values are never compared: under last-write-wins the only
/// question the receive path asks is whether the remote stamp is greater, which `Timestamp: Ord`
/// answers (see `docs/ARCHITECTURE.md` §7 D5 for why that equivalence holds, and what would break it).
pub trait Value:
    Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

impl<T> Value for T where
    T: Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Generic-bound bundles (see ARCHITECTURE.md §3.8).
//!
//! The reconciliation machinery repeats the same multi-bound constraints on every key and value
//! type parameter. [`Key`] and [`Value`] bundle those *data* bounds (Clone/Debug/Hash/…/`'static`)
//! once, with blanket impls, so implementation sites can read `impl<K: Key, V: Value>` instead of
//! spelling the full list out each time.
//!
//! These bundles cover only the data bounds; *entry-semantics* bounds (such as
//! [`Projectable`](crate::reconcilable::Projectable)) are not bundled here and travel as extra
//! bounds alongside `V: Value` where required.
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
/// a shorthand for the repeated `Clone + Debug + Hash + PartialEq + Send + Sync + Serialize +
/// DeserializeOwned + 'static` list. Note it requires `PartialEq` (not `Ord`), unlike [`Key`].
pub trait Value:
    Clone + Debug + Hash + PartialEq + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

impl<T> Value for T where
    T: Clone + Debug + Hash + PartialEq + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

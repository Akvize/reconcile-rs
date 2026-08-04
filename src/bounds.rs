// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Generic-bound bundles, so sites read `impl<K: Key, V: Value>` (docs/ARCHITECTURE.md §3.3).
//!
//! Data bounds only. Entry semantics are inherent methods on
//! [`Entry`](crate::reconcilable::Entry) / [`State`](crate::reconcilable::State), not bounds.
//!
//! [`Hash`] carries a correctness requirement: [`fingerprint::hash`](crate::fingerprint::hash)
//! hashes key and value into one stream with no separator, so a custom impl **must be
//! self-delimiting** or two distinct elements collide across the boundary and replicas silently
//! stop converging. The std impls already are. See `docs/CONTRACT.md` §4.

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
/// Blanket-implemented, so it is never implemented by hand. No ordering: values are ordered by the
/// stamp on their [`Entry`](crate::reconcilable::Entry). No `PartialEq` either — values are never
/// compared, only stamps (docs/ARCHITECTURE.md §7 D5).
pub trait Value:
    Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

impl<T> Value for T where
    T: Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

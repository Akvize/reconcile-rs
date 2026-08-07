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
//! These bundles cover only the data bounds. Entry semantics (tombstone/live state, last-write-wins
//! merge, the timestamp-less projection) travel with [`Entry`](crate::entry::Entry) /
//! [`State`](crate::entry::State), not with the `V` bound: `V` is always the plain value type, and
//! the reconciliation machinery wraps it in `Entry<Timestamp, V>` (or projects it to `State<V>`)
//! internally.

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
/// [`Entry`](crate::entry::Entry), never by their own content.
///
/// It also needs no `PartialEq`. Values are never compared: under last-write-wins,
/// [`Entry::merge`](crate::entry::Entry::merge) returns the *other* entry exactly when the remote
/// stamp is strictly greater, so "would merging change state?" is answered by comparing stamps
/// (`Timestamp: Ord`) alone — no bound on `V`, and no clone of the value, needed on the receive
/// path. That equivalence holds only because conflict resolution is last-write-wins; a different
/// policy could need to inspect the value itself.
pub trait Value:
    Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

impl<T> Value for T where
    T: Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

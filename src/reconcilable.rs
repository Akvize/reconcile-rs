// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`Reconcilable`] trait.

use std::hash::Hash;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::hlc::Hlc;

/// Values stored in a map to be synced by the [`ReconcileStore`](crate::reconcile_store::ReconcileStore)
/// have to be [`Reconcilable`] to ensure safe conflict handling.
pub trait Reconcilable {
    fn reconcile(&self, other: &Self) -> Self;
}

/// Last-write-wins keyed on a [`Hlc`] timestamp.
///
/// Because [`Hlc`] is a **total order** `(wall_ms, counter, node_id)`, `reconcile` is `max`
/// over that order: it is commutative, associative and idempotent, so every replica
/// converges to the same value (Strong Eventual Consistency). Two *distinct* writes can
/// never share an `Hlc` (the same node bumps the counter, different nodes differ on
/// `node_id`), so the equal-timestamp branch only fires for identical values. See the
/// [`hlc`](crate::hlc) module for why the previous physical-clock scheme was unsafe.
impl<V: Clone> Reconcilable for (Hlc, V) {
    fn reconcile(&self, other: &Self) -> Self {
        if other.0 > self.0 {
            other.clone()
        } else {
            self.clone()
        }
    }
}

/// Marks values that may represent a deletion (a *tombstone*).
///
/// The reconciliation engine uses this to decide which applied updates require
/// causal-stability tracking before the corresponding key can be garbage-collected.
/// See the tombstone-resurrection discussion on
/// [`ReconcileStore`](crate::reconcile_store::ReconcileStore).
pub trait MaybeTombstone {
    /// Returns `true` if this value is a deletion marker (tombstone).
    fn is_tombstone(&self) -> bool;
}

impl<V> MaybeTombstone for (Hlc, Option<V>) {
    fn is_tombstone(&self) -> bool {
        self.1.is_none()
    }
}

/// A timestamp-less projection of a value, used by lightweight read-only mirrors
/// ([`LightReconcileStore`](crate::light::LightReconcileStore)) and by the value-only
/// *projection* tree a dated store maintains to answer them.
///
/// Its [`Hash`] is **value-only by construction**: it wraps just the [`Option<V>`] payload, with no
/// [`Hlc`] timestamp, so two replicas that agree on the logical value compute the *same* per-element
/// fingerprint regardless of when (or on which node) each last wrote it. A live value is
/// `ValueOnly(Some(v))`; a tombstone is `ValueOnly(None)`.
///
/// This is deliberately a *separate* type from the dated `(Hlc, Option<V>)`: the dated value keeps
/// its timestamp-inclusive `Hash` (required by the engine's `version_hash` for the #109
/// causal-stability acks), so a single value-only hash never replaces it. See issue #128.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ValueOnly<V>(pub Option<V>);

impl<V> ValueOnly<V> {
    /// Borrow the live value, or `None` if this is a tombstone.
    pub fn as_value(&self) -> Option<&V> {
        self.0.as_ref()
    }

    /// Returns `true` if this projection is a deletion marker (tombstone).
    pub fn is_tombstone(&self) -> bool {
        self.0.is_none()
    }
}

/// A dated value that can be projected to its timestamp-less [`ValueOnly`] form.
///
/// The dated store maintains a parallel value-only *projection* tree so it can reconcile with
/// dateless mirrors over the existing range-diff protocol without exposing timestamps. The
/// projection is kept in sync wherever the dated map mutates (insert, reconcile, GC removal).
///
/// The associated [`Projected`](Projectable::Projected) type carries exactly the bounds the
/// projection tree and the value-only wire channel need, so the reconciliation engine only has to
/// add a single `Projectable` bound. Note it deliberately does **not** require `PartialEq`: the
/// value-only path decides equality on fingerprints, never on the projected values themselves.
pub trait Projectable {
    /// The value-only projection. Its `Hash` must ignore any timestamp (see [`ValueOnly`]).
    type Projected: Clone + Hash + Send + Sync + Serialize + DeserializeOwned + 'static;
    /// Project this dated value to its timestamp-less form.
    fn project(&self) -> Self::Projected;
}

impl<V: Clone + Hash + Send + Sync + Serialize + DeserializeOwned + 'static> Projectable
    for (Hlc, Option<V>)
{
    type Projected = ValueOnly<V>;
    fn project(&self) -> ValueOnly<V> {
        ValueOnly(self.1.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_is_commutative_on_equal_wall_and_counter() {
        // Distinct node_ids with the same wall+counter: the old physical-clock tie-break
        // kept the local value, so the two replicas diverged forever (defect (b)). The
        // total order makes the survivor deterministic regardless of argument order.
        let a = (Hlc::new(100, 0, 1), "a");
        let b = (Hlc::new(100, 0, 2), "b");
        assert_eq!(a.reconcile(&b), b.reconcile(&a));
        // The higher node_id wins consistently.
        assert_eq!(a.reconcile(&b).1, "b");
    }

    #[test]
    fn reconcile_is_idempotent() {
        let a = (Hlc::new(7, 3, 42), "x");
        assert_eq!(a.reconcile(&a), a);
    }

    #[test]
    fn reconcile_picks_the_greater_timestamp() {
        let older = (Hlc::new(10, 0, 5), "old");
        let newer = (Hlc::new(11, 0, 1), "new");
        assert_eq!(older.reconcile(&newer).1, "new");
        assert_eq!(newer.reconcile(&older).1, "new");
    }
}

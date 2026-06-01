// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`Reconcilable`] trait.

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

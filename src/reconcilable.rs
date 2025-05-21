// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`Reconcilable`] trait.

use chrono::{DateTime, Utc};

/// Values stored in a map to be synced by the [`Service`](crate::Service)
/// have to be [`Reconcilable`] to ensure safe conflict handling.
pub trait Reconcilable {
    fn reconcile(&self, other: &Self) -> Self;
}

impl<V: Clone> Reconcilable for (DateTime<Utc>, V) {
    fn reconcile(&self, other: &Self) -> Self {
        if other.0 > self.0 {
            other.clone()
        } else {
            self.clone()
        }
    }
}

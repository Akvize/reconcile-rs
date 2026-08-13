// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Generic-bound bundles: `ARCHITECTURE.md` §4.1.
//!
//! Data bounds only, blanket-implemented. Entry semantics travel with
//! [`Entry`](crate::entry::Entry)/[`State`](crate::entry::State), not with `V`.

use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Bundle of the data bounds required of a key type. Blanket-implemented; never implemented by
/// hand.
pub trait Key: Clone + Debug + Ord + Send + Sync + Serialize + DeserializeOwned + 'static {}

impl<T> Key for T where T: Clone + Debug + Ord + Send + Sync + Serialize + DeserializeOwned + 'static
{}

/// Bundle of the data bounds required of a value type. Blanket-implemented; never implemented by
/// hand.
///
/// No `Ord` and no `PartialEq`: under last-write-wins the receive path decides everything by
/// comparing stamps, so a value is never ordered or compared.
pub trait Value: Clone + Debug + Send + Sync + Serialize + DeserializeOwned + 'static {}

impl<T> Value for T where T: Clone + Debug + Send + Sync + Serialize + DeserializeOwned + 'static {}

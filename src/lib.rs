// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This crate provides a reconciliation [`Service`] wrapping a redundant key-value store.
//! Different instances can talk together over UDP to efficiently
//! reconcile their differences.

//! All the data is available locally in all instances, and the user can be
//! notified of changes to the collection with an insertion hook.

//! The protocol allows finding a difference over millions of elements with a limited
//! number of round-trips. It should also work well to populate an instance from
//! scratch from other instances.

pub mod gen_ip;
pub(crate) mod internal_service;
pub mod map;
pub mod reconcilable;
pub mod service;
pub(crate) mod timeout_wheel;

pub use service::{DatedMaybeTombstone, Service};

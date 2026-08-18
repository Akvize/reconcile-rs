// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::ReplicatedMap;

use super::ephemeral_config;

/// `get_cloned` must not hold the read lock past its return, so a write immediately
/// following it (the `get`-then-`insert` pattern `get`'s own guard would self-deadlock on)
/// completes without hanging.
#[tokio::test]
async fn get_cloned_does_not_hold_the_lock_across_a_following_write() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();
    store.insert(1, 10);

    let value = store.get_cloned(&1);
    assert_eq!(value, Some(10));
    // If `get_cloned` still held the read lock here, this write lock acquisition would hang
    // forever instead of returning.
    store.insert(1, 20);

    assert_eq!(store.get_cloned(&1), Some(20));
    assert_eq!(store.get_cloned(&2), None);
}

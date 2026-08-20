// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `IterMut`: yields `(&K, &mut V)` over a [`FingerprintTreeMap`]. `#[cfg(test)]`-only (gated on
//! the whole module at its `mod iter_mut;` declaration, not item-by-item): it hands out `&mut V`
//! without updating the element fingerprint or the cached subtree aggregate --
//! [`FingerprintTreeMap::with_mut`] is the supported mutation path. `pub(super)`, not private:
//! `values.rs`'s `#[cfg(test)]`-only `ValuesMut` wraps it, and `tests.rs` calls `iter_mut()`
//! directly -- both are siblings under `fingerprint_tree_map_iter`, not descendants of this
//! module.

use serde::Serialize;

use crate::fingerprint_tree_map::{FingerprintTreeMap, Node};

/// A per-node frame of the [`IterMut`] traversal stack.
struct Frame<'a, K, V> {
    kv: std::iter::Zip<std::slice::Iter<'a, K>, std::slice::IterMut<'a, V>>,
    children: Option<std::slice::IterMut<'a, Box<Node<K, V>>>>,
}

/// Yields `(&K, &mut V)` in ascending key order.
///
/// Mutation through it leaves stale fingerprints; use [`FingerprintTreeMap::with_mut`].
pub(super) struct IterMut<'a, K, V> {
    stack: Vec<Frame<'a, K, V>>,
}

impl<'a, K, V> IterMut<'a, K, V> {
    /// Pushes `node` and the leftmost path beneath it, so the top frame yields in-order first.
    fn push_left_path(stack: &mut Vec<Frame<'a, K, V>>, mut node: &'a mut Node<K, V>) {
        loop {
            let Node {
                keys,
                values,
                children,
                ..
            } = node;
            let kv = keys.iter().zip(values.iter_mut());
            match children {
                Some(ch) => {
                    let mut child_iter = ch.iter_mut();
                    let first = child_iter.next().expect("internal node has >= 1 child");
                    stack.push(Frame {
                        kv,
                        children: Some(child_iter),
                    });
                    node = &mut **first; // descend into leftmost child
                }
                None => {
                    stack.push(Frame { kv, children: None });
                    return;
                }
            }
        }
    }
}

impl<'a, K: 'a + Serialize + Ord, V: Serialize> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            if let Some((k, v)) = frame.kv.next() {
                let next_child: Option<&'a mut Node<K, V>> = frame
                    .children
                    .as_mut()
                    .and_then(|c| c.next())
                    .map(|b| &mut **b);
                if let Some(child) = next_child {
                    Self::push_left_path(&mut self.stack, child);
                }
                return Some((k, v));
            }
            self.stack.pop();
        }
    }
}

impl<'a, K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// Yields `(&K, &mut V)` in ascending key order; leaves fingerprints stale.
    pub(super) fn iter_mut(&'a mut self) -> IterMut<'a, K, V> {
        let mut stack = Vec::new();
        IterMut::push_left_path(&mut stack, &mut self.root);
        IterMut { stack }
    }
}

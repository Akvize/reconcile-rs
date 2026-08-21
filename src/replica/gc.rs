// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::net::IpAddr;

use serde::Serialize;

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::Entry;

use super::Replica;

/// A deterministic, cross-node version token for a value: the low 64 bits of `rsos::digest`
/// (`ARCHITECTURE.md` §5 invariant 7).
///
/// A peer acknowledges the exact tombstone version it holds, so a stale ack cannot authorize GC of
/// a newer one.
pub(crate) fn version_hash<V: Serialize>(value: &V) -> u64 {
    rsos::digest(value).0[0]
}

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Remove a key from the dated `map`, its value-only projection, and the live-tombstone
    /// index (the GC removal path).
    pub(crate) fn gc_remove(&self, key: &K) -> Option<Entry<Timestamp, V>> {
        let mut guard = self.map.write();
        self.live_tombstones.write().remove(key);
        self.projection.write().remove(key);
        guard.remove(key)
    }

    /// Whether the tombstone for `key` at this version has been acknowledged by every member and
    /// is safe to collect. With no members known, GC is allowed.
    pub(crate) fn is_tombstone_stable(&self, key: &K, version: u64) -> bool {
        let members = self.members.read();
        if members.is_empty() {
            return true;
        }
        let acks = self.tombstone_acks.read();
        let Some(key_acks) = acks.get(key) else {
            return false;
        };
        members
            .iter()
            .all(|peer| key_acks.get(peer) == Some(&version))
    }

    /// Drop the acknowledgment bookkeeping for a key once its tombstone has been collected.
    pub(crate) fn forget_tombstone(&self, key: &K) {
        self.tombstone_acks.write().remove(key);
    }

    /// Whether `peer` still owes an acknowledgment on some held tombstone — i.e. whether its
    /// absence would block GC.
    ///
    /// Walks [`live_tombstones`](Self::live_tombstones), not
    /// [`tombstone_acks`](Self::tombstone_acks): a freshly deleted tombstone has no ack entry yet
    /// and must still count as pending.
    pub(crate) fn has_pending_tombstone_acks(&self, peer: IpAddr) -> bool {
        let live = self.live_tombstones.read();
        if live.is_empty() {
            return false;
        }
        let map = self.map.read();
        let acks = self.tombstone_acks.read();
        live.iter().any(|key| {
            let Some(entry) = map.get(key) else {
                return false;
            };
            let version = version_hash(entry);
            acks.get(key).and_then(|peer_acks| peer_acks.get(&peer)) != Some(&version)
        })
    }
}

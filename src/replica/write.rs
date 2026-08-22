// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::observability;
use crate::FingerprintTreeMap;

use super::{send_messages_to, Message, Replica, SendPorts, PEER_EXPIRATION};

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Insert into the dated `map` **and** mirror the value-only projection (and the
    /// live-tombstone index), under a consistent lock order (`map` -> `live_tombstones` ->
    /// `projection`) shared by every mutation path so the structures never deadlock against each
    /// other. The caller already holds the `map` write guard.
    pub(super) fn map_insert(
        &self,
        guard: &mut FingerprintTreeMap<K, Entry<Timestamp, V>>,
        key: K,
        value: Entry<Timestamp, V>,
    ) -> Option<Entry<Timestamp, V>> {
        // Keep the live-tombstone index in step with the map at its single mutation sink: a
        // tombstone value adds the key; any live value (a fresh insert, or an LWW overwrite that
        // resurrects a previously-deleted key) removes it. This index drives the per-round
        // causal-stability ack resend in `start_reconciliation`.
        {
            let mut live_tombstones = self.live_tombstones.write();
            if value.is_tombstone() {
                live_tombstones.insert(key.clone());
            } else {
                live_tombstones.remove(&key);
            }
        }
        self.projection.write().insert(key.clone(), value.project());
        guard.insert(key, value)
    }

    pub(super) fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    pub fn just_insert(&self, key: K, value: Entry<Timestamp, V>) -> Option<Entry<Timestamp, V>> {
        // Hooks run outside the write lock: a hook that re-inserts must not re-enter it and
        // deadlock (matching the update-merge path in `handle_messages`).
        (self.pre_insert.read())(&key, &value);

        // A tombstone value is a removal; a live value is an insertion. Counting here (rather
        // than in `ReplicatedMap`) keeps every local mutation path covered.
        if value.is_tombstone() {
            observability::record_remove();
        } else {
            observability::record_insert();
        }

        let mut guard = self.map.write();
        self.map_insert(&mut guard, key, value)
    }

    /// Broadcast a batch of messages to every known peer, on a detached task so the write path
    /// does not block on the network. The low-level send primitive both the immediate path and
    /// the coalescing flush ([`queue_broadcast`](Self::queue_broadcast)) reduce to.
    pub(super) fn broadcast(&self, messages: Vec<Message<K, Entry<Timestamp, V>, State<V>>>) {
        let peers = self.get_peers();
        let port = self.port;
        let transport = Arc::clone(&self.transport);
        let authenticator = self.authenticator.clone();
        let sender_counter = Arc::clone(&self.sender_counter);
        tokio::spawn(async move {
            let ports = SendPorts {
                transport: &*transport,
                authenticator: &authenticator,
                sender_counter: &sender_counter,
            };
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(&messages, &ports, &peer, &mut send_buf).await;
            }
        });
    }

    pub fn insert(&self, key: K, value: Entry<Timestamp, V>) -> Option<Entry<Timestamp, V>> {
        let ret = self.just_insert(key.clone(), value.clone());
        self.queue_broadcast(vec![(key, value)]);
        ret
    }

    /// Broadcast a single locally-mutated entry to peers, mirroring [`insert`](Self::insert)'s
    /// propagation. Used by in-place mutation paths (`ReplicatedMap::get_mut`) that write the map
    /// directly and must still notify peers so the edit reconciles, without re-applying it locally.
    pub(crate) fn broadcast_update(&self, key: K, value: Entry<Timestamp, V>) {
        self.queue_broadcast(vec![(key, value)]);
    }

    pub fn just_insert_bulk(&self, key_values: &[(K, Entry<Timestamp, V>)]) {
        // Hooks run outside the write lock, for the same re-entrancy reason as `just_insert`.
        for (key, value) in key_values {
            (self.pre_insert.read())(key, value);
            if value.is_tombstone() {
                observability::record_remove();
            } else {
                observability::record_insert();
            }
        }
        let mut guard = self.map.write();
        for (key, value) in key_values {
            self.map_insert(&mut guard, key.clone(), value.clone());
        }
    }

    pub fn insert_bulk(&self, key_values: &[(K, Entry<Timestamp, V>)]) {
        self.just_insert_bulk(key_values);
        self.queue_broadcast(key_values.to_vec());
    }
}

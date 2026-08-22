// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::net::SocketAddr;

use tracing::{debug, instrument, trace, warn};

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::observability;
use gossip::auth;

use super::{send_messages_to, version_hash, Message, Replica, MAX_MESSAGES_PER_DATAGRAM};

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Handle the messages in an already-authenticated, replay-checked [`Payload`] — taking
    /// [`auth::Payload<auth::Verified>`](auth::Payload) rather than bytes makes an unchecked
    /// datagram unrepresentable here.
    ///
    /// Returns whether the datagram carried at least one **dated** message, which is what
    /// qualifies the sender for membership: a value-only sender is a read replica and must not
    /// gate tombstone GC.
    #[instrument(name = "reconcile.handle", skip_all, fields(peer = %peer))]
    pub(super) async fn handle_messages(
        &self,
        payload: auth::Payload<'_, auth::Verified>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) -> bool {
        let timer = observability::timer();
        let payload = payload.as_bytes();
        trace!("received {} bytes from {peer}", payload.len());
        let mut in_comparison = Vec::new();
        let mut updates: Vec<(K, Entry<Timestamp, V>)> = Vec::new();
        let mut acks: Vec<(K, u64)> = Vec::new();
        let mut value_in_comparison = Vec::new();
        // Decode the whole datagram through `gossip::bincode`. `MAX_MESSAGES_PER_DATAGRAM` bounds the
        // message count (a datagram can hold no more one-byte messages than its byte length), so a
        // crafted datagram cannot be expanded without limit. A malformed datagram is dropped whole —
        // never panicking the receive loop, an unauthenticated remote-DoS hazard.
        let messages: Vec<Message<K, Entry<Timestamp, V>, State<V>>> =
            match gossip::bincode::decode_stream(payload, MAX_MESSAGES_PER_DATAGRAM) {
                Ok(messages) => messages,
                Err(kind) => {
                    warn!("failed to deserialize datagram from {peer}, dropping it: {kind:?}");
                    observability::record_datagram_dropped("malformed");
                    return false;
                }
            };
        for message in messages {
            match message {
                Message::ComparisonItem(segment) => in_comparison.push(segment),
                Message::Update(update) => updates.push(update),
                Message::Ack(ack) => acks.push(ack),
                Message::ValueComparisonItem(segment) => value_in_comparison.push(segment),
                // A dated store is authoritative and never integrates a value-only update; read replicas
                // are the only consumers of `ValueUpdate`. Ignore it defensively.
                Message::ValueUpdate(_) => {}
                // #463: reserved, never sent by this version. Ignored rather than matched with a
                // wildcard, so a future real variant added at a *new* tag cannot silently fall
                // through this arm unhandled — only these two already-reserved tags do.
                Message::Reserved5(_) | Message::Reserved6(_) => {}
            }
        }
        let spoke_dated = !in_comparison.is_empty() || !updates.is_empty() || !acks.is_empty();
        // record tombstone acknowledgments received from the peer
        if !acks.is_empty() {
            let peer_ip = peer.ip();
            let map_guard = self.map.read();
            let mut guard = self.tombstone_acks.write();
            for (key, version) in acks {
                // Only acks for locally-held tombstones, so `tombstone_acks` cannot grow
                // unbounded. An ack arriving before its deletion is dropped here and recovered by
                // the next round's ack resend.
                if map_guard.get(&key).is_some_and(|v| v.is_tombstone()) {
                    guard.entry(key).or_default().insert(peer_ip, version);
                } else {
                    trace!(
                        "dropped ack from {peer_ip} for key with no local tombstone; \
                         ignoring to prevent unbounded bookkeeping"
                    );
                }
            }
        }
        if !in_comparison.is_empty() {
            debug!("received {} segments", in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.map.read();
                rbsr::protocol_round(
                    &*guard,
                    in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // Refinement comparison items are small and latency-sensitive: send them inline, now.
            if !out_comparison.is_empty() {
                debug!("returning {} segments", out_comparison.len());
                trace!("segments: {out_comparison:?}");
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::ComparisonItem::<K, Entry<Timestamp, V>, State<V>>)
                    .collect();
                send_messages_to(&messages, &self.send_ports(), &peer, send_buf).await;
            }
            // The differing values are the bulk payload — a cold/empty peer pulls the whole dataset
            // here. Hand them to a rate-paced background task so the burst cannot overrun the
            // receiver and the receive loop stays free for other peers.
            if !differences.is_empty() {
                debug!("returning {} diff_ranges", differences.len());
                trace!("diff_ranges: {differences:?}");
                // Claim both slots (per-peer + global budget) *before* snapshotting the range
                // into a Vec. A skipped dump allocates nothing; the peer re-initiates on its
                // next diff round once a slot is free.
                if let Some((peer_guard, global_guard)) = self.try_claim_dump_slot(peer) {
                    let updates: Vec<Message<K, Entry<Timestamp, V>, State<V>>> = {
                        let guard = self.map.read();
                        let mut updates = Vec::new();
                        for range in differences {
                            for (k, v) in guard.range(range) {
                                updates.push(Message::Update((k.clone(), v.clone())));
                            }
                        }
                        updates
                    };
                    if !updates.is_empty() {
                        self.spawn_paced_send(updates, peer, peer_guard, global_guard);
                    }
                    // If updates is empty the guards drop here, releasing both slots.
                }
            }
        }
        if !updates.is_empty() {
            debug!("received {} updates", updates.len());
            observability::record_updates_received(updates.len());
            // Tombstones we now hold as a result of these updates, to be acknowledged back to
            // the peer so it can eventually garbage-collect them once causally stable.
            let mut acks_to_send = Vec::new();
            // 1) Under a read lock, decide which merged values would actually change state. We must
            //    NOT run the pre-insert hook here: hooks are contractually executed *outside* the
            //    map's write lock (matching `just_insert`), so a hook that re-inserts cannot
            //    re-enter the lock and deadlock.
            let mut to_apply: Vec<(K, Entry<Timestamp, V>)> = Vec::new();
            {
                let guard = self.map.read();
                for (k, remote_v) in updates {
                    // Advance our clock past the timestamp carried by the remote value, so a
                    // later local write is ordered after everything we have seen. This is
                    // what prevents lost updates under clock skew.
                    self.clock.observe(remote_v.stamp);
                    match guard.get(&k) {
                        Some(local_v) => {
                            // Under LWW the stamp comparison alone answers "would merging change
                            // state?", so the value is never cloned or compared here.
                            if remote_v.stamp > local_v.stamp {
                                to_apply.push((k, remote_v));
                            } else if local_v.is_tombstone() {
                                // We already hold an equal-or-newer value; still acknowledge it
                                // if it is the same tombstone, so the peer learns we have it.
                                acks_to_send.push(
                                    Message::Ack::<K, Entry<Timestamp, V>, State<V>>((
                                        k,
                                        version_hash(local_v),
                                    )),
                                );
                            }
                        }
                        None => to_apply.push((k, remote_v)),
                    }
                }
            }
            // 2) Run the pre-insert hooks with no lock held, exactly as `just_insert` does.
            for (k, v) in &to_apply {
                (self.pre_insert.read())(k, v);
            }
            // 3) Re-acquire and re-reconcile: the lock was released, so a concurrent write may
            //    have landed. `reconcile` is idempotent `max`, so re-applying is safe either way.
            if !to_apply.is_empty() {
                let mut guard = self.map.write();
                for (k, v) in to_apply {
                    let merged_v = match guard.get(&k) {
                        Some(local_v) => local_v.merge(&v),
                        None => v,
                    };
                    let version = merged_v.is_tombstone().then(|| version_hash(&merged_v));
                    self.map_insert(&mut guard, k.clone(), merged_v);
                    if let Some(version) = version {
                        acks_to_send.push(Message::Ack::<K, Entry<Timestamp, V>, State<V>>((
                            k, version,
                        )));
                    }
                }
            }
            if !acks_to_send.is_empty() {
                send_messages_to(&acks_to_send, &self.send_ports(), &peer, send_buf).await;
            }
        }
        // Value-only channel: answer a dateless read replica by diffing against the value-only
        // *projection* tree (never the dated map) and replying with `ValueUpdate`s carrying only
        // the projected payload. This path is entirely independent of the dated channel and of the
        // causal-stability state — no acks, no membership, no GC interaction.
        if !value_in_comparison.is_empty() {
            debug!("received {} value-only segments", value_in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.projection.read();
                rbsr::protocol_round(
                    &*guard,
                    value_in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // Refinement comparison items are small and latency-sensitive: send them inline, now.
            if !out_comparison.is_empty() {
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::ValueComparisonItem::<K, Entry<Timestamp, V>, State<V>>)
                    .collect();
                send_messages_to(&messages, &self.send_ports(), &peer, send_buf).await;
            }
            // Bulk value-only payload — a dateless read replica pulling the dataset. Rate-pace it on a
            // background task, exactly like the dated bulk path.
            if !differences.is_empty() {
                if let Some((peer_guard, global_guard)) = self.try_claim_dump_slot(peer) {
                    let updates: Vec<Message<K, Entry<Timestamp, V>, State<V>>> = {
                        let guard = self.projection.read();
                        let mut updates = Vec::new();
                        for range in differences {
                            for (k, p) in guard.range(range) {
                                updates.push(Message::ValueUpdate((k.clone(), p.clone())));
                            }
                        }
                        updates
                    };
                    if !updates.is_empty() {
                        self.spawn_paced_send(updates, peer, peer_guard, global_guard);
                    }
                }
            }
        }
        observability::record_handle_duration(timer);
        spoke_dated
    }
}

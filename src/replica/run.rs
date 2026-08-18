// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::Instant;

use ipnet::IpNet;
use rand::seq::SliceRandom;
use tokio::time::timeout;
use tracing::{debug, instrument, trace, warn};

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::observability;
use gossip::auth;

use super::{
    send_messages_to, send_to_retry, version_hash, Message, Replica, BUFFER_SIZE,
    MAX_MESSAGES_PER_DATAGRAM, TOMBSTONE_ACK_RESEND_BYTE_BUDGET,
};

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Drive the gossip and reconciliation loops forever.
    ///
    /// This method does not return and cannot fail: network send errors are logged and
    /// counted, never fatal, so a vanished or unreachable peer cannot stop the loops.
    #[instrument(name = "reconcile.run", skip_all, fields(port = self.port))]
    pub async fn run(self) {
        // One byte larger than the largest legal datagram, so a message that fills it exactly
        // is distinguishable from one that was truncated.
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        self.start_reconciliation(&mut send_buf).await;
        loop {
            // Re-read each iteration so the cadence can be retuned at runtime.
            let recv_timeout = *self.reconcile_interval.read();
            match timeout(recv_timeout, self.transport.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    debug!("no recent activity; initiating diff protocol");
                    self.start_reconciliation(&mut send_buf).await;
                }
                Ok(Err(err)) => {
                    warn!("network error in recv_from: {err}");
                    observability::record_datagram_dropped("recv_error");
                }
                Ok(Ok((size, peer))) => {
                    observability::record_bytes_received(size);
                    if peer.port() != self.port {
                        warn!(
                            "received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    if size == recv_buf.len() {
                        warn!("Buffer too small for message, discarded");
                        observability::record_datagram_dropped("too_large");
                    } else {
                        // Authenticate the datagram *before* any deserialization. Only a cleared
                        // `Payload` can reach `handle_messages`; a missing or invalid tag is
                        // dropped silently (trace-only, to avoid attacker-driven log flooding).
                        match self.authenticator.open(&recv_buf[..size]) {
                            Some(payload) => {
                                // Reject a differently-versioned peer with a distinguishable,
                                // counted reason — never confused with "malformed" or "bad_mac".
                                // Runs on already-authenticated bytes (a forged version claim is
                                // rejected the same way a forged payload is), but ahead of every
                                // other per-sender bookkeeping below.
                                let payload = match payload.check_version() {
                                    Ok(payload) => payload,
                                    Err(version) => {
                                        trace!(
                                            "dropped datagram from {peer}: wire version {version} \
                                             != {}",
                                            gossip::auth::WIRE_VERSION
                                        );
                                        observability::record_datagram_dropped("version");
                                        continue;
                                    }
                                };
                                let sender = peer.ip();
                                // If this sender is new and membership is at capacity, drop before
                                // allocating any per-sender state (replay filter, peers map,
                                // membership). Placed ahead of the replay filter so a capped-out
                                // sender never gets an entry there either. Known senders bypass it.
                                let (is_known, current_len) = {
                                    let guard = self.members.read();
                                    (guard.contains(&sender), guard.len())
                                };
                                if !self.max_peers.admits(is_known, current_len) {
                                    trace!(
                                        "dropped datagram from {peer}: peer cap reached \
                                         ({current_len}/{})",
                                        self.max_peers.max()
                                    );
                                    observability::record_datagram_dropped("peer_cap");
                                    continue;
                                }
                                // A no-op in unauthenticated mode: the filter was built disabled.
                                let (seq, stamp) = (payload.seq, payload.stamp);
                                let Some(payload) =
                                    payload.verify_replay(&self.replay_filter, sender)
                                else {
                                    trace!(
                                        "dropped replayed or stale datagram from {peer}: \
                                         seq={seq} stamp={stamp}"
                                    );
                                    observability::record_datagram_dropped("replay");
                                    continue;
                                };
                                let spoke_dated =
                                    self.handle_messages(payload, peer, &mut send_buf).await;
                                // Only accepted datagrams register a sender, so a spoofed host
                                // cannot become a member and block GC forever. A sender that spoke
                                // only the value-only channel is a read replica: it never acks
                                // tombstones, so it must never join `members` either.
                                if spoke_dated {
                                    self.peers.write().insert(sender, Instant::now());
                                    self.members.write().insert(sender);
                                }
                            }
                            None => {
                                trace!("dropped datagram from {peer}: missing or invalid MAC");
                                observability::record_datagram_dropped("bad_mac");
                            }
                        }
                    }
                }
            }
        }
    }

    #[instrument(name = "reconcile.round", skip_all)]
    pub async fn start_reconciliation(&self, send_buf: &mut Vec<u8>) {
        let timer = observability::timer();
        observability::record_reconcile_round();
        let segments = {
            let guard = self.map.read();
            rbsr::initial_ranges(&*guard)
        };
        send_buf.clear();
        for segment in segments {
            gossip::bincode::encode(
                &Message::ComparisonItem::<K, Entry<Timestamp, V>, State<V>>(segment),
                send_buf,
            )
            .expect("serializing a ComparisonItem into an in-memory buffer cannot fail");
        }
        // Snapshot the runtime-tunable topology once per round: no torn round, no lock held
        // across the sends below.
        let nets = self.nets.read().clone();
        let local = *self.local_net.read();
        let remote_interval = self.remote_interval.load(Ordering::Relaxed).max(1);
        let remote_fanout = self.remote_fanout.load(Ordering::Relaxed);
        let round = self.round.fetch_add(1, Ordering::Relaxed);
        // Treat an interval of 0 as "every round" to avoid a modulo-by-zero.
        let do_remote = round.is_multiple_of(remote_interval);
        let known = self.get_peers();

        // De-duplicate so a discovery probe that happens to hit a known peer is not sent twice.
        let mut targets: HashSet<IpAddr> = HashSet::new();

        // Speculative probes only: an address that answers is registered then, not now. An
        // authoritative source drives the store's seed/decommission loop instead.
        targets.extend(self.probe.discover().await.unwrap_or_default());

        // Local network: contact every known peer, every round (fast intra-network convergence).
        for &addr in &known {
            if local.contains(&addr) {
                targets.insert(addr);
            }
        }

        // Remote peers on cross-network rounds only, a bounded subset per bucket, plus an
        // `unclassified` bucket: repair is decoupled from net membership, so a topology change
        // can never orphan a contacted peer from repair.
        if do_remote {
            let remote_nets: Vec<IpNet> = nets.iter().copied().filter(|&n| n != local).collect();
            let mut buckets: HashMap<Option<usize>, Vec<IpAddr>> = HashMap::new();
            for &addr in &known {
                if local.contains(&addr) {
                    continue; // already contacted every round above
                }
                let bucket = remote_nets.iter().position(|n| n.contains(&addr));
                buckets.entry(bucket).or_default().push(addr);
            }
            let mut rng = self.rng.write();
            for (_, mut peers) in buckets {
                peers.shuffle(&mut *rng);
                targets.extend(peers.into_iter().take(remote_fanout));
            }
        }

        // Piggyback causal-stability ack resends for the tombstones we hold.
        self.resend_held_tombstone_acks(send_buf, round);

        // initiate the reconciliation protocol with the selected peers and discovery probes
        for peer in targets {
            trace!("initial_ranges {} bytes to {peer}", send_buf.len());
            if let Err(err) = send_to_retry(
                &*self.transport,
                &self.authenticator,
                &self.sender_counter,
                send_buf,
                SocketAddr::new(peer, self.port),
            )
            .await
            {
                warn!("failed to send reconciliation initiation to {peer}: {err}; continuing");
            }
        }
        observability::record_round_duration(timer);
    }

    /// Append an ack for each held tombstone to `send_buf`, returning the count.
    ///
    /// Acks are otherwise pairwise, so past two nodes
    /// [`is_tombstone_stable`](Self::is_tombstone_stable) never completes; resending every round
    /// makes the matrix converge transitively, and makes an ack that arrived before its tombstone
    /// (dropped by the admission gate) recoverable on a later round.
    ///
    /// Bounded to [`TOMBSTONE_ACK_RESEND_BYTE_BUDGET`] bytes per datagram, over a window whose
    /// start advances with `round` across sorted keys, so every tombstone is covered within a
    /// bounded number of rounds.
    fn resend_held_tombstone_acks(&self, send_buf: &mut Vec<u8>, round: u32) -> usize {
        let mut keys: Vec<K> = self.live_tombstones.read().iter().cloned().collect();
        if keys.is_empty() {
            return 0;
        }
        keys.sort_unstable();
        let n = keys.len();
        let budget = send_buf.len() + TOMBSTONE_ACK_RESEND_BYTE_BUDGET;
        let start = (round as usize) % n;
        let map_guard = self.map.read();
        let mut appended = 0;
        let mut budget_truncated = false;
        for offset in 0..n {
            if send_buf.len() >= budget {
                budget_truncated = true;
                break;
            }
            let key = &keys[(start + offset) % n];
            // Re-confirm against the map: the tombstone may have been resurrected or GC'd since
            // we snapshotted the index, and only the live tombstone's version is a valid ack.
            if let Some(v) = map_guard.get(key).filter(|v| v.is_tombstone()) {
                gossip::bincode::encode(
                    &Message::Ack::<K, Entry<Timestamp, V>, State<V>>((
                        key.clone(),
                        version_hash(v),
                    )),
                    send_buf,
                )
                .expect("serializing an Ack into an in-memory buffer cannot fail");
                appended += 1;
            }
        }
        if budget_truncated {
            trace!(
                "resent {appended}/{n} held-tombstone acks this round (datagram byte budget \
                 reached); the remainder rotates in on subsequent rounds"
            );
        }
        observability::record_tombstone_acks_resent(appended);
        appended
    }

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

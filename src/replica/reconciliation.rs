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

use ipnet::IpNet;
use rand::seq::SliceRandom;
use tracing::{instrument, trace, warn};

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::observability;

use super::{send_to_retry, version_hash, Message, Replica, TOMBSTONE_ACK_RESEND_BYTE_BUDGET};

impl<K: Key + Hash, V: Value> Replica<K, V> {
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
        let do_remote = round % remote_interval == 0;
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
    pub(super) fn resend_held_tombstone_acks(&self, send_buf: &mut Vec<u8>, round: u32) -> usize {
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
}

// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::replica::{
    send_messages_to, send_to_retry, Message, SendPorts, MAX_MESSAGES_PER_DATAGRAM,
};
use crate::transport::Transport;
use gossip::auth;
use gossip::gen_ip::gen_ip;

use super::ReadReplicaMap;

const BUFFER_SIZE: usize = 65507;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);

/// The wire value type, named only so the shared [`Message`] enum has a concrete `Update` payload
/// — which a read replica ignores, storing no dated value.
type WireDated<V> = Entry<Timestamp, V>;

impl<K: Key, V: Value> ReadReplicaMap<K, V> {
    /// Set the hook invoked outside the map lock, before each inbound value is integrated. A
    /// deletion arrives as `State::Tombstone`. This is a setter: a second call replaces the
    /// first, it does not add to it.
    pub fn set_on_update<F: Send + Sync + Fn(&K, &State<V>) + 'static>(&self, on_update: F) {
        *self.on_update.write() = Box::new(on_update);
    }

    /// Integrate inbound value-only updates by plain overwrite (a read replica holds no timestamp to
    /// compare against — it trusts the authoritative dated peer). Hooks run outside the map lock,
    /// so a hook may safely call back into the read replica.
    pub(super) fn integrate(&self, updates: Vec<(K, State<V>)>) {
        if updates.is_empty() {
            return;
        }
        {
            let hook = self.on_update.read();
            for (k, state) in &updates {
                hook(k, state);
            }
        }
        let mut guard = self.tree.write();
        for (k, state) in updates {
            guard.insert(k, state);
        }
    }

    /// Bundle the outbound ports the batched-send helpers need, exactly as
    /// [`Replica`](crate::Replica) does. See [`SendPorts`].
    fn send_ports(&self) -> SendPorts<'_, dyn Transport> {
        SendPorts {
            transport: &*self.transport,
            authenticator: &self.authenticator,
            sender_counter: &self.sender_counter,
        }
    }

    /// Run one round of value-only anti-entropy against the configured peers. Normally driven by
    /// [`run`](Self::run)'s loop; exposed for callers that want to force an out-of-band round
    /// (e.g. in tests), mirroring [`ReplicatedMap::start_reconciliation`](crate::ReplicatedMap::start_reconciliation).
    pub async fn start_reconciliation(&self) {
        let mut send_buf = Vec::new();
        self.start_reconciliation_inner(&mut send_buf).await;
    }

    /// Send our value-only comparison items to every known peer plus a random address (discovery),
    /// kicking off / continuing a value-only reconciliation round. `send_buf` is caller-owned so
    /// [`run`](Self::run)'s hot loop can reuse one allocation across rounds.
    async fn start_reconciliation_inner(&self, send_buf: &mut Vec<u8>) {
        let segments = rbsr::initial_ranges(&*self.tree.read());
        send_buf.clear();
        for segment in segments {
            gossip::bincode::encode(
                &Message::ValueComparisonItem::<K, WireDated<V>, State<V>>(segment),
                send_buf,
            )
            .unwrap();
        }
        let mut peers = self.get_peers();
        // A random address out of the peer network, for discovery — like the dated store, we do not
        // add it to the known peers; a real peer there will answer and be recorded then.
        let net = *self.net.read();
        let addr = gen_ip(&mut *self.rng.write(), net);
        peers.push(addr);
        for peer in peers {
            trace!(
                "read replica initial_ranges {} bytes to {peer}",
                send_buf.len()
            );
            if let Err(err) = send_to_retry(
                &*self.transport,
                &self.authenticator,
                &self.sender_counter,
                send_buf,
                SocketAddr::new(peer, self.port),
            )
            .await
            {
                warn!(
                    "read replica failed to send reconciliation initiation to {peer}: {err}; \
                     continuing"
                );
            }
        }
    }

    async fn handle_messages(
        &self,
        payload: auth::Payload<'_, auth::Verified>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) {
        let payload = payload.as_bytes();
        trace!("read replica received {} bytes from {peer}", payload.len());
        let mut value_in_comparison = Vec::new();
        let mut value_updates: Vec<(K, State<V>)> = Vec::new();
        // `MAX_MESSAGES_PER_DATAGRAM` bounds the expansion; a malformed datagram is dropped whole.
        let messages: Vec<Message<K, WireDated<V>, State<V>>> =
            match gossip::bincode::decode_stream(payload, MAX_MESSAGES_PER_DATAGRAM) {
                Ok(messages) => messages,
                Err(kind) => {
                    warn!(
                        "read replica failed to deserialize datagram from {peer}, dropping it: \
                         {kind:?}"
                    );
                    return;
                }
            };
        for message in messages {
            match message {
                Message::ValueComparisonItem(segment) => value_in_comparison.push(segment),
                Message::ValueUpdate(update) => value_updates.push(update),
                // The dated channel is meaningless to a read replica (it cannot store dated values
                // nor participate in causal stability). Ignore it.
                Message::ComparisonItem(_) | Message::Update(_) | Message::Ack(_) => {}
                // #463: reserved, never sent by this version.
                Message::Reserved5(_) | Message::Reserved6(_) => {}
            }
        }

        self.integrate(value_updates);

        if !value_in_comparison.is_empty() {
            debug!(
                "read replica received {} value-only segments",
                value_in_comparison.len()
            );
            let mut out_comparison = Vec::new();
            let mut differences = Vec::new();
            {
                let guard = self.tree.read();
                rbsr::protocol_round(
                    &*guard,
                    value_in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // `differences` are ranges this read replica would owe the peer. A read-only replica
            // never sends authoritative values, so we deliberately drop them and only bounce back
            // the refined comparison items that keep the peer's side of the diff progressing.
            if !out_comparison.is_empty() {
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::<K, WireDated<V>, State<V>>::ValueComparisonItem)
                    .collect();
                send_messages_to(&messages, &self.send_ports(), &peer, send_buf).await;
            }
        }
    }

    /// Run the read replica's reconciliation loop forever. Spawn this on a task; the read replica
    /// converges to the dated cluster's current values and reflects deletions as tombstones.
    pub async fn run(self) {
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        self.start_reconciliation_inner(&mut send_buf).await;
        loop {
            match timeout(ACTIVITY_TIMEOUT, self.transport.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    debug!("read replica: no recent activity; initiating value-only diff");
                    self.start_reconciliation_inner(&mut send_buf).await;
                }
                Ok(Err(err)) => warn!("read replica network error in recv_from: {err}"),
                Ok(Ok((size, peer))) => {
                    if peer.port() != self.port {
                        warn!(
                            "read replica received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    if size == recv_buf.len() {
                        warn!("read replica buffer too small for message, discarded");
                    } else {
                        match self.authenticator.open(&recv_buf[..size]) {
                            Some(payload) => {
                                // Reject a differently-versioned peer distinguishably from an
                                // authentication failure — see `Replica::run`'s identical gate.
                                let payload = match payload.check_version() {
                                    Ok(payload) => payload,
                                    Err(version) => {
                                        trace!(
                                            "read replica dropped datagram from {peer}: wire \
                                             version {version} != {}",
                                            auth::WIRE_VERSION
                                        );
                                        continue;
                                    }
                                };
                                let sender = peer.ip();
                                // Per-peer cap check: drop datagrams from unknown senders when the
                                // peers map is at capacity, before any per-sender state is
                                // allocated (peers slot or replay-filter entry).
                                {
                                    let guard = self.peers.read();
                                    let (known, current_len) =
                                        (guard.contains_key(&sender), guard.len());
                                    if !self.max_peers.admits(known, current_len) {
                                        trace!(
                                            "read replica dropped datagram from {peer}: peer cap \
                                             reached ({current_len}/{})",
                                            self.max_peers.max()
                                        );
                                        continue;
                                    }
                                }
                                let (seq, stamp) = (payload.seq, payload.stamp);
                                let Some(payload) =
                                    payload.verify_replay(&self.replay_filter, sender)
                                else {
                                    trace!(
                                        "read replica dropped replayed datagram from {peer}: \
                                         seq={seq} stamp={stamp}"
                                    );
                                    continue;
                                };
                                self.handle_messages(payload, peer, &mut send_buf).await;
                                // Record the sender so we keep gossiping value-only diffs to it.
                                self.peers.write().insert(sender, Instant::now());
                            }
                            None => trace!(
                                "read replica dropped datagram from {peer}: missing or invalid MAC"
                            ),
                        }
                    }
                }
            }
        }
    }
}

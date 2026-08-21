// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::time::Instant;

use tokio::time::timeout;
use tracing::{debug, instrument, trace, warn};

use crate::bounds::{Key, Value};
use crate::observability;

use super::{PeerCap, Replica, BUFFER_SIZE};

impl PeerCap {
    pub(crate) fn new(max_peers: usize) -> Self {
        PeerCap(max_peers)
    }

    /// Whether a datagram from a sender should be admitted, given whether the sender is already
    /// tracked and how many distinct peers are currently tracked.
    pub(crate) fn admits(self, known: bool, current_len: usize) -> bool {
        known || current_len < self.0
    }

    pub(crate) fn max(self) -> usize {
        self.0
    }
}

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
}

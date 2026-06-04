// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReconcileMirror`], a lightweight, **dateless, read-only mirror** of a dated
//! [`ReconcileStore`](crate::ReconcileStore).
//!
//! # What it is for
//!
//! A dated store keeps a [`Hlc`] timestamp next to every value so it can resolve conflicts
//! (last-write-wins) and run the #109 tombstone causal-stability machinery. For a fleet with many
//! *passive read replicas* that only ever consume values, that timestamp is pure overhead: ~12–16
//! bytes per entry that the replica never needs. A `ReconcileMirror` stores only
//! [`ValueOnly<V>`] — the `Option<V>` payload, no timestamp — and still converges with a dated peer
//! over the **existing range-based diff protocol**, on the same UDP port.
//!
//! # How it stays #109-safe
//!
//! The mirror speaks only the **value-only channel** of the protocol (the `ValueComparisonItem` /
//! `ValueUpdate` messages, see issue #128). A
//! dated peer answers those by diffing against its value-only *projection* tree, so the mirror never
//! sees a timestamp and the dated↔dated path is untouched. Crucially, the mirror **never
//! acknowledges tombstones and is never added to a dated peer's #109 membership set**, so it cannot
//! block tombstone garbage collection — the regression the naive "single value-only hash" design
//! would have caused.
//!
//! # Read-only
//!
//! The mirror **always integrates** inbound updates (plain overwrite — it holds no timestamp to
//! compare) and **never sends authoritative values**: it drives the diff by exchanging comparison
//! items but ignores any difference it is asked to push back. It is a sink, not a source.
//!
//! # Limitations
//!
//! - A mirror reflects a deletion as a tombstone only if it observes the tombstone before the dated
//!   store **garbage-collects** it. With the default tombstone timeout (60 s) a mirror polling on
//!   the order of a second has ample time; but if GC is configured to outrun mirror propagation,
//!   the mirror may retain the pre-deletion value. Likewise, once a dated store GC's a tombstone the
//!   mirror keeps its own value-only entry for that key (it never pushes data back), so `get` may
//!   still return the stale value. Both are consequences of the mirror being a passive, read-only
//!   replica with no causal-stability bookkeeping of its own.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use ipnet::IpNet;
use parking_lot::{MappedRwLockReadGuard, RwLock, RwLockReadGuard};
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::auth;
use crate::bounds::Key;
use crate::diff::{Diffable, HashRangeQueryable};
use crate::fingerprint::Fingerprint;
use crate::gen_ip::gen_ip;
use crate::hlc::Hlc;
use crate::reconcilable::ValueOnly;
use crate::reconcile_engine::{send_messages_to, send_to_retry, Message};
use crate::reconcile_store::Config;
use crate::HRTree;

const BUFFER_SIZE: usize = 65507;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);
const PEER_EXPIRATION: Duration = Duration::from_secs(60);

type OnUpdateCallback<K, V> = Box<dyn Send + Sync + Fn(&K, &ValueOnly<V>)>;

/// The wire value type a mirror names for (de)serialization. The mirror never stores a dated value;
/// it only needs the type so the shared [`Message`] enum has a concrete `Update` payload (which it
/// ignores) and so the value-only [`Projected`](crate::reconcilable::Projectable::Projected) type
/// resolves to [`ValueOnly<V>`].
type WireDated<V> = (Hlc, Option<V>);

/// A lightweight, dateless, read-only mirror of a dated [`ReconcileStore`](crate::ReconcileStore).
///
/// See the [module documentation](crate::mirror) for the design and the #109-safety guarantees.
pub struct ReconcileMirror<K, V> {
    /// The value-only mirror. Its range fingerprints are timestamp-less by construction (see
    /// [`ValueOnly`]), matching a dated peer's value-only projection.
    tree: Arc<RwLock<HRTree<K, ValueOnly<V>>>>,
    port: u16,
    socket: Arc<UdpSocket>,
    peer_net: IpNet,
    rng: Arc<RwLock<StdRng>>,
    peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
    authenticator: auth::Authenticator,
    /// Invoked just before each inbound value is integrated, so callers can be notified of changes.
    on_update: Arc<RwLock<OnUpdateCallback<K, V>>>,
}

impl<K, V> Clone for ReconcileMirror<K, V> {
    fn clone(&self) -> Self {
        ReconcileMirror {
            tree: self.tree.clone(),
            port: self.port,
            socket: self.socket.clone(),
            peer_net: self.peer_net,
            rng: self.rng.clone(),
            peers: self.peers.clone(),
            authenticator: self.authenticator.clone(),
            on_update: self.on_update.clone(),
        }
    }
}

// NOTE: `V` here is NOT the `Value` bundle: a mirror only ever handles the value-only projection,
// so it does not require `V: PartialEq` (which `Value` includes). The data bounds are spelled out
// to keep the required bound set identical (no tightening). `K` matches `Key` exactly.
impl<K: Key, V: Clone + Debug + DeserializeOwned + Hash + Send + Serialize + Sync + 'static>
    ReconcileMirror<K, V>
{
    /// Create a new mirror bound to the configured UDP socket.
    ///
    /// The mirror honours the same [`Config`] as a dated store, including
    /// [`with_cluster_key`](Config::with_cluster_key): to mirror an authenticated cluster it must
    /// share the cluster key. The Hybrid-Logical-Clock `node_id` is ignored (a mirror mints no
    /// timestamps).
    pub async fn new(config: Config) -> Self {
        let socket = UdpSocket::bind(SocketAddr::new(config.listen_addr, config.port))
            .await
            .unwrap();
        debug!(
            "ReconcileMirror listening on: {}",
            socket.local_addr().unwrap()
        );
        let authenticator = auth::Authenticator::new(config.cluster_key, config.encrypt);
        if !authenticator.is_enabled() {
            warn!(
                "SECURITY: no cluster key set — the lightweight mirror accepts UNAUTHENTICATED \
                 datagrams. Set Config::with_cluster_key to match the dated cluster."
            );
        }
        ReconcileMirror {
            tree: Arc::new(RwLock::new(HRTree::<K, ValueOnly<V>>::new())),
            port: config.port,
            socket: Arc::new(socket),
            peer_net: config.peer_net,
            rng: Arc::new(RwLock::new(StdRng::from_entropy())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            authenticator,
            on_update: Arc::new(RwLock::new(Box::new(|_, _| {}))),
        }
    }

    /// Provide the address of a known dated peer, reducing the time to first sync.
    pub fn with_seed(self, peer: IpAddr) -> Self {
        self.peers.write().insert(peer, Instant::now());
        self
    }

    /// Register a hook invoked (outside the map lock) just before each inbound value is integrated.
    ///
    /// Useful to be notified of changes mirrored from the dated cluster. The tombstone case is
    /// `ValueOnly(None)`.
    pub fn add_on_update<F: Send + Sync + Fn(&K, &ValueOnly<V>) + 'static>(&self, on_update: F) {
        *self.on_update.write() = Box::new(on_update);
    }

    /// Get the live value for a key, or `None` if the key is absent or holds a mirrored tombstone.
    pub fn get(&self, k: &K) -> Option<MappedRwLockReadGuard<'_, V>> {
        let guard = self.tree.read();
        RwLockReadGuard::try_map(guard, |tree| tree.get(k).and_then(|vo| vo.as_value())).ok()
    }

    /// Whether the mirror currently holds a live value for the key (a tombstone counts as absent).
    pub fn contains_key(&self, k: &K) -> bool {
        self.tree.read().get(k).is_some_and(|vo| !vo.is_tombstone())
    }

    /// Number of entries currently stored, **including mirrored tombstones**.
    pub fn len(&self) -> usize {
        self.tree.read().len()
    }

    /// Whether the mirror is empty (no entries at all, tombstones included).
    pub fn is_empty(&self) -> bool {
        self.tree.read().is_empty()
    }

    /// Value-only fingerprint over a range. After convergence this equals the dated peer's
    /// [`value_fingerprint`](crate::ReconcileStore::value_fingerprint) over the same range.
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.tree.read().hash(&range)
    }

    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    /// Integrate inbound value-only updates by plain overwrite (the mirror holds no timestamp to
    /// compare against — it trusts the authoritative dated peer). Hooks run outside the map lock,
    /// so a hook may safely call back into the mirror.
    fn integrate(&self, updates: Vec<(K, ValueOnly<V>)>) {
        if updates.is_empty() {
            return;
        }
        {
            let hook = self.on_update.read();
            for (k, vo) in &updates {
                hook(k, vo);
            }
        }
        let mut guard = self.tree.write();
        for (k, vo) in updates {
            guard.insert(k, vo);
        }
    }

    /// Send our value-only comparison items to every known peer plus a random address (discovery),
    /// kicking off / continuing a value-only reconciliation round.
    pub async fn start_reconciliation(&self, send_buf: &mut Vec<u8>) {
        let segments = self.tree.read().start_diff();
        send_buf.clear();
        for segment in segments {
            Message::ValueComparisonItem::<K, WireDated<V>, ValueOnly<V>>(segment)
                .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
                .unwrap();
        }
        let mut peers = self.get_peers();
        // A random address out of the peer network, for discovery — like the dated store, we do not
        // add it to the known peers; a real peer there will answer and be recorded then.
        let addr = gen_ip(&mut *self.rng.write(), self.peer_net);
        peers.push(addr);
        for peer in peers {
            trace!("mirror start_diff {} bytes to {peer}", send_buf.len());
            send_to_retry(
                &self.socket,
                &self.authenticator,
                send_buf,
                (peer, self.port),
            )
            .await
            .unwrap();
        }
    }

    async fn handle_messages(
        &self,
        payload: auth::Payload<'_>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) {
        let payload = payload.as_bytes();
        trace!("mirror received {} bytes from {peer}", payload.len());
        let mut value_in_comparison = Vec::new();
        let mut value_updates: Vec<(K, ValueOnly<V>)> = Vec::new();
        let mut deserializer = Deserializer::from_slice(payload, DefaultOptions::new());
        loop {
            match Message::<K, WireDated<V>, ValueOnly<V>>::deserialize(&mut deserializer) {
                Err(ref kind) => {
                    if let bincode::ErrorKind::Io(err) = kind.as_ref() {
                        if err.kind() == std::io::ErrorKind::UnexpectedEof {
                            break;
                        }
                    }
                    // Never panic on network input (remote-DoS hardening, like the dated engine).
                    warn!(
                        "mirror failed to deserialize datagram from {peer}, dropping it: {kind:?}"
                    );
                    break;
                }
                Ok(Message::ValueComparisonItem(segment)) => value_in_comparison.push(segment),
                Ok(Message::ValueUpdate(update)) => value_updates.push(update),
                // The dated channel is meaningless to a mirror (it cannot store dated values nor
                // participate in #109). Ignore it.
                Ok(Message::ComparisonItem(_)) | Ok(Message::Update(_)) | Ok(Message::Ack(_)) => {}
            }
        }

        self.integrate(value_updates);

        if !value_in_comparison.is_empty() {
            debug!(
                "mirror received {} value-only segments",
                value_in_comparison.len()
            );
            let mut out_comparison = Vec::new();
            let mut differences = Vec::new();
            {
                let guard = self.tree.read();
                guard.diff_round(value_in_comparison, &mut out_comparison, &mut differences);
            }
            // `differences` are ranges this mirror would owe the peer. A read-only mirror never
            // sends authoritative values, so we deliberately drop them and only bounce back the
            // refined comparison items that keep the peer's side of the diff progressing.
            if !out_comparison.is_empty() {
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::<K, WireDated<V>, ValueOnly<V>>::ValueComparisonItem)
                    .collect();
                send_messages_to(
                    &messages,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &peer,
                    send_buf,
                )
                .await;
            }
        }
    }

    /// Run the mirror's reconciliation loop forever. Spawn this on a task; the mirror converges to
    /// the dated cluster's current values and reflects deletions as tombstones.
    pub async fn run(self) {
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        self.start_reconciliation(&mut send_buf).await;
        loop {
            match timeout(ACTIVITY_TIMEOUT, self.socket.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    debug!("mirror: no recent activity; initiating value-only diff");
                    self.start_reconciliation(&mut send_buf).await;
                }
                Ok(Err(err)) => warn!("mirror network error in recv_from: {err}"),
                Ok(Ok((size, peer))) => {
                    if peer.port() != self.port {
                        warn!(
                            "mirror received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    if size == recv_buf.len() {
                        warn!("mirror buffer too small for message, discarded");
                    } else {
                        match self.authenticator.open(&recv_buf[..size]) {
                            Some(payload) => {
                                self.handle_messages(payload, peer, &mut send_buf).await;
                                // Record the sender so we keep gossiping value-only diffs to it.
                                self.peers.write().insert(peer.ip(), Instant::now());
                            }
                            None => trace!(
                                "mirror dropped datagram from {peer}: missing or invalid MAC"
                            ),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile_store::Config;

    fn ephemeral_config() -> Config {
        Config {
            port: 0,
            listen_addr: "127.0.0.1".parse().unwrap(),
            peer_net: "127.0.0.1/8".parse().unwrap(),
            cluster_key: None,
            node_id: None,
            encrypt: false,
        }
    }

    /// `get` returns the live value, and absent keys are `None`.
    #[tokio::test]
    async fn get_returns_integrated_value() {
        let mirror = ReconcileMirror::<i32, String>::new(ephemeral_config()).await;
        assert!(mirror.get(&1).is_none());
        mirror.integrate(vec![(1, ValueOnly(Some("hello".to_string())))]);
        assert_eq!(mirror.get(&1).as_deref(), Some(&"hello".to_string()));
        assert!(mirror.contains_key(&1));
        assert_eq!(mirror.len(), 1);
    }

    /// A mirrored tombstone (`ValueOnly(None)`) hides the value but is still a stored entry.
    #[tokio::test]
    async fn mirrors_tombstones() {
        let mirror = ReconcileMirror::<i32, String>::new(ephemeral_config()).await;
        mirror.integrate(vec![(1, ValueOnly(Some("v".to_string())))]);
        assert_eq!(mirror.get(&1).as_deref(), Some(&"v".to_string()));

        // A later tombstone overwrites it: the value disappears from `get`, but the key is retained
        // as a tombstone (the mirror has no timestamp and trusts the authoritative peer).
        mirror.integrate(vec![(1, ValueOnly(None))]);
        assert!(mirror.get(&1).is_none());
        assert!(!mirror.contains_key(&1));
        assert_eq!(mirror.len(), 1, "the tombstone is retained as an entry");
    }

    /// The on-update hook fires for every integrated value, including tombstones.
    #[tokio::test]
    async fn on_update_hook_fires() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mirror = ReconcileMirror::<i32, i32>::new(ephemeral_config()).await;
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();
        mirror.add_on_update(move |_, _| {
            count2.fetch_add(1, Ordering::SeqCst);
        });
        mirror.integrate(vec![(1, ValueOnly(Some(10))), (2, ValueOnly(None))]);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// The mirror's value-only fingerprint matches an independently-built tree of the same logical
    /// content — i.e. timestamps genuinely play no part in the hash.
    #[tokio::test]
    async fn value_fingerprint_is_timestamp_independent() {
        let mirror = ReconcileMirror::<i32, String>::new(ephemeral_config()).await;
        mirror.integrate(vec![
            (1, ValueOnly(Some("a".to_string()))),
            (2, ValueOnly(None)),
        ]);

        let mut reference: HRTree<i32, ValueOnly<String>> = HRTree::new();
        reference.insert(1, ValueOnly(Some("a".to_string())));
        reference.insert(2, ValueOnly(None));

        assert_eq!(mirror.fingerprint(..), reference.hash(&..));
    }

    /// A live value and its `ValueOnly` projection hash identically only via the value-only basis:
    /// per-entry, the dateless mirror saves the whole `Hlc` (the point of #128).
    #[test]
    fn value_only_is_smaller_per_entry() {
        let dated = std::mem::size_of::<(Hlc, Option<u64>)>();
        let light = std::mem::size_of::<ValueOnly<u64>>();
        assert!(
            light < dated,
            "value-only entry ({light} B) should be smaller than dated entry ({dated} B)"
        );
    }
}

// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`Service`], a wrapper to a key-value map
//! to enable reconciliation between different instances over a network.

use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

use bincode::{DefaultOptions, Deserializer, Serializer};
use ipnet::IpNet;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::diff::Diffable;
use crate::gen_ip::gen_ip;
use crate::map::Map;
use crate::reconcilable::{Reconcilable, ReconciliationResult};

const BUFFER_SIZE: usize = 65507;

/// Wraps a key-value map to enable reconciliation between different instances over a network.
///
/// The service also keeps track of the addresses of other instances.
///
/// Provides wrappers for its underlying [`Map`]s insertion and deletion methods,
/// as well as its main service method: `run()`,
/// which must be called to actually synchronize with peers.
///
/// This struct does not handle removals. See
/// [`RemoveService`](crate::remove_service::RemoveService).
#[derive(Debug)]
pub(crate) struct InternalService<M> {
    map: Arc<RwLock<M>>,
    port: u16,
    socket: Arc<UdpSocket>,
    peer_net: IpNet,
    rng: Arc<RwLock<StdRng>>,
    peers: Arc<RwLock<HashSet<IpAddr>>>,
}

impl<M> InternalService<M> {
    pub async fn new(map: M, port: u16, listen_addr: IpAddr, peer_net: IpNet) -> Self {
        let socket = UdpSocket::bind(SocketAddr::new(listen_addr, port))
            .await
            .unwrap();
        debug!("Listening on: {}", socket.local_addr().unwrap());
        InternalService {
            map: Arc::new(RwLock::new(map)),
            port,
            socket: Arc::new(socket),
            peer_net,
            rng: Arc::new(RwLock::new(StdRng::from_entropy())),
            peers: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn with_seed(self, peer: IpAddr) -> Self {
        self.peers.write().unwrap().insert(peer);
        self
    }
}

impl<M> Clone for InternalService<M> {
    fn clone(&self) -> Self {
        InternalService {
            map: self.map.clone(),
            port: self.port,
            socket: self.socket.clone(),
            peer_net: self.peer_net,
            rng: self.rng.clone(),
            peers: self.peers.clone(),
        }
    }
}

/// Direct read access to the underlying map.
impl<M: Map> InternalService<M> {
    pub fn read(&self) -> RwLockReadGuard<'_, M> {
        self.map.read().unwrap()
    }
}

/// Represent an atomic message for the reconciliation protocol.
#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K: Serialize, V: Serialize, C: Serialize> {
    /// Provides information about a set of keys that allows checking
    /// whether there are differences between the two instances over this set
    ComparisonItem(C),
    /// Provides an individual key-value pair when the protocol
    /// has identified that it differs on the two instances
    Update((K, V)),
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Reconcilable + Send + Serialize + Sync + 'static,
        C: Debug + DeserializeOwned + Send + Serialize + Sync + 'static,
        D: Debug,
        M: Map<Key = K, Value = V, DifferenceItem = D>
            + Diffable<ComparisonItem = C, DifferenceItem = D>,
    > InternalService<M>
{
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let mut guard = self.map.write().unwrap();
        let ret = guard.insert(key.clone(), value.clone());
        let peers = self.peers.read().unwrap().clone();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let message = Message::Update::<K, V, C>((key, value));
            let messages = vec![message];
            let mut send_buf = Vec::new();
            for peer in peers {
                let peer = SocketAddr::new(peer, port);
                send_messages_to(&messages, Arc::clone(&socket), &peer, &mut send_buf).await;
            }
        });
        ret
    }

    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        let mut guard = self.map.write().unwrap();
        for (key, value) in key_values {
            guard.insert(key.clone(), value.clone());
        }
        let peers = self.peers.read().unwrap().clone();
        let messages: Vec<_> = key_values
            .iter()
            .map(|kv| Message::Update::<K, V, C>(kv.clone()))
            .collect();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let mut send_buf = Vec::new();
            for peer in peers {
                let peer = SocketAddr::new(peer, port);
                send_messages_to(&messages, Arc::clone(&socket), &peer, &mut send_buf).await;
            }
        });
    }

    pub async fn run<FI: Fn(&K, &V, Option<&V>)>(self, before_insert: FI) {
        // extra byte that easily detect when the buffer is too small
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        let recv_timeout = Duration::from_millis(100);
        // start the protocol at the beginning
        self.start_diff_protocol(&mut send_buf).await;
        // infinite loop
        loop {
            match timeout(recv_timeout, self.socket.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    // timeout
                    debug!("no recent activity; initiating diff protocol");
                    self.start_diff_protocol(&mut send_buf).await;
                }
                Ok(Err(err)) => {
                    // network error
                    warn!("network error in recv_from: {err}");
                }
                Ok(Ok((size, peer))) => {
                    // received datagram
                    if peer.port() != self.port {
                        warn!(
                            "received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    self.handle_messages(&recv_buf, (size, peer), &mut send_buf, &before_insert)
                        .await;
                    self.peers.write().unwrap().insert(peer.ip());
                }
            }
        }
    }

    async fn start_diff_protocol(&self, send_buf: &mut Vec<u8>) {
        let segments = {
            let guard = self.map.read().unwrap();
            guard.start_diff()
        };
        send_buf.clear();
        for segment in segments {
            Message::ComparisonItem::<K, V, C>(segment)
                .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
                .unwrap();
        }
        let mut peers = self.peers.read().unwrap().clone();
        // also try sending to another random IP from the peer network
        peers.insert(gen_ip(&mut *self.rng.write().unwrap(), self.peer_net));
        for peer in peers {
            trace!("start_diff {} bytes to {peer}", send_buf.len());
            self.socket
                .send_to(send_buf, (peer, self.port))
                .await
                .unwrap();
        }
    }

    async fn handle_messages<FI: Fn(&K, &V, Option<&V>)>(
        &self,
        recv_buf: &[u8],
        (size, peer): (usize, SocketAddr),
        send_buf: &mut Vec<u8>,
        before_insert: &FI,
    ) {
        if size == recv_buf.len() {
            warn!("Buffer too small for message, discarded");
            return;
        }
        trace!("received {} bytes from {peer}", size);
        let mut in_comparison = Vec::new();
        let mut updates = Vec::new();
        let mut deserializer = Deserializer::from_slice(&recv_buf[..size], DefaultOptions::new());
        // read messages in buffer
        loop {
            match Message::deserialize(&mut deserializer) {
                Err(ref kind) => {
                    if let bincode::ErrorKind::Io(err) = kind.as_ref() {
                        if err.kind() == std::io::ErrorKind::UnexpectedEof {
                            break;
                        }
                    }
                    panic!("failed to deserialize message: {:?}", kind);
                }
                Ok(Message::ComparisonItem(segment)) => in_comparison.push(segment),
                Ok(Message::Update(update)) => updates.push(update),
            }
        }
        // handle messages
        if !in_comparison.is_empty() {
            debug!("received {} segments", in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.map.read().unwrap();
                guard.diff_round(in_comparison, &mut out_comparison, &mut differences);
            }
            let mut messages = Vec::new();
            if !out_comparison.is_empty() {
                debug!("returning {} segments", out_comparison.len());
                trace!("segments: {out_comparison:?}");
                for segment in out_comparison {
                    messages.push(Message::ComparisonItem::<K, V, C>(segment))
                }
            }
            if !differences.is_empty() {
                debug!("returning {} diff_ranges", differences.len());
                trace!("diff_ranges: {differences:?}");
                let guard = self.map.read().unwrap();
                for update in guard.enumerate_diff_ranges(differences) {
                    messages.push(Message::Update(update));
                }
            }
            if !messages.is_empty() {
                send_messages_to(&messages, Arc::clone(&self.socket), &peer, send_buf).await;
            }
        }
        if !updates.is_empty() {
            debug!("received {} updates", updates.len());
            let mut guard = self.map.write().unwrap();
            for (k, v) in updates {
                let local_v = guard.get(&k);
                let do_change = local_v
                    .map(|local_v| local_v.reconcile(&v) == ReconciliationResult::KeepOther)
                    .unwrap_or(true);
                if do_change {
                    before_insert(&k, &v, local_v);
                    guard.insert(k, v);
                }
            }
        }
    }
}

async fn send_messages_to<K: Serialize, V: Serialize, C: Serialize>(
    messages: &[Message<K, V, C>],
    socket: Arc<UdpSocket>,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
) {
    debug!("sending {} messages to {peer}", messages.len());
    send_buf.clear();
    for message in messages {
        let last_size = send_buf.len();
        message
            .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
            .unwrap();
        if send_buf.len() > BUFFER_SIZE {
            trace!("sending {} bytes to {peer}", last_size);
            socket.send_to(&send_buf[..last_size], &peer).await.unwrap();
            trace!("sent {} bytes to {peer}", last_size);
            send_buf.drain(..last_size);
        }
    }
    trace!("sending last {} bytes to {peer}", send_buf.len());
    socket.send_to(send_buf, &peer).await.unwrap();
    trace!("sent last {} bytes to {peer}", send_buf.len());
}

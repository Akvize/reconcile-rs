// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`InternalService`], the inner layer of the [`Service`](crate::service::Service)
//! that handles communication between instances at the network level.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use ipnet::IpNet;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::diff::Diffable;
use crate::gen_ip::gen_ip;
use crate::map::Map;
use crate::reconcilable::{Reconcilable, ReconciliationResult};

const BUFFER_SIZE: usize = 65507;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);
const PEER_EXPIRATION: Duration = Duration::from_secs(60);

const MAX_SENDTO_RETRIES: u32 = 4;

/// The internal service at the network level.
/// This struct does not handle removals, which are managed by the external layer.
/// For more information, see [`Service`](crate::service::Service).
#[derive(Debug)]
pub(crate) struct InternalService<M> {
    pub(crate) map: Arc<RwLock<M>>,
    port: u16,
    socket: Arc<UdpSocket>,
    peer_net: IpNet,
    rng: Arc<RwLock<StdRng>>,
    peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
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
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_seed(self, addr: IpAddr) -> Self {
        let now = Instant::now();
        self.peers.write().unwrap().insert(addr, now);
        self
    }

    pub fn read(&self) -> RwLockReadGuard<'_, M> {
        self.map.read().unwrap()
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

async fn send_to_retry<A: ToSocketAddrs>(
    socket: &UdpSocket,
    buf: &[u8],
    target: A,
) -> std::io::Result<usize> {
    let mut res = Ok(0);
    for _ in 0..MAX_SENDTO_RETRIES {
        res = socket.send_to(buf, &target).await;
        if res.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    res
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
    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write().unwrap();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let mut guard = self.map.write().unwrap();
        let ret = guard.insert(key.clone(), value.clone());
        let peers = self.get_peers();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let message = Message::Update::<K, V, C>((key, value));
            let messages = vec![message];
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
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
        let peers = self.get_peers();
        let messages: Vec<_> = key_values
            .iter()
            .map(|kv| Message::Update::<K, V, C>(kv.clone()))
            .collect();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(&messages, Arc::clone(&socket), &peer, &mut send_buf).await;
            }
        });
    }

    pub async fn run<FI: Fn(&K, &V, Option<&V>)>(self, before_insert: FI) {
        // extra byte that easily detect when the buffer is too small
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        let recv_timeout = ACTIVITY_TIMEOUT;
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
                    let now = Instant::now();
                    let addr = peer.ip();
                    self.peers.write().unwrap().insert(addr, now);
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
        let mut peers = self.get_peers();
        // select a random address out of the peer network
        // NOTE: the random address might not correspond to a real peer, so we do not add it to the
        // list of known peers, just to our local copies of the addresses; if a peer exists at this
        // address, they will eventually send us a message in return, and we will add them to the
        // list of known peer
        let addr = gen_ip(&mut *self.rng.write().unwrap(), self.peer_net);
        peers.push(addr);
        // initiate the reconciliation protocol with all the known peers, and a random one
        for peer in peers {
            trace!("start_diff {} bytes to {peer}", send_buf.len());
            send_to_retry(&self.socket, send_buf, (peer, self.port))
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
            send_to_retry(&socket, &send_buf[..last_size], &peer)
                .await
                .unwrap();
            trace!("sent {} bytes to {peer}", last_size);
            send_buf.drain(..last_size);
        }
    }
    trace!("sending last {} bytes to {peer}", send_buf.len());
    send_to_retry(&socket, send_buf, &peer).await.unwrap();
    trace!("sent last {} bytes to {peer}", send_buf.len());
}

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
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::diff::{Diffable, HashSegment};
use crate::gen_ip::gen_ip;
use crate::reconcilable::{Reconcilable, ReconciliationResult};
use crate::service::ServiceConfig;
use crate::{HRTree, HashRangeQueryable};

const BUFFER_SIZE: usize = 65507;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);
const PEER_EXPIRATION: Duration = Duration::from_secs(60);

const MAX_SENDTO_RETRIES: u32 = 4;

type PreInsertCallback<K, V> = Box<dyn Send + Sync + Fn(&K, &V)>;

/// The internal service at the network level.
/// This struct does not handle removals, which are managed by the external layer.
/// For more information, see [`Service`](crate::service::Service).
pub(crate) struct InternalService<K, V> {
    pub(crate) map: Arc<RwLock<HRTree<K, V>>>,
    port: u16,
    socket: Arc<UdpSocket>,
    peer_net: IpNet,
    rng: Arc<RwLock<StdRng>>,
    pub(crate) peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
    pub(crate) pre_insert: Arc<RwLock<PreInsertCallback<K, V>>>,
}

impl<K, V> Clone for InternalService<K, V> {
    fn clone(&self) -> Self {
        InternalService {
            map: self.map.clone(),
            port: self.port,
            socket: self.socket.clone(),
            peer_net: self.peer_net,
            rng: self.rng.clone(),
            peers: self.peers.clone(),
            pre_insert: self.pre_insert.clone(),
        }
    }
}

/// Represent an atomic message for the reconciliation protocol.
#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K: Serialize, V: Serialize> {
    /// Provides information about a set of keys that allows checking
    /// whether there are differences between the two instances over this set
    ComparisonItem(HashSegment<K>),
    /// Provides an individual key-value pair when the protocol
    /// has identified that it differs on the two instances
    Update((K, V)),
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Reconcilable + Send + Serialize + Sync + 'static,
    > InternalService<K, V>
{
    pub async fn new(config: ServiceConfig) -> Self {
        let socket = UdpSocket::bind(SocketAddr::new(config.listen_addr, config.port))
            .await
            .unwrap();
        debug!("Listening on: {}", socket.local_addr().unwrap());
        let map = HRTree::<K, V>::new();
        InternalService {
            map: Arc::new(RwLock::new(map)),
            port: config.port,
            socket: Arc::new(socket),
            peer_net: config.peer_net,
            rng: Arc::new(RwLock::new(StdRng::from_entropy())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            pre_insert: Arc::new(RwLock::new(Box::new(|_, _| {}))),
        }
    }

    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> u64 {
        self.map.read().hash(&range)
    }

    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        // 1) Call the pre-insert hook *before* taking the map lock
        (self.pre_insert.read())(&key, &value);

        // 2) Now acquire write lock and insert
        let mut guard = self.map.write();
        guard.insert(key.clone(), value.clone())
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self.just_insert(key.clone(), value.clone());
        let peers = self.get_peers();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let message = Message::Update::<K, V>((key, value));
            let messages = vec![message];
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(&messages, Arc::clone(&socket), &peer, &mut send_buf).await;
            }
        });
        ret
    }

    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        // 1) First run all pre-insert hooks outside the map lock
        for (key, value) in key_values {
            (self.pre_insert.read())(key, value);
        }
        // 2) Then acquire the write lock once and insert them
        let mut guard = self.map.write();
        for (key, value) in key_values {
            guard.insert(key.clone(), value.clone());
        }
    }

    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.just_insert_bulk(key_values);
        let peers = self.get_peers();
        let messages: Vec<_> = key_values
            .iter()
            .map(|kv| Message::Update::<K, V>(kv.clone()))
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

    pub async fn run(self) {
        // extra byte that easily detect when the buffer is too small
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        let recv_timeout = ACTIVITY_TIMEOUT;
        // start the protocol at the beginning
        self.start_reconciliation(&mut send_buf).await;
        // infinite loop
        loop {
            match timeout(recv_timeout, self.socket.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    // timeout
                    debug!("no recent activity; initiating diff protocol");
                    self.start_reconciliation(&mut send_buf).await;
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
                    self.handle_messages(&recv_buf, (size, peer), &mut send_buf)
                        .await;
                    let now = Instant::now();
                    let addr = peer.ip();
                    self.peers.write().insert(addr, now);
                }
            }
        }
    }

    pub async fn start_reconciliation(&self, send_buf: &mut Vec<u8>) {
        let segments = {
            let guard = self.map.read();
            guard.start_diff()
        };
        send_buf.clear();
        for segment in segments {
            Message::ComparisonItem::<K, V>(segment)
                .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
                .unwrap();
        }
        let mut peers = self.get_peers();
        // select a random address out of the peer network
        // NOTE: the random address might not correspond to a real peer, so we do not add it to the
        // list of known peers, just to our local copies of the addresses; if a peer exists at this
        // address, they will eventually send us a message in return, and we will add them to the
        // list of known peer
        let addr = gen_ip(&mut *self.rng.write(), self.peer_net);
        peers.push(addr);
        // initiate the reconciliation protocol with all the known peers, and a random one
        for peer in peers {
            trace!("start_diff {} bytes to {peer}", send_buf.len());
            send_to_retry(&self.socket, send_buf, (peer, self.port))
                .await
                .unwrap();
        }
    }

    async fn handle_messages(
        &self,
        recv_buf: &[u8],
        (size, peer): (usize, SocketAddr),
        send_buf: &mut Vec<u8>,
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
                let guard = self.map.read();
                guard.diff_round(in_comparison, &mut out_comparison, &mut differences);
            }
            let mut messages = Vec::new();
            if !out_comparison.is_empty() {
                debug!("returning {} segments", out_comparison.len());
                trace!("segments: {out_comparison:?}");
                for segment in out_comparison {
                    messages.push(Message::ComparisonItem::<K, V>(segment))
                }
            }
            if !differences.is_empty() {
                debug!("returning {} diff_ranges", differences.len());
                trace!("diff_ranges: {differences:?}");
                let guard = self.map.read();
                for range in differences {
                    for update in guard.get_range(&range).map(|(k, v)| (k.clone(), v.clone())) {
                        messages.push(Message::Update(update));
                    }
                }
            }
            if !messages.is_empty() {
                send_messages_to(&messages, Arc::clone(&self.socket), &peer, send_buf).await;
            }
        }
        if !updates.is_empty() {
            debug!("received {} updates", updates.len());
            let mut guard = self.map.write();
            for (k, v) in updates {
                let local_v = guard.get(&k);
                let do_change = local_v
                    .map(|local_v| ReconciliationResult::KeepOther == local_v.reconcile(&v))
                    .unwrap_or(true);
                if do_change {
                    (self.pre_insert.read())(&k, &v);
                    guard.insert(k, v);
                }
            }
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

async fn send_messages_to<K: Serialize, V: Serialize>(
    messages: &[Message<K, V>],
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

#[cfg(test)]
mod deadlock_regressions {

    use crate::{service::ServiceConfig, Service};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_insert_hook_can_call_insert_again_without_deadlock() {
        let config = ServiceConfig::default()
            .with_port(8080)
            .with_listen_addr("127.0.0.44".parse().unwrap());
        // let tree = HRTree::from_iter(vec![(1, 10), (2, 20)]);
        let svc = Service::new(config).await;
        svc.insert_bulk(&vec![(1, 10_u8)]);

        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();

        let hook_svc = svc.clone();
        // Install a pre-insert hook that itself calls insert on the same Service
        let once = Arc::new(AtomicBool::new(false));
        let guard = once.clone();
        svc.add_pre_insert(move |&k, &v| {
            // inner insert should no longer deadlock
            if !guard.swap(true, Ordering::SeqCst) {
                let _ = hook_svc.just_insert(k + 100, v.1.unwrap_or_default() + 100);
                // TODO Check vs Utc::now()
            }
            flag2.store(true, Ordering::SeqCst);
        });

        // Trigger our hook via a normal insert
        let _ = svc.just_insert(42, 99);
        assert!(
            flag.load(Ordering::SeqCst),
            "The pre-insert hook never ran to completion (likely deadlocked)"
        );
    }
}

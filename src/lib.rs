// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This crate provides a key-data map structure [`HRTree`](hrtree::HRTree) that can be used together
//! with the reconciliation [`Service`]. Different instances can talk together over
//! UDP to efficiently reconcile their differences.

//! All the data is available locally in all instances, and the user can be
//! notified of changes to the collection with an insertion hook.

//! The protocol allows finding a difference over millions of elements with a limited
//! number of round-trips. It should also work well to populate an instance from
//! scratch from other instances.

pub mod diff;
pub mod hrtree;
pub mod map;
pub mod reconcilable;

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

use bincode::{DefaultOptions, Deserializer, Serializer};
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::diff::Diffable;
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
/// This struct does not handle removals. See [`RemoveService`].
#[derive(Debug)]
pub struct Service<M> {
    map: Arc<RwLock<M>>,
    peers: Arc<RwLock<Vec<SocketAddr>>>,
}

impl<M> Service<M> {
    pub fn new(map: M) -> Self {
        Service {
            map: Arc::new(RwLock::new(map)),
            peers: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl<M> Clone for Service<M> {
    fn clone(&self) -> Self {
        Service {
            map: self.map.clone(),
            peers: self.peers.clone(),
        }
    }
}

/// Direct read access to the underlying map.
impl<M: Map> Service<M> {
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
        R: Map<Key = K, Value = V, DifferenceItem = D>
            + Diffable<ComparisonItem = C, DifferenceItem = D>,
    > Service<R>
{
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let mut guard = self.map.write().unwrap();
        let ret = guard.insert(key.clone(), value.clone());
        let peers = self.peers.read().unwrap().clone();
        tokio::spawn(async {
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let message = Message::Update::<K, V, C>((key, value));
            let messages = vec![message];
            let mut send_buf = Vec::new();
            for peer in peers {
                send_messages_to(&messages, &socket, &peer, &mut send_buf).await;
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
        tokio::spawn(async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let mut send_buf = Vec::new();
            for peer in peers {
                send_messages_to(&messages, &socket, &peer, &mut send_buf).await;
            }
        });
    }

    pub async fn run<FI: Fn(&K, &V, Option<&V>)>(
        self,
        socket: UdpSocket,
        other_addr: SocketAddr,
        before_insert: FI,
    ) {
        self.peers.write().unwrap().push(other_addr);

        // extra byte that easily detect when the buffer is too small
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        let recv_timeout = Duration::from_millis(100);
        // start the protocol at the beginning
        self.start_diff_protocol(&socket, other_addr, &mut send_buf)
            .await;
        // infinite loop
        loop {
            match timeout(recv_timeout, socket.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    // timeout
                    debug!("no recent activity; initiating diff protocol");
                    self.start_diff_protocol(&socket, other_addr, &mut send_buf)
                        .await;
                }
                Ok(Err(err)) => {
                    // network error
                    warn!("network error in recv_from: {err}");
                }
                Ok(Ok((size, peer))) => {
                    // received datagram
                    self.handle_messages(
                        &socket,
                        &recv_buf,
                        (size, peer),
                        &mut send_buf,
                        &before_insert,
                    )
                    .await;
                }
            }
        }
    }

    async fn start_diff_protocol(
        &self,
        socket: &UdpSocket,
        other_addr: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) {
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
        trace!("start_diff {} bytes to {other_addr}", send_buf.len());
        socket.send_to(send_buf, &other_addr).await.unwrap();
    }

    async fn handle_messages<FI: Fn(&K, &V, Option<&V>)>(
        &self,
        socket: &UdpSocket,
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
            let res = Message::deserialize(&mut deserializer);
            if let Err(kind) = res.as_ref() {
                if let bincode::ErrorKind::Io(err) = kind.as_ref() {
                    if err.kind() == std::io::ErrorKind::UnexpectedEof {
                        break;
                    }
                }
            }
            let message: Message<K, V, C> = res.unwrap();
            match message {
                Message::ComparisonItem(segment) => {
                    in_comparison.push(segment);
                }
                Message::Update(update) => {
                    updates.push(update);
                }
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
                send_messages_to(&messages, socket, &peer, send_buf).await;
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
    socket: &UdpSocket,
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

pub type MaybeTombstone<V> = Option<V>;
pub type DatedMaybeTombstone<V> = (DateTime<Utc>, MaybeTombstone<V>);

/// A wrapper to the [`Service`] to provide a remove method.
pub struct RemoveService<M: Map> {
    service: Service<M>,
    tombstones: Arc<RwLock<HashMap<M::Key, DateTime<Utc>>>>,
}

impl<M: Map> RemoveService<M> {
    pub fn new(map: M) -> Self {
        RemoveService {
            service: Service::new(map),
            tombstones: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, M> {
        self.service.map.read().unwrap()
    }
}

impl<M: Map> Clone for RemoveService<M> {
    fn clone(&self) -> Self {
        RemoveService {
            service: self.service.clone(),
            tombstones: self.tombstones.clone(),
        }
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Send + Serialize + Sync + 'static,
        C: Debug + DeserializeOwned + Send + Serialize + Sync + 'static,
        D: Debug,
        R: Map<Key = K, Value = DatedMaybeTombstone<V>, DifferenceItem = D>
            + Diffable<ComparisonItem = C, DifferenceItem = D>,
    > RemoveService<R>
{
    pub fn insert(&self, key: K, value: V, timestamp: DateTime<Utc>) -> Option<V> {
        let mut guard = self.tombstones.write().unwrap();
        guard.remove(&key);

        let ret = self.service.insert(key, (timestamp, Some(value)));
        ret.and_then(|t| t.1)
    }

    pub fn insert_bulk(&self, key_values: &[(K, V, DateTime<Utc>)]) {
        let mut guard = self.tombstones.write().unwrap();
        for (k, _, _) in key_values {
            guard.remove(k);
        }

        self.service.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v, t)| (k.clone(), (*t, Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn remove(&self, key: &K, timestamp: DateTime<Utc>) -> Option<V> {
        let mut guard = self.tombstones.write().unwrap();
        guard.insert(key.clone(), timestamp);

        let ret = self.service.insert(key.clone(), (timestamp, None));
        ret.and_then(|t| t.1)
    }

    pub fn remove_bulk(&self, keys: &[(K, DateTime<Utc>)]) {
        let mut guard = self.tombstones.write().unwrap();
        for (k, t) in keys {
            guard.insert(k.clone(), *t);
        }

        self.service.insert_bulk(
            &keys
                .iter()
                .map(|(k, t)| (k.clone(), (*t, None)))
                .collect::<Vec<_>>(),
        );
    }

    pub async fn run<
        FI: Fn(&K, &(DateTime<Utc>, Option<V>), Option<&(DateTime<Utc>, Option<V>)>),
    >(
        self,
        socket: UdpSocket,
        other_addr: SocketAddr,
        before_insert: FI,
    ) {
        self.service.run(socket, other_addr, before_insert).await
    }
}

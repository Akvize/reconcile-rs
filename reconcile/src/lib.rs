pub mod map;
pub mod reconcilable;

use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

use bincode::{DefaultOptions, Deserializer, Serializer};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use htree::Diffable;

use crate::map::Map;
use crate::reconcilable::{Reconcilable, ReconciliationResult};

const BUFFER_SIZE: usize = 65507;

#[derive(Debug)]
pub struct Service<M> {
    map: Arc<RwLock<M>>,
}

impl<M> Service<M> {
    pub fn new(map: M) -> Self {
        Service {
            map: Arc::new(RwLock::new(map)),
        }
    }
}

impl<M> Clone for Service<M> {
    fn clone(&self) -> Self {
        Service {
            map: self.map.clone(),
        }
    }
}

impl<K, V, M: Map<Key = K, Value = V>> Service<M> {
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let mut guard = self.map.write().unwrap();
        guard.insert(key, value)
    }

    pub fn read(&self) -> RwLockReadGuard<'_, M> {
        self.map.read().unwrap()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K: Serialize, V: Serialize, C: Serialize> {
    ComparisonItem(C),
    Update((K, V)),
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Serialize,
        V: Clone + DeserializeOwned + Hash + Reconcilable + Serialize,
        C: Debug + DeserializeOwned + Serialize,
        D: Debug,
        R: Map<Key = K, Value = V, DifferenceItem = D>
            + Diffable<ComparisonItem = C, DifferenceItem = D>,
    > Service<R>
{
    pub async fn run<FI: Fn(&K, &V, Option<&V>), FU: Fn(&Self)>(
        self,
        socket: UdpSocket,
        other_addr: SocketAddr,
        before_insert: FI,
        after_sync: FU,
    ) {
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
                        &after_sync,
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

    async fn handle_messages<FI: Fn(&K, &V, Option<&V>), FU: Fn(&Self)>(
        &self,
        socket: &UdpSocket,
        recv_buf: &[u8],
        (size, peer): (usize, SocketAddr),
        send_buf: &mut Vec<u8>,
        before_insert: &FI,
        after_sync: &FU,
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
        }
        if !updates.is_empty() {
            debug!("received {} updates", updates.len());
            let mut changed = false;
            {
                let mut guard = self.map.write().unwrap();
                for (k, v) in updates {
                    let local_v = guard.get(&k);
                    let do_change = local_v
                        .map(|local_v| local_v.reconcile(&v) == ReconciliationResult::KeepOther)
                        .unwrap_or(true);
                    if do_change {
                        before_insert(&k, &v, local_v);
                        guard.insert(k, v);
                        changed = true;
                    }
                }
            }
            if changed {
                after_sync(self);
            }
        }
    }
}

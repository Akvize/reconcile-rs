use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use diff::HashSegment;

use reconcilable::Reconcilable;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::UdpSocket;
use tracing::{debug, info, trace, warn};

const BUFFER_SIZE: usize = 60000;

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K: Serialize, V: Serialize> {
    HashSegment(HashSegment<K>),
    Update((K, V)),
}

pub async fn run<
    K: Clone + Debug + DeserializeOwned + Hash + Ord + Serialize,
    V: Clone + DeserializeOwned + Hash + Serialize,
    R: Reconcilable<Key = K, Value = V>,
>(
    socket: Arc<UdpSocket>,
    other_addr: SocketAddr,
    state: Arc<RwLock<R>>,
) -> Result<(), std::io::Error> {
    // extra byte that easily detect when the buffer is too small
    let mut recv_buf = [0; BUFFER_SIZE + 1];
    let mut send_buf = Vec::new();
    let my_options = DefaultOptions::new();
    let mut last_activity = None;
    // infinite loop
    loop {
        if let Ok((size, peer)) = socket.try_recv_from(&mut recv_buf) {
            last_activity = Some(Instant::now());
            if size == recv_buf.len() {
                warn!("Buffer too small for message, discarded");
                continue;
            }
            trace!("got {} bytes from {peer}", size);
            let mut segments = Vec::new();
            let mut updates = Vec::new();
            let mut deserializer = Deserializer::from_slice(&recv_buf[..size], my_options);
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
                let message: Message<K, V> = res.unwrap();
                match message {
                    Message::HashSegment(segment) => {
                        segments.push(segment);
                    }
                    Message::Update(update) => {
                        updates.push(update);
                    }
                }
            }
            // handle messages
            if !segments.is_empty() {
                trace!("got {} segments", segments.len());
                let mut diffs = Vec::new();
                let segments = {
                    let guard = state.read().unwrap();
                    guard.diff_round(&mut diffs, segments)
                };
                let mut messages = Vec::new();
                if !segments.is_empty() {
                    debug!("Split in {} segments", segments.len());
                    for segment in segments {
                        messages.push(Message::HashSegment::<K, V>(segment))
                    }
                }
                if !diffs.is_empty() {
                    let guard = state.read().unwrap();
                    info!("Found {} diffs", diffs.len());
                    debug!("Diffs: {diffs:?}");
                    for update in guard.send_updates(diffs) {
                        messages.push(Message::Update(update));
                    }
                }
                if !messages.is_empty() {
                    debug!("sending {} messages to {peer}", messages.len());
                    send_buf.clear();
                    for message in messages {
                        let last_size = send_buf.len();
                        message
                            .serialize(&mut Serializer::new(&mut send_buf, my_options))
                            .unwrap();
                        if send_buf.len() > BUFFER_SIZE {
                            socket.send_to(&send_buf[..last_size], &peer).await?;
                            debug!("sent {} bytes to {peer}", send_buf.len());
                            send_buf.drain(..last_size);
                        }
                    }
                    socket.send_to(&send_buf, &peer).await?;
                    debug!("sent {} bytes to {peer}", send_buf.len());
                    last_activity = Some(Instant::now());
                }
            }
            if !updates.is_empty() {
                debug!("got {} updates", updates.len());
                let mut guard = state.write().unwrap();
                let signature = guard.reconcile(updates);
                info!("Updated state; global hash is now {}", signature);
            }
        }
        let is_active = last_activity
            .map(|last_activity| Instant::now() - last_activity < Duration::from_millis(10))
            .unwrap_or(false);
        if !is_active {
            let segments = {
                let guard = state.read().unwrap();
                guard.start_diff()
            };
            send_buf.clear();
            for segment in segments {
                Message::HashSegment::<K, V>(segment)
                    .serialize(&mut Serializer::new(&mut send_buf, my_options))
                    .unwrap();
            }
            trace!("start_diff {} bytes to {other_addr}", send_buf.len());
            socket.send_to(&send_buf, &other_addr).await?;
            last_activity = Some(Instant::now());
        }
    }
}

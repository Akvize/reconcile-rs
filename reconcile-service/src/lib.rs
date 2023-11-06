use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};

use diff::{Diffable, HashSegment};
use reconcilable::{Map, Reconcilable, ReconciliationResult};

const BUFFER_SIZE: usize = 65507;

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K: Serialize, V: Serialize> {
    HashSegment(HashSegment<K>),
    Update((K, V)),
}

pub async fn run<
    K: Clone + Debug + DeserializeOwned + Hash + Ord + Serialize,
    V: Clone + DeserializeOwned + Hash + Reconcilable + Serialize,
    R: Map<Key = K, Value = V> + Diffable<Key = K>,
    FI: Fn(&K, &V, Option<&V>),
    FU: Fn(&R),
>(
    socket: UdpSocket,
    other_addr: SocketAddr,
    mut reconcilable: R,
    pre_insert: FI,
    post_change: FU,
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
            trace!("received {} bytes from {peer}", size);
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
                debug!("received {} segments", segments.len());
                let mut diff_ranges = Vec::new();
                let mut out_segments = Vec::new();
                reconcilable.diff_round(segments, &mut out_segments, &mut diff_ranges);
                let mut messages = Vec::new();
                if !out_segments.is_empty() {
                    debug!("returning {} segments", out_segments.len());
                    trace!("segments: {out_segments:?}");
                    for segment in out_segments {
                        messages.push(Message::HashSegment::<K, V>(segment))
                    }
                }
                if !diff_ranges.is_empty() {
                    debug!("returning {} diff_ranges", diff_ranges.len());
                    trace!("diff_ranges: {diff_ranges:?}");
                    for update in reconcilable.enumerate_diff_ranges(diff_ranges) {
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
                            trace!("sending {} bytes to {peer}", last_size);
                            socket.send_to(&send_buf[..last_size], &peer).await?;
                            trace!("sent {} bytes to {peer}", last_size);
                            send_buf.drain(..last_size);
                        }
                    }
                    trace!("sending last {} bytes to {peer}", send_buf.len());
                    socket.send_to(&send_buf, &peer).await?;
                    trace!("sent last {} bytes to {peer}", send_buf.len());
                    last_activity = Some(Instant::now());
                }
            }
            if !updates.is_empty() {
                debug!("received {} updates", updates.len());
                let mut changed = false;
                for (k, v) in updates {
                    let local_v = reconcilable.get(&k);
                    let do_change = local_v
                        .map(|local_v| local_v.reconcile(&v) == ReconciliationResult::KeepOther)
                        .unwrap_or(true);
                    if do_change {
                        pre_insert(&k, &v, local_v);
                        reconcilable.insert(k, v);
                        changed = true;
                    }
                }
                if changed {
                    post_change(&reconcilable);
                }
            }
        }
        let is_active = last_activity
            .map(|last_activity| Instant::now() - last_activity < Duration::from_millis(10000))
            .unwrap_or(false);
        if !is_active {
            debug!("no recent activity; initiating diff protocol");
            let segments = reconcilable.start_diff();
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

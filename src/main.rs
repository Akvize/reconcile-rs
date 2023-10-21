use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use chrono::{DateTime, Utc};
use clap::Parser;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn, trace};

use reconciliate::diff::{Diffable, HashRangeQueryable, HashSegment};
use reconciliate::htree::HTree;

const BUFFER_SIZE: usize = 60000;

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K: Serialize, V: Serialize> {
    HashSegment(HashSegment<K>),
    Update((K, V)),
}

async fn answer_queries<
    K: Clone + Debug + DeserializeOwned + Hash + Ord + Serialize,
    V: Clone + DeserializeOwned + Hash + Serialize,
    F: Fn(&V, V) -> Option<V>,
>(
    socket: Arc<UdpSocket>,
    other_addr: SocketAddr,
    tree: Arc<RwLock<HTree<K, V>>>,
    conflict_handler: F)  -> Result<(), std::io::Error> {
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
                    let guard = tree.read().unwrap();
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
                    let guard = tree.read().unwrap();
                    info!("Found diffs: {diffs:?}");
                    for diff in diffs {
                        for (k, v) in guard.get_range(&diff) {
                            messages.push(Message::Update((k.clone(), v.clone())));
                        }
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
                let mut guard = tree.write().unwrap();
                for (k, v) in updates {
                    if let Some(local_v) = guard.get(&k) {
                        if let Some(v) = conflict_handler(local_v, v) {
                            guard.insert(k, v);
                        }
                    } else {
                        guard.insert(k, v);
                    }
                }
                info!("Updated state; global hash is now {}", guard.hash(&..));
            }
        }
        let is_active = last_activity
            .map(|last_activity| Instant::now() - last_activity < Duration::from_millis(10))
            .unwrap_or(false);
        if !is_active {
            let segments = {
                let guard = tree.read().unwrap();
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

#[derive(Parser)]
struct Args {
    listen_addr: SocketAddr,
    other_addr: SocketAddr,
    elements: usize,
    #[arg(short, long, default_value_t = tracing::Level::INFO)]
    log_level: tracing::Level,
}

#[tokio::main]
async fn main() {
    let Args {
        listen_addr,
        other_addr,
        elements,
        log_level,
    } = Args::parse();

    tracing_subscriber::fmt().with_max_level(log_level).init();

    let socket = Arc::new(UdpSocket::bind(listen_addr).await.unwrap());
    info!("Listening on: {}", socket.local_addr().unwrap());

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value = chrono::offset::Utc::now().to_string();
        key_values.push((key, value));
    }
    let tree = HTree::from_iter(key_values.into_iter());

    info!("Global hash is {}", tree.hash(&..));
    let state = Arc::new(RwLock::new(tree));
    let conflict_handler = |local_v: &String, v: String| -> Option<String> {
        if DateTime::<Utc>::from_str(local_v).unwrap() < DateTime::<Utc>::from_str(&v).unwrap() {
            debug!("Keeping local val {local_v}, dropping remote val {v}");
            return None;
        }
        debug!("Replacing local val {local_v} with remote val {v}");
        Some(v)
    };

    answer_queries(Arc::clone(&socket), other_addr, Arc::clone(&state), conflict_handler)
        .await
        .unwrap();
}

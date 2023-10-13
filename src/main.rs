use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use futures::future::{select, Either};
use rand::{Rng, SeedableRng};
use rmp_serde::{Deserializer, Serializer};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use reconciliate::diff::{Diffable, HashRangeQueryable, HashSegment};
use reconciliate::htree::HTree;

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K, V> {
    HashSegment(HashSegment<K>),
    Update((K, V)),
}

async fn answer_queries(
    socket: Arc<UdpSocket>,
    tree: Arc<RwLock<HTree<u64, u64>>>,
) -> Result<(), std::io::Error> {
    let mut recv_buf = [0; 4096];
    let mut send_buf = Vec::new();
    // infinite loop
    loop {
        let (size, peer) = socket.recv_from(&mut recv_buf).await?;
        if size == recv_buf.len() {
            warn!("Buffer too small for message, discarded");
            continue;
        }
        debug!("got {} bytes from {peer}", size);
        let mut segments = Vec::new();
        let mut updates = Vec::new();
        let mut deserializer = Deserializer::new(&recv_buf[..size]);
        // read messages in buffer
        loop {
            let res = Message::deserialize(&mut deserializer);
            if let Err(rmp_serde::decode::Error::InvalidMarkerRead(ref error)) = res {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    break;
                }
            }
            let message: Message<u64, u64> = res.unwrap();
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
            debug!("got {} segments", segments.len());
            let mut diffs = Vec::new();
            let segments = {
                let guard = tree.read().unwrap();
                guard.diff_round(&mut diffs, segments)
            };
            let mut messages = Vec::new();
            if !segments.is_empty() {
                debug!("Split in {} segments", segments.len());
                for segment in segments {
                    messages.push(Message::HashSegment::<u64, u64>(segment))
                }
            }
            if !diffs.is_empty() {
                let guard = tree.read().unwrap();
                info!("Found diffs: {diffs:?}");
                for diff in diffs {
                    for (k, v) in guard.get_range(&diff) {
                        messages.push(Message::Update((*k, *v)));
                    }
                }
            }
            if !messages.is_empty() {
                send_buf.clear();
                let n_messages = messages.len();
                for message in messages {
                    message
                        .serialize(&mut Serializer::new(&mut send_buf))
                        .unwrap();
                }
                debug!(
                    "sending {n_messages} messages {} bytes to {peer}",
                    send_buf.len()
                );
                socket.send_to(&send_buf, &peer).await?;
            }
        }
        if !updates.is_empty() {
            debug!("got {} updates", updates.len());
            let mut guard = tree.write().unwrap();
            for (k, v) in updates {
                if let Some(cur) = guard.get(&k) {
                    // conflict resolution
                    if &v > cur {
                        guard.insert(k, v);
                        continue;
                    }
                }
                guard.insert(k, v);
            }
            info!("Updated state; global hash is now {}", guard.hash(&..));
        }
    }
}

async fn send_queries(
    socket: Arc<UdpSocket>,
    other_addr: SocketAddr,
    tree: Arc<RwLock<HTree<u64, u64>>>,
) -> Result<(), std::io::Error> {
    let mut send_buf = Vec::new();
    loop {
        let segments = {
            let guard = tree.read().unwrap();
            guard.start_diff()
        };
        send_buf.clear();
        for segment in segments {
            Message::HashSegment::<u64, u64>(segment)
                .serialize(&mut Serializer::new(&mut send_buf))
                .unwrap();
        }
        debug!("start_diff {} bytes to {other_addr}", send_buf.len());
        socket.send_to(&send_buf, &other_addr).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
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

    let mut tree = HTree::new();
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    for _ in 0..elements {
        let key: u64 = rng.gen::<u64>();
        let value: u64 = rng.gen();
        tree.insert(key, value);
    }

    info!("Global hash is {}", tree.hash(&..));
    let state = Arc::new(RwLock::new(tree));

    let handle_recv = tokio::spawn(answer_queries(Arc::clone(&socket), Arc::clone(&state)));
    let handle_send = tokio::spawn(send_queries(socket, other_addr, state));
    match select(handle_recv, handle_send).await {
        Either::Left((left, _right)) => left.unwrap().unwrap(),
        Either::Right((right, _left)) => right.unwrap().unwrap(),
    }
}

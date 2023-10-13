use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use futures::future::{select, Either};
use rand::{Rng, SeedableRng};
use rmp_serde::{decode, Serializer};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use reconciliate::diff::{Diffable, HashRangeQueryable, HashSegment};
use reconciliate::htree::HTree;

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K, V> {
    HashSegments(Vec<HashSegment<K>>),
    Updates(Vec<(K, V)>),
}

async fn answer_queries(
    socket: Arc<UdpSocket>,
    tree: Arc<RwLock<HTree<u64, u64>>>,
) -> Result<(), std::io::Error> {
    let mut recv_buf = [0; 4096];
    let mut send_buf = Vec::new();
    loop {
        let (size, peer) = socket.recv_from(&mut recv_buf).await?;
        if size == recv_buf.len() {
            warn!("Buffer too small for message, discarded");
            continue;
        }
        let message: Message<u64, u64> = decode::from_slice(&recv_buf[..size]).unwrap();
        match message {
            Message::HashSegments(segments) => {
                debug!("got {} segments {} bytes from {peer}", segments.len(), size);
                let mut diffs = Vec::new();
                let segments = {
                    let guard = tree.read().unwrap();
                    guard.diff_round(&mut diffs, segments)
                };
                if !segments.is_empty() {
                    send_buf.clear();
                    let n_segments = segments.len();
                    Message::HashSegments::<u64, u64>(segments)
                        .serialize(&mut Serializer::new(&mut send_buf))
                        .unwrap();
                    debug!(
                        "sending {n_segments} segments {} bytes to {peer}",
                        send_buf.len()
                    );
                    socket.send_to(&send_buf, &peer).await?;
                }
                if !diffs.is_empty() {
                    let mut updates = Vec::new();
                    {
                        let guard = tree.read().unwrap();
                        info!("Found diffs: {diffs:?}");
                        for diff in diffs {
                            for (k, v) in guard.get_range(&diff.0) {
                                updates.push((*k, *v));
                            }
                        }
                    }
                    send_buf.clear();
                    let n_updates = updates.len();
                    Message::Updates(updates)
                        .serialize(&mut Serializer::new(&mut send_buf))
                        .unwrap();
                    debug!(
                        "sending {n_updates} updates {} bytes to {peer}",
                        send_buf.len()
                    );
                    socket.send_to(&send_buf, &peer).await?;
                }
            }
            Message::Updates(updates) => {
                debug!("got {} updates {} bytes from {peer}", updates.len(), size);
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
        Message::HashSegments::<u64, u64>(segments)
            .serialize(&mut Serializer::new(&mut send_buf))
            .unwrap();
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

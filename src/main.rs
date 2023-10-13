use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use futures::future::{select, Either};
use rand::{Rng, SeedableRng};
use rmp_serde::{decode, Serializer};
use serde::Serialize;
use tokio::net::UdpSocket;

use reconciliate::diff::{Diffable, HashSegment};
use reconciliate::htree::HTree;

async fn answer_queries(
    socket: Arc<UdpSocket>,
    tree: Arc<RwLock<HTree<u64, u64>>>,
) -> Result<(), std::io::Error> {
    let mut recv_buf = [0; 4096];
    let mut send_buf = Vec::new();
    loop {
        let (size, peer) = socket.recv_from(&mut recv_buf).await?;
        if size == recv_buf.len() {
            println!("Buffer too small for message, discarded");
            continue;
        }
        let segments: Vec<HashSegment<u64>> = decode::from_slice(&recv_buf[..size]).unwrap();
        println!("got {} segments {} bytes from {peer}", segments.len(), size);
        let mut diffs = Vec::new();
        let segments = {
            let guard = tree.read().unwrap();
            guard.diff_round(&mut diffs, segments)
        };
        if !segments.is_empty() {
            send_buf.clear();
            segments
                .serialize(&mut Serializer::new(&mut send_buf))
                .unwrap();
            println!(
                "sending {} segments {} bytes to {peer}",
                segments.len(),
                send_buf.len()
            );
            socket.send_to(&send_buf, &peer).await?;
        }
        if !diffs.is_empty() {
            println!("Found diffs: {diffs:?}");
        }
        if segments.is_empty() {
            break Ok(());
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
        segments
            .serialize(&mut Serializer::new(&mut send_buf))
            .unwrap();
        println!("start_diff {} bytes to {other_addr}", send_buf.len());
        socket.send_to(&send_buf, &other_addr).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[derive(Parser)]
struct Args {
    listen_addr: SocketAddr,
    other_addr: SocketAddr,
    elements: usize,
}

#[tokio::main]
async fn main() {
    let Args {
        listen_addr,
        other_addr,
        elements,
    } = Args::parse();
    let socket = Arc::new(UdpSocket::bind(listen_addr).await.unwrap());
    println!("Listening on: {}", socket.local_addr().unwrap());

    let mut tree = HTree::new();
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    for _ in 0..elements {
        let key: u64 = rng.gen::<u64>();
        let value: u64 = rng.gen();
        tree.insert(key, value);
    }

    let state = Arc::new(RwLock::new(tree));

    let handle_recv = tokio::spawn(answer_queries(Arc::clone(&socket), Arc::clone(&state)));
    let handle_send = tokio::spawn(send_queries(socket, other_addr, state));
    match select(handle_recv, handle_send).await {
        Either::Left((left, _right)) => left.unwrap().unwrap(),
        Either::Right((right, _left)) => right.unwrap().unwrap(),
    }
}

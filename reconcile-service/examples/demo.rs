use std::net::SocketAddr;

use clap::Parser;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use tokio::net::UdpSocket;
use tracing::info;

use diff::HashRangeQueryable;
use reconcilable::rhtree::RHTree;

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

    let socket = UdpSocket::bind(listen_addr).await.unwrap();
    info!("Listening on: {}", socket.local_addr().unwrap());

    // build collection
    let mut rhtree = RHTree::new();
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let time = chrono::offset::Utc::now();
        let value = Alphanumeric.sample_string(&mut rng, 1000);
        rhtree.insert(key, time, value);
    }
    info!("Global hash is {}", rhtree.hash(&..));

    reconcile_service::run(socket, other_addr, rhtree)
        .await
        .unwrap();
}

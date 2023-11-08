use std::net::SocketAddr;

use clap::Parser;
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use tokio::net::UdpSocket;
use tracing::info;

use reconcile::diff::HashRangeQueryable;
use reconcile::htree::HTree;
use reconcile::Service;

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
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..elements {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let time = chrono::offset::Utc::now();
        let value = Alphanumeric.sample_string(&mut rng, 1000);
        key_values.push((key, (time, value)));
    }
    let tree = HTree::from_iter(key_values);
    info!("Global hash is {}", tree.hash(&..));

    let service = Service::new(tree);
    service
        .run(
            socket,
            other_addr,
            |_k, _v, _old_v| (),
            |service| {
                info!(
                    "Updated state; global hash is now {}",
                    service.read().hash(&..)
                )
            },
        )
        .await;
}

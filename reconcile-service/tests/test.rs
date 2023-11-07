use std::net::SocketAddr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};
use tokio::net::UdpSocket;

use diff::HashRangeQueryable;
use htree::HTree;
use reconcile_service::ReconcileService;

#[tokio::test(flavor = "multi_thread")]
async fn test() {
    let addr1: SocketAddr = "127.0.0.42:8080".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.43:8080".parse().unwrap();
    let socket1 = UdpSocket::bind(addr1).await.unwrap();
    let socket2 = UdpSocket::bind(addr2).await.unwrap();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..1000 {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let time = Utc::now();
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        key_values.push((key, (time, value)));
    }
    let tree1 = HTree::from_iter(key_values.into_iter());
    let tree2: HTree<String, (DateTime<Utc>, String)> = HTree::new();

    let service1 = ReconcileService::new(tree1);
    let service2 = ReconcileService::new(tree2);

    let task1 = tokio::spawn(service1.clone().run(socket1, addr2, |_, _, _| {}, |_| {}));
    let task2 = tokio::spawn(service2.clone().run(socket2, addr1, |_, _, _| {}, |_| {}));

    assert_ne!(service1.read().hash(&..), service2.read().hash(&..));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(service1.read().hash(&..), service2.read().hash(&..));

    task2.abort();
    task1.abort();
}

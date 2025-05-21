use std::time::Duration;

use rand::{
    distributions::{Alphanumeric, DistString},
    Rng, SeedableRng,
};

use reconcile::{service::ServiceConfig, Service};

/// Wait for a while until the provided predicate becomes true
///
/// If the predicate become true in the delay, return true, otherwise return false. This functions
/// minimizes the wait time by checking regularly if the predicate is true.
async fn wait_until<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f() {
            return true;
        }
    }
    false
}

macro_rules! assert_until {
    ( $x:expr ) => {
        assert!(wait_until(|| $x).await, stringify!($x))
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn test() {
    let port = 8080;
    let peer_net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.44".parse().unwrap();
    let addr2 = "127.0.0.45".parse().unwrap();
    let cfg1 = ServiceConfig::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_peer_net(peer_net);
    let cfg2 = ServiceConfig::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_peer_net(peer_net);

    // create tree1 with many values
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    // start reconciliation services for tree1 and tree2
    let service1 = Service::new(cfg1).await.with_seed(addr2);
    service1.insert_bulk(&key_values);
    let start_hash = service1.fingerprint(..);
    let service2 = Service::new(cfg2).await.with_seed(addr1);
    let task2 = tokio::spawn(service2.clone().run());
    assert_eq!(service2.fingerprint(..), 0);
    let task1 = tokio::spawn(service1.clone().run());
    assert_eq!(service1.fingerprint(..), start_hash);

    // check that tree2 is filled with the values from tree1
    assert_until!(service2.fingerprint(..) == start_hash);

    // check that tree1 is unchanged
    assert_eq!(service1.fingerprint(..), start_hash);

    // add value to tree2, and check that it is transferred to tree1
    let key = "42".to_string();
    let value = "Hello, World!".to_string();
    service2.insert(key.clone(), value.clone());
    assert_until!(service1.get(&key).as_deref() == Some(&value));

    // remove value from tree1, and check that the tombstone is transferred to tree2
    service1.remove(&key);
    assert_until!(service2.get(&key).is_none());

    // check that the more recent value always wins
    for _ in 0..20 {
        let key = "42".to_string();
        let value1 = "Hello, World!".to_string();
        let value2 = "Good bye, World!".to_string();
        if rng.gen() {
            // value1 vs value2
            service1.insert(key.clone(), value1.clone());
            service2.insert(key.clone(), value2.clone());
            assert_until!(service1.get(&key).as_deref() == Some(&value2));
            assert_until!(service2.get(&key).as_deref() == Some(&value2));
        } else if rng.gen() {
            // value2 vs value1
            service1.insert(key.clone(), value2.clone());
            service2.insert(key.clone(), value1.clone());
            assert_until!(service1.get(&key).as_deref() == Some(&value1));
            assert_until!(service2.get(&key).as_deref() == Some(&value1));
        } else if rng.gen() {
            // value1 vs tombstone
            service1.insert(key.clone(), value1);
            service2.remove(&key);
            assert_until!(service1.get(&key).is_none());
            assert_until!(service2.get(&key).is_none());
        } else {
            // tombstone vs value1
            service1.remove(&key);
            service2.insert(key.clone(), value1.clone());
            assert_until!(service1.get(&key).as_deref() == Some(&value1));
            assert_until!(service2.get(&key).as_deref() == Some(&value1));
        }
    }

    // check that a newer value can overwrite a tombstone
    let key = "43".to_string();
    let value1 = "Hello, World!".to_string();
    let value2 = "Goodbye!".to_string();
    // insert (key, value1) pair
    service1.insert(key.clone(), value1.clone());
    // wait until service2 has received it
    assert_until!(service2.get(&key).as_deref() == Some(&value1));
    // remove the key from service2
    service2.remove(&key);
    // wait until service1 has received the tombstone
    assert_until!(service1.get(&key).is_none());
    // overwrite tombstone by inserting (key, value2)
    service1.insert(key.clone(), value2.clone());
    // check that instance2 receives value2
    assert_until!(service2.get(&key).as_deref() == Some(&value2));

    task2.abort();
    task1.abort();
}

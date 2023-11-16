use std::time::Duration;

use chrono::Utc;
use rand::{
    distributions::{Alphanumeric, DistString},
    Rng, SeedableRng,
};

use reconcile::{DatedMaybeTombstone, HRTree, HashRangeQueryable, Service};

/// Wait for a while until the provided predicate becomes true
///
/// If the predicate become true in the delay, retrurn true, otherwise return false. This functions
/// minimizes the wait time by checking regularly if the predicate is true.
async fn wait_until<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f() {
            return true;
        }
    }
    false
}

macro_rules! assert_until {
    ( $x:expr ) => {
        assert!(wait_until(|| $x).await)
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn test() {
    let port = 8080;
    let peer_net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.44".parse().unwrap();
    let addr2 = "127.0.0.45".parse().unwrap();

    // create tree1 with many values
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut key_values = Vec::new();
    for _ in 0..1000 {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: DatedMaybeTombstone<String> =
            (Utc::now(), Some(Alphanumeric.sample_string(&mut rng, 100)));
        key_values.push((key, value));
    }
    let tree1 = HRTree::from_iter(key_values.into_iter());
    let start_hash = tree1.hash(&..);

    // empty tree2
    let tree2: HRTree<String, DatedMaybeTombstone<String>> = HRTree::new();

    // start reconciliation services for tree1 and tree2
    let service1 = Service::new(tree1, port, addr1, peer_net)
        .await
        .with_seed(addr2);
    let service2 = Service::new(tree2, port, addr2, peer_net)
        .await
        .with_seed(addr1);
    let task2 = tokio::spawn(service2.clone().run());
    assert_eq!(service2.read().hash(&..), 0);
    let task1 = tokio::spawn(service1.clone().run());
    assert_eq!(service1.read().hash(&..), start_hash);

    // check that tree2 is filled with the values from tree1
    assert_until!(service2.read().hash(&..) == start_hash);

    // check that tree1 is unchanged
    assert_eq!(service1.read().hash(&..), start_hash);

    // add value to tree2, and check that it is transferred to tree1
    let key = "42".to_string();
    let value = "Hello, World!".to_string();
    service2.insert(key.clone(), value.clone(), Utc::now());
    assert_until!(service1.read().get(&key).unwrap().1.as_ref() == Some(&value));

    // remove value from tree1, and check that the tombstone is transferred to tree2
    service1.remove(&key, Utc::now());
    assert_until!(service2.read().get(&key).unwrap().1.is_none());

    // check that the more recent value always wins
    for _ in 0..10 {
        // add value to tree2, and check that it is transferred to tree1
        let key = "42".to_string();
        let t1 = Utc::now();
        let value1 = "Hello, World!".to_string();
        let t2 = Utc::now();
        if rng.gen() {
            service1.insert(key.clone(), value1.clone(), t1);
            service2.remove(&key, t2);
            assert_until!(service1.read().get(&key).unwrap().1 == None);
            assert_until!(service2.read().get(&key).unwrap().1 == None);
        } else {
            service1.remove(&key, t1);
            service2.insert(key.clone(), value1.clone(), t2);
            assert_until!(service1.read().get(&key).unwrap().1.as_ref() == Some(&value1));
            assert_until!(service2.read().get(&key).unwrap().1.as_ref() == Some(&value1));
        }
    }

    // check that the following set of changes yields the correct behavior:
    // 1. Add a (key, value1) pair on instance1
    // 2. Check that instance2 has received the value1
    // 3. Remove the key on instance2
    // 4. Check that instance1 has received the tombstone
    // 5. Add the key, value2 pair on instance1
    // 6. Check that instance2 has received the value2
    {
        let key = "43".to_string();

        let t1 = Utc::now();
        let value1 = "Hello, World!".to_string();
        service1.insert(key.clone(), value1.clone(), t1);
        assert_until!(service2.read().get(&key).unwrap().1.as_ref() == Some(&value1));

        let t2 = Utc::now();
        service2.remove(&key, t2);
        assert_until!(service1.read().get(&key).unwrap().1 == None);

        let t3 = Utc::now();
        let value2 = "Goodbye!".to_string();
        service1.insert(key.clone(), value2.clone(), t3);
        assert_until!(service2.read().get(&key).unwrap().1.as_ref() == Some(&value2));
    }

    task2.abort();
    task1.abort();
}

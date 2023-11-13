use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use tokio::net::UdpSocket;

use crate::diff::Diffable;
use crate::map::Map;
use crate::service::Service;

pub type MaybeTombstone<V> = Option<V>;
pub type DatedMaybeTombstone<V> = (DateTime<Utc>, MaybeTombstone<V>);

/// A wrapper to the [`Service`] to provide a remove method.
pub struct RemoveService<M: Map> {
    service: Service<M>,
    tombstones: Arc<RwLock<HashMap<M::Key, DateTime<Utc>>>>,
}

impl<M: Map> RemoveService<M> {
    pub fn new(map: M, socket: UdpSocket) -> Self {
        RemoveService {
            service: Service::new(map, socket),
            tombstones: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_seed(mut self, peer: SocketAddr) -> Self {
        self.service = self.service.with_seed(peer);
        self
    }

    pub fn read(&self) -> RwLockReadGuard<'_, M> {
        self.service.read()
    }
}

impl<M: Map> Clone for RemoveService<M> {
    fn clone(&self) -> Self {
        RemoveService {
            service: self.service.clone(),
            tombstones: self.tombstones.clone(),
        }
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Send + Serialize + Sync + 'static,
        C: Debug + DeserializeOwned + Send + Serialize + Sync + 'static,
        D: Debug,
        R: Map<Key = K, Value = DatedMaybeTombstone<V>, DifferenceItem = D>
            + Diffable<ComparisonItem = C, DifferenceItem = D>,
    > RemoveService<R>
{
    pub fn insert(&self, key: K, value: V, timestamp: DateTime<Utc>) -> Option<V> {
        let mut guard = self.tombstones.write().unwrap();
        guard.remove(&key);

        let ret = self.service.insert(key, (timestamp, Some(value)));
        ret.and_then(|t| t.1)
    }

    pub fn insert_bulk(&self, key_values: &[(K, V, DateTime<Utc>)]) {
        let mut guard = self.tombstones.write().unwrap();
        for (k, _, _) in key_values {
            guard.remove(k);
        }

        self.service.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v, t)| (k.clone(), (*t, Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn remove(&self, key: &K, timestamp: DateTime<Utc>) -> Option<V> {
        let mut guard = self.tombstones.write().unwrap();
        guard.insert(key.clone(), timestamp);

        let ret = self.service.insert(key.clone(), (timestamp, None));
        ret.and_then(|t| t.1)
    }

    pub fn remove_bulk(&self, keys: &[(K, DateTime<Utc>)]) {
        let mut guard = self.tombstones.write().unwrap();
        for (k, t) in keys {
            guard.insert(k.clone(), *t);
        }

        self.service.insert_bulk(
            &keys
                .iter()
                .map(|(k, t)| (k.clone(), (*t, None)))
                .collect::<Vec<_>>(),
        );
    }

    pub async fn run<
        FI: Fn(&K, &(DateTime<Utc>, Option<V>), Option<&(DateTime<Utc>, Option<V>)>),
    >(
        self,
        before_insert: FI,
    ) {
        self.service.run(before_insert).await
    }
}

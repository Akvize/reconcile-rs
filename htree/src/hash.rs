use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub fn hash<K: Hash, V: Hash>(key: &K, value: &V) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

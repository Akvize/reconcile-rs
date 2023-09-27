use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::cmp::Ordering;

struct Node<K, V> {
    hash: u64,
    key: K,
    value: V,
    left: Option<Box<Node<K, V>>>,
    right: Option<Box<Node<K, V>>>,
}

fn hash<K: Hash, V: Hash>(key: &K, value: &V) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

impl<K: Hash, V: Hash> Node<K, V> {
    fn new(key: K, value: V) -> Self {
        Node {
            hash: hash(&key, &value),
            key,
            value,
            left: None,
            right: None,
        }
    }

    fn replace(&mut self, value: V) -> (u64, V) {
        let old_hash = self.hash;
        let new_hash = hash(&self.key, &value);
        self.hash = new_hash;
        let old_value = std::mem::replace(&mut self.value, value);
        (new_hash ^ old_hash, old_value)
    }
}

pub struct HTree<K, V> {
    root: Option<Box<Node<K, V>>>,
}

impl<K: Hash + Ord, V: Hash> HTree<K, V> {
    pub fn new() -> Self {
        HTree {
            root: None,
        }
    }

    pub fn hash(&self) -> u64 {
        match &self.root {
            Some(node) => node.hash,
            None => 0,
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        fn aux<K: Hash + Ord, V: Hash>(anchor: &mut Option<Box<Node<K, V>>>, key: K, value: V) -> (u64, Option<V>) {
            if let Some(node) = anchor {
                match key.cmp(&node.key) {
                    Ordering::Equal => {
                        let (diff_hash, old_node) = node.replace(value);
                        (diff_hash, Some(old_node))
                    },
                    Ordering::Less => {
                        let (diff_hash, old_node) = aux(&mut node.left, key, value);
                        node.hash ^= diff_hash;
                        (diff_hash, old_node)
                    }
                    Ordering::Greater => {
                        let (diff_hash, old_node) = aux(&mut node.right, key, value);
                        node.hash ^= diff_hash;
                        (diff_hash, old_node)
                    }
                }
            } else {
                let node = Box::new(Node::new(key, value));
                let hash = node.hash;
                *anchor = Some(node);
                (hash, None)
            }
        }
        aux(&mut self.root, key, value).1
    }

    pub fn validate(&self) {
        fn aux<K: Hash + Ord, V: Hash>(anchor: &Option<Box<Node<K, V>>>, min: Option<&K>, max: Option<&K>) -> u64 {
            if let Some(node) = anchor {
                if let Some(min) = min {
                    if &node.key < min {
                        panic!("Ordering invariant violated");
                    }
                }
                if let Some(max) = max {
                    if &node.key > max {
                        panic!("Ordering invariant violated");
                    }
                }
                let left_hash = aux(&node.left, min, Some(&node.key));
                let right_hash = aux(&node.right, Some(&node.key), max);
                let local_hash = hash(&node.key, &node.value);
                let expected_hash = left_hash ^ right_hash ^ local_hash;
                if expected_hash != node.hash {
                    panic!("Hashing invariant violated");
                }
                expected_hash
            } else {
                0
            }
        }
        aux(&self.root, None, None);
    }
}

#[test]
fn test() {
    let mut tree = HTree::new();
    tree.validate();
    tree.insert(50, "Hello");
    tree.insert(25, "World!");
    tree.insert(75, "Everyone!");
    tree.validate();

    println!("{}", tree.hash());
}

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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

impl<K, V> Default for HTree<K, V> {
    fn default() -> Self {
        HTree { root: None }
    }
}

impl<K: Hash + Ord, V: Hash> HTree<K, V> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn hash(&self) -> u64 {
        match &self.root {
            Some(node) => node.hash,
            None => 0,
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        fn aux<K: Hash + Ord, V: Hash>(
            anchor: &mut Option<Box<Node<K, V>>>,
            key: K,
            value: V,
        ) -> (u64, Option<V>) {
            if let Some(node) = anchor {
                match key.cmp(&node.key) {
                    Ordering::Equal => {
                        let (diff_hash, old_node) = node.replace(value);
                        (diff_hash, Some(old_node))
                    }
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

    pub fn remove(&mut self, key: &K) -> Option<V> {
        fn leftmost<K, V>(mut node: &mut Box<Node<K, V>>) -> Box<Node<K, V>> {
            while node.left.as_mut().unwrap().left.is_some() {
                node = node.left.as_mut().unwrap();
            }
            let mut ret = node.left.take().unwrap();
            node.left = ret.right.take();
            ret
        }
        fn aux<K: Hash + Ord, V>(
            anchor: &mut Option<Box<Node<K, V>>>,
            key: &K,
        ) -> (u64, Option<V>) {
            if let Some(node) = anchor {
                match key.cmp(&node.key) {
                    Ordering::Equal => {
                        let mut node = anchor.take().unwrap();
                        let ret = (node.hash, Some(node.value));
                        match (node.left.take(), node.right.take()) {
                            (None, None) => (),
                            (None, Some(right)) => *anchor = Some(right),
                            (Some(left), None) => *anchor = Some(left),
                            (Some(left), Some(mut right)) => {
                                if right.left.is_some() {
                                    let mut next_node = leftmost(&mut right);
                                    next_node.left = Some(left);
                                    next_node.right = Some(right);
                                    *anchor = Some(next_node);
                                } else {
                                    right.left = Some(left);
                                    *anchor = Some(right);
                                }
                            }
                        };
                        ret
                    }
                    Ordering::Less => {
                        let (diff_hash, old_node) = aux(&mut node.left, key);
                        node.hash ^= diff_hash;
                        (diff_hash, old_node)
                    }
                    Ordering::Greater => {
                        let (diff_hash, old_node) = aux(&mut node.right, key);
                        node.hash ^= diff_hash;
                        (diff_hash, old_node)
                    }
                }
            } else {
                (0, None)
            }
        }
        aux(&mut self.root, key).1
    }

    pub fn validate(&self) {
        fn aux<K: Hash + Ord, V: Hash>(
            anchor: &Option<Box<Node<K, V>>>,
            min: Option<&K>,
            max: Option<&K>,
        ) -> u64 {
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
fn test_simple() {
    // empty
    let mut tree = HTree::new();
    assert_eq!(tree.hash(), 0);
    tree.validate();

    // 1 value
    tree.insert(50, "Hello");
    tree.validate();
    let hash1 = tree.hash();
    assert_ne!(hash1, 0);

    // 2 values
    tree.insert(25, "World!");
    tree.validate();
    let hash2 = tree.hash();
    assert_ne!(hash2, 0);
    assert_ne!(hash2, hash1);

    // 3 values
    tree.insert(75, "Everyone!");
    tree.validate();
    let hash3 = tree.hash();
    assert_ne!(hash3, 0);
    assert_ne!(hash3, hash1);
    assert_ne!(hash3, hash2);

    // back to 2 values
    tree.remove(&75);
    tree.validate();
    let hash4 = tree.hash();
    assert_eq!(hash4, hash2);
}

#[test]
fn test_compare() {
    let mut tree1 = HTree::new();
    for (key, value) in [(25, "World!"), (50, "Hello"), (75, "Everyone!")] {
        tree1.insert(key, value);
    }

    let mut tree2 = HTree::new();
    for (key, value) in [(75, "Everyone!"), (50, "Hello"), (25, "World!")] {
        tree2.insert(key, value);
    }

    let mut tree3 = HTree::new();
    for (key, value) in [(75, "Everyone!"), (25, "World!"), (50, "Hello")] {
        tree3.insert(key, value);
    }

    assert_eq!(tree1.hash(), tree2.hash());
    assert_eq!(tree1.hash(), tree3.hash());
}

#[cfg(test)]
mod tests {
    use rand::{seq::SliceRandom, Rng, SeedableRng};

    #[test]
    fn big_test() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut tree = super::HTree::new();
        let mut key_values = Vec::new();

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen();
            let value: u64 = rng.gen();
            tree.insert(key, value);
            tree.validate();
            key_values.push((key, value));
        }

        // remove some
        key_values.shuffle(&mut rng);
        for _ in 0..100 {
            let (key, value) = key_values.pop().unwrap();
            let value2 = tree.remove(&key);
            tree.validate();
            assert_eq!(value2, Some(value));
        }
    }
}

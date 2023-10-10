use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::{Bound, RangeBounds};

#[derive(Debug)]
pub struct Node<K, V> {
    key: K,
    value: V,
    self_hash: u64,
    left: Option<Box<Node<K, V>>>,
    right: Option<Box<Node<K, V>>>,
    tree_hash: u64,
    tree_size: usize,
}

fn hash<K: Hash, V: Hash>(key: &K, value: &V) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

impl<K: Hash, V: Hash> Node<K, V> {
    fn new(key: K, value: V) -> Self {
        let hash = hash(&key, &value);
        Node {
            key,
            value,
            self_hash: hash,
            left: None,
            right: None,
            tree_hash: hash,
            tree_size: 1,
        }
    }

    fn replace(&mut self, value: V) -> (u64, V) {
        let old_hash = self.self_hash;
        let new_hash = hash(&self.key, &value);
        self.self_hash = new_hash;
        let old_value = std::mem::replace(&mut self.value, value);
        let diff_hash = new_hash ^ old_hash;
        self.tree_hash ^= diff_hash;
        (diff_hash, old_value)
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn len(&self) -> usize {
        self.tree_size
    }

    pub fn at(&self, mut index: usize) -> &Node<K, V> {
        if index >= self.tree_size {
            panic!(
                "index out of bounds: the len is {} but the index is {index}",
                self.tree_size
            );
        }
        if let Some(left) = self.left.as_ref() {
            if index < left.tree_size {
                return left.at(index);
            } else {
                index -= left.tree_size;
            }
        }
        if index == 0 {
            return self;
        }
        self.right.as_ref().unwrap().at(index - 1)
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

    pub fn position(&self, key: &K) -> Option<usize> {
        fn aux<K: Hash + Ord, V: Hash>(node: &Node<K, V>, key: &K) -> Option<usize> {
            match key.cmp(&node.key) {
                Ordering::Equal => node.left.as_ref().map(|left| left.tree_size).or(Some(0)),
                Ordering::Less => node.left.as_ref().and_then(|left| aux(left, key)),
                Ordering::Greater => node.right.as_ref().and_then(|right| {
                    aux(right, key).map(|index| node.tree_size - right.tree_size + index)
                }),
            }
        }
        self.root.as_ref().and_then(|node| aux(node, key))
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
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
                        node.tree_hash ^= diff_hash;
                        if old_node.is_none() {
                            node.tree_size += 1;
                        }
                        (diff_hash, old_node)
                    }
                    Ordering::Greater => {
                        let (diff_hash, old_node) = aux(&mut node.right, key, value);
                        node.tree_hash ^= diff_hash;
                        if old_node.is_none() {
                            node.tree_size += 1;
                        }
                        (diff_hash, old_node)
                    }
                }
            } else {
                let node = Box::new(Node::new(key, value));
                let hash = node.self_hash;
                *anchor = Some(node);
                (hash, None)
            }
        }
        aux(&mut self.root, key, value).1
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        fn take_leftmost<K, V>(node: &mut Box<Node<K, V>>) -> Box<Node<K, V>> {
            if node.left.as_mut().unwrap().left.is_some() {
                let ret = take_leftmost(node.left.as_mut().unwrap());
                node.tree_hash ^= ret.self_hash;
                node.tree_size -= 1;
                ret
            } else {
                let mut leftmost = node.left.take().unwrap();
                node.tree_hash ^= leftmost.self_hash;
                node.tree_size -= 1;
                if let Some(right) = leftmost.right.take() {
                    leftmost.tree_hash ^= right.tree_hash;
                    leftmost.tree_size -= right.tree_size;
                    node.left = Some(right);
                }
                assert_eq!(leftmost.tree_size, 1);
                assert_eq!(leftmost.self_hash, leftmost.tree_hash);
                leftmost
            }
        }
        fn aux<K: Hash + Ord, V>(
            anchor: &mut Option<Box<Node<K, V>>>,
            key: &K,
        ) -> (u64, Option<V>) {
            if let Some(node) = anchor {
                match key.cmp(&node.key) {
                    Ordering::Equal => {
                        let mut node = anchor.take().unwrap();
                        let ret = (node.self_hash, Some(node.value));
                        match (node.left.take(), node.right.take()) {
                            (None, None) => (),
                            (None, Some(right)) => *anchor = Some(right),
                            (Some(left), None) => *anchor = Some(left),
                            (Some(left), Some(mut right)) => {
                                if right.left.is_some() {
                                    let mut next_node = take_leftmost(&mut right);
                                    next_node.tree_hash ^= left.tree_hash ^ right.tree_hash;
                                    next_node.tree_size += left.tree_size + right.tree_size;
                                    next_node.left = Some(left);
                                    next_node.right = Some(right);
                                    *anchor = Some(next_node);
                                } else {
                                    right.tree_hash ^= left.tree_hash;
                                    right.tree_size += left.tree_size;
                                    right.left = Some(left);
                                    *anchor = Some(right);
                                }
                            }
                        };
                        ret
                    }
                    Ordering::Less => {
                        let (diff_hash, old_node) = aux(&mut node.left, key);
                        node.tree_hash ^= diff_hash;
                        if old_node.is_some() {
                            node.tree_size -= 1;
                        }
                        (diff_hash, old_node)
                    }
                    Ordering::Greater => {
                        let (diff_hash, old_node) = aux(&mut node.right, key);
                        node.tree_hash ^= diff_hash;
                        if old_node.is_some() {
                            node.tree_size -= 1;
                        }
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
        ) -> (u64, usize) {
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
                let self_hash = hash(&node.key, &node.value);
                if self_hash != node.self_hash {
                    panic!("Self hashing invariant violated");
                }
                let (left_hash, left_size) = aux(&node.left, min, Some(&node.key));
                let (right_hash, right_size) = aux(&node.right, Some(&node.key), max);
                let tree_hash = left_hash ^ right_hash ^ self_hash;
                if tree_hash != node.tree_hash {
                    panic!("Tree hashing invariant violated");
                }
                let tree_size = left_size + right_size + 1;
                if tree_size != node.tree_size {
                    panic!("Tree size invariant violated");
                }
                (tree_hash, tree_size)
            } else {
                (0, 0)
            }
        }
        aux(&self.root, None, None);
    }
}

impl<K, V> PartialEq for HTree<K, V> {
    fn eq(&self, other: &Self) -> bool {
        match (self.root.as_ref(), other.root.as_ref()) {
            (Some(self_), Some(other)) => self_.tree_hash == other.tree_hash,
            (None, None) => true,
            _ => false,
        }
    }
}

impl<K, V> Eq for HTree<K, V> {}

impl<K: Hash + Ord, V: Hash> FromIterator<(K, V)> for HTree<K, V> {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (K, V)>,
    {
        let mut tree = HTree::new();
        for (k, v) in iter {
            tree.insert(k, v);
        }
        tree
    }
}

pub struct IntoIter<K, V> {
    stack: Vec<Box<Node<K, V>>>,
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(mut node) = self.stack.pop() {
            if let Some(left) = node.left.take() {
                self.stack.push(node);
                self.stack.push(left);
                self.next()
            } else {
                if let Some(right) = node.right.take() {
                    self.stack.push(right);
                }
                Some((node.key, node.value))
            }
        } else {
            None
        }
    }
}

impl<K, V> IntoIterator for HTree<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            stack: self.root.into_iter().collect(),
        }
    }
}

pub struct Iter<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, bool)>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, left_explored)) = self.stack.pop() {
            if !left_explored {
                if let Some(left) = node.left.as_ref() {
                    self.stack.push((node, true));
                    self.stack.push((left, false));
                    return self.next();
                }
            }
            if let Some(right) = node.right.as_ref() {
                self.stack.push((right, false));
            }
            Some((&node.key, &node.value))
        } else {
            None
        }
    }
}

impl<'a, K, V> IntoIterator for &'a HTree<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        Iter {
            stack: self
                .root
                .iter()
                .map(|node| (node.as_ref(), false))
                .collect(),
        }
    }
}

impl<K, V> HTree<K, V> {
    pub fn iter(&self) -> Iter<K, V> {
        self.into_iter()
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for HTree<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

trait HashRangeQueryable {
    type Key;
    fn hash<R: RangeBounds<Self::Key>>(&self, range: R) -> u64;
    fn insertion_position(&self, key: &Self::Key) -> usize;
    fn key_at(&self, index: usize) -> &Self::Key;
    fn len(&self) -> usize;
}

impl<K: Hash + Ord, V: Hash> HashRangeQueryable for HTree<K, V> {
    type Key = K;
    fn hash<R: RangeBounds<K>>(&self, range: R) -> u64 {
        fn aux<K: Ord, V, R: RangeBounds<K>>(
            node: &Option<Box<Node<K, V>>>,
            range: &R,
            subtree_lower_bound: Option<&K>,
            subtree_upper_bound: Option<&K>,
        ) -> u64 {
            if let Some(node) = node {
                // check if the lower-bound is included in the range
                let lower_bound_included = match range.start_bound() {
                    Bound::Unbounded => true,
                    Bound::Included(key) | Bound::Excluded(key) => {
                        if let Some(subtree_lower_bound) = subtree_lower_bound {
                            key < subtree_lower_bound
                        } else {
                            false
                        }
                    }
                };
                // check if the upper-bound is included in the range
                let upper_bound_included = match range.end_bound() {
                    Bound::Unbounded => true,
                    Bound::Included(key) | Bound::Excluded(key) => {
                        if let Some(subtree_upper_bound) = subtree_upper_bound {
                            key > subtree_upper_bound
                        } else {
                            false
                        }
                    }
                };
                // if both lower and upper bounds are included in the range, just use the tree hash invariant
                if lower_bound_included && upper_bound_included {
                    return node.tree_hash;
                }
                // otherwise, recurse in the relevant sub-trees

                let mut ret = 0;
                // check if the left sub-tree is partially covered by the range
                if match range.start_bound() {
                    Bound::Unbounded => true,
                    Bound::Included(key) | Bound::Excluded(key) => key < &node.key,
                } {
                    // recurse left
                    ret ^= aux(&node.left, range, subtree_lower_bound, Some(&node.key));
                }
                // check if the node itself is included in the range
                if range.contains(&node.key) {
                    ret ^= node.self_hash;
                }
                // check if the right sub-tree is partially covered by the range
                if match range.end_bound() {
                    Bound::Unbounded => true,
                    Bound::Included(key) | Bound::Excluded(key) => key > &node.key,
                } {
                    // recurse right
                    ret ^= aux(&node.right, range, Some(&node.key), subtree_upper_bound);
                }
                ret
            } else {
                0
            }
        }
        aux(&self.root, &range, None, None)
    }

    fn insertion_position(&self, key: &K) -> usize {
        fn aux<K: Hash + Ord, V: Hash>(node: &Node<K, V>, key: &K) -> usize {
            match key.cmp(&node.key) {
                Ordering::Equal => node.left.as_ref().map(|left| left.tree_size).unwrap_or(0),
                Ordering::Less => node.left.as_ref().map(|left| aux(left, key)).unwrap_or(0),
                Ordering::Greater => node
                    .right
                    .as_ref()
                    .map(|right| node.tree_size - right.tree_size + aux(right, key))
                    .unwrap_or(node.tree_size),
            }
        }
        self.root.as_ref().map(|node| aux(node, key)).unwrap_or(0)
    }

    fn key_at(&self, index: usize) -> &K {
        &self.root.as_ref().unwrap().at(index).key
    }

    fn len(&self) -> usize {
        self.root
            .as_ref()
            .map(|node| node.tree_size)
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Diff<K> {
    InSelf((Bound<K>, Bound<K>)),
    InOther((Bound<K>, Bound<K>)),
    InBoth((Bound<K>, Bound<K>)),
}

trait Diffable {
    type Key;
    fn diff(&self, other: &Self) -> Vec<Diff<Self::Key>>;
}

impl<K: Clone, T: HashRangeQueryable<Key = K>> Diffable for T {
    type Key = K;
    fn diff(&self, other: &T) -> Vec<Diff<K>> {
        fn aux<'a, K: Clone, T: HashRangeQueryable<Key = K>>(
            self_: &'a T,
            other: &'a T,
            range: (Bound<&'a K>, Bound<&'a K>),
            output: &mut Vec<Diff<K>>,
        ) {
            match (self_.hash(range), other.hash(range)) {
                (a, b) if a == b => return,
                (_, 0) => {
                    output.push(Diff::InSelf((range.0.cloned(), range.1.cloned())));
                    return;
                }
                (0, _) => {
                    output.push(Diff::InOther((range.0.cloned(), range.1.cloned())));
                    return;
                }
                (_, _) => (),
            }
            let (start_bound, end_bound) = range;
            let self_start_index = match start_bound {
                Bound::Unbounded => 0,
                Bound::Included(key) => self_.insertion_position(key),
                Bound::Excluded(_) => unreachable!(),
            };
            let self_end_index = match end_bound {
                Bound::Unbounded => self_.len(),
                Bound::Included(_) => unreachable!(),
                Bound::Excluded(key) => self_.insertion_position(key),
            };
            let other_start_index = match start_bound {
                Bound::Unbounded => 0,
                Bound::Included(key) => other.insertion_position(key),
                Bound::Excluded(_) => unreachable!(),
            };
            let other_end_index = match end_bound {
                Bound::Unbounded => other.len(),
                Bound::Included(_) => unreachable!(),
                Bound::Excluded(key) => other.insertion_position(key),
            };
            let self_count = self_end_index - self_start_index;
            let other_count = other_end_index - other_start_index;
            match (self_count, other_count) {
                (0, 0) => unreachable!(),          // both hashes would be 0, and thus equal
                (0, _) | (_, 0) => unreachable!(), // detected above since hashes equal to 0
                (1, 1) => {
                    // either the same key with different
                    // values, or two different keys; in
                    // any cases, the range should be
                    // exchanged
                    output.push(Diff::InBoth((range.0.cloned(), range.1.cloned())));
                }
                (a, _) => {
                    let mid_key = if a == 1 {
                        // recurse w.r.t. other
                        other.key_at(other_start_index + (other_end_index - other_start_index) / 2)
                    } else {
                        // recurse w.r.t. self
                        self_.key_at(self_start_index + (self_end_index - self_start_index) / 2)
                    };
                    // recurse left
                    let left_range = (start_bound, Bound::Excluded(mid_key));
                    aux(self_, other, left_range, output);
                    // recurse right
                    let right_range = (Bound::Included(mid_key), end_bound);
                    aux(self_, other, right_range, output);
                }
            }
        }
        let mut ret = Vec::new();
        aux(self, other, (Bound::Unbounded, Bound::Unbounded), &mut ret);
        ret
    }
}

pub struct ItemRange<'a, K, V, R: RangeBounds<K>> {
    range: R,
    stack: Vec<(&'a Node<K, V>, bool)>,
}

enum RangeOrdering {
    Less,
    Inside,
    Greater,
}

fn range_compare<T: Ord, R: RangeBounds<T>>(item: &T, range: &R) -> RangeOrdering {
    if match range.start_bound() {
        Bound::Included(key) => item < key,
        Bound::Excluded(key) => item <= key,
        _ => false,
    } {
        return RangeOrdering::Less;
    }
    if match range.end_bound() {
        Bound::Included(key) => item > key,
        Bound::Excluded(key) => item >= key,
        _ => false,
    } {
        return RangeOrdering::Greater;
    }
    RangeOrdering::Inside
}

impl<'a, K: Ord, V, R: RangeBounds<K>> Iterator for ItemRange<'a, K, V, R> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, left_explored)) = self.stack.pop() {
            if !self.range.contains(&node.key) {
                self.stack.clear();
                return None;
            }
            if !left_explored {
                if let Some(left) = node.left.as_ref() {
                    self.stack.push((node, true));
                    self.stack.push((left, false));
                    return self.next();
                }
            }
            if let Some(right) = node.right.as_ref() {
                self.stack.push((right, false));
            }
            Some((&node.key, &node.value))
        } else {
            None
        }
    }
}

impl<K: Ord, V> HTree<K, V> {
    pub fn get_range<R: RangeBounds<K>>(&self, range: R) -> ItemRange<K, V, R> {
        let mut stack = Vec::new();
        let mut maybe_node = self.root.as_ref();
        while let Some(node) = maybe_node {
            match range_compare(&node.key, &range) {
                RangeOrdering::Less => maybe_node = node.right.as_ref(),
                RangeOrdering::Greater => maybe_node = node.left.as_ref(),
                RangeOrdering::Inside => {
                    stack.push((node.as_ref(), true));
                    maybe_node = node.left.as_ref();
                }
            }
        }
        ItemRange { range, stack }
    }
}

pub fn reconciliate<K, V>(first: &mut HTree<K, V>, second: &mut HTree<K, V>)
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    for diff in first.diff(second) {
        match diff {
            Diff::InSelf(range) => {
                for (k, v) in first.get_range(range) {
                    second.insert(k.clone(), v.clone());
                }
            }
            Diff::InOther(range) => {
                for (k, v) in second.get_range(range) {
                    first.insert(k.clone(), v.clone());
                }
            }
            Diff::InBoth(_range) => unimplemented!(),
        }
    }
}

#[test]
fn test_simple() {
    // empty
    let mut tree = HTree::new();
    assert_eq!(tree.hash(..), 0);
    tree.validate();

    // 1 value
    tree.insert(50, "Hello");
    tree.validate();
    let hash1 = tree.hash(..);
    assert_ne!(hash1, 0);

    // 2 values
    tree.insert(25, "World!");
    tree.validate();
    let hash2 = tree.hash(..);
    assert_ne!(hash2, 0);
    assert_ne!(hash2, hash1);

    // 3 values
    tree.insert(75, "Everyone!");
    tree.validate();
    let hash3 = tree.hash(..);
    assert_ne!(hash3, 0);
    assert_ne!(hash3, hash1);
    assert_ne!(hash3, hash2);

    // back to 2 values
    tree.remove(&75);
    tree.validate();
    let hash4 = tree.hash(..);
    assert_eq!(hash4, hash2);
}

#[test]
fn test_compare() {
    let tree1 = HTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Everyone!")]);
    let tree2 = HTree::from_iter([(75, "Everyone!"), (50, "Hello"), (25, "World!")]);
    let tree3 = HTree::from_iter([(75, "Everyone!"), (25, "World!"), (50, "Hello")]);
    let tree4 = HTree::from_iter([(75, "Everyone!"), (25, "World!"), (40, "Hello")]);
    let tree5 = HTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Goodbye!")]);

    assert_eq!(tree1.hash(..), tree1.hash(..));
    assert_eq!(tree1.hash(..), tree2.hash(..));
    assert_eq!(tree1.hash(..), tree3.hash(..));
    assert_ne!(tree1.hash(..), tree4.hash(..));
    assert_ne!(tree1.hash(..), tree5.hash(..));

    assert_eq!(tree1, tree1);
    assert_eq!(tree1, tree2);
    assert_eq!(tree1, tree3);
    assert_ne!(tree1, tree4);
    assert_ne!(tree1, tree5);

    assert_eq!(tree1.diff(&tree1), vec![]);
    assert_eq!(tree1.diff(&tree2), vec![]);
    assert_eq!(tree1.diff(&tree3), vec![]);
    assert_eq!(
        tree1.diff(&tree4),
        vec![
            Diff::InOther((Bound::Included(40), Bound::Excluded(50))),
            Diff::InSelf((Bound::Included(50), Bound::Excluded(75)))
        ]
    );
    assert_eq!(
        tree1.diff(&tree5),
        vec![Diff::InBoth((Bound::Included(75), Bound::Unbounded))]
    );

    let range = tree1.get_range((Bound::Included(40), Bound::Excluded(50)));
    assert_eq!(range.collect::<Vec<_>>(), vec![]);
    let range = tree1.get_range((Bound::Included(50), Bound::Excluded(75)));
    assert_eq!(range.collect::<Vec<_>>(), vec![(&50, &"Hello")]);
    let range = tree4.get_range((Bound::Included(40), Bound::Excluded(50)));
    assert_eq!(range.collect::<Vec<_>>(), vec![(&40, &"Hello")]);
    let range = tree4.get_range((Bound::Included(50), Bound::Excluded(75)));
    assert_eq!(range.collect::<Vec<_>>(), vec![]);

    let mut tree1 = tree1;
    let mut tree4 = tree4;
    reconciliate(&mut tree1, &mut tree4);
    assert_eq!(tree1, tree4);
    assert_eq!(
        tree1.get_range(..).collect::<Vec<_>>(),
        [
            (&25, &"World!"),
            (&40, &"Hello"),
            (&50, &"Hello"),
            (&75, &"Everyone!")
        ]
    )
}

#[cfg(test)]
mod tests {
    use rand::{seq::SliceRandom, Rng, SeedableRng};

    use super::HashRangeQueryable;

    #[test]
    fn big_test() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut tree = super::HTree::new();
        let mut key_values = Vec::new();

        let mut expected_hash = 0;

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen();
            let value: u64 = rng.gen();
            tree.insert(key, value);
            tree.validate();
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(tree.hash(..), expected_hash);
            key_values.push((key, value));
        }

        // check for partial ranges
        let mid = u64::MAX / 2;
        assert_ne!(tree.hash(mid..), tree.hash(..));
        assert_ne!(tree.hash(..mid), tree.hash(..));
        assert_eq!(tree.hash(..mid) ^ tree.hash(mid..), tree.hash(..));

        // check key_at() with first and last indexes
        assert_eq!(
            key_values.iter().map(|(key, _)| key).min(),
            Some(tree.key_at(0))
        );
        assert_eq!(
            key_values.iter().map(|(key, _)| key).max(),
            Some(tree.key_at(tree.len() - 1))
        );

        // check for at/position consistency
        let key = key_values[0].0;
        let index = tree.position(&key).unwrap();
        assert_ne!(index, 0);
        assert_eq!(tree.root.as_ref().unwrap().at(index).key, key);

        // test insertion_position
        assert_eq!(tree.insertion_position(&key), tree.position(&key).unwrap());
        assert_eq!(tree.insertion_position(&0), 0);
        assert_eq!(tree.insertion_position(&u64::MAX), tree.len());

        let items: Vec<(u64, u64)> = tree.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(items.len(), key_values.len());
        key_values.sort();
        assert_eq!(items, key_values);

        // remove some
        key_values.shuffle(&mut rng);
        for _ in 0..1000 {
            let (key, value) = key_values.pop().unwrap();
            let value2 = tree.remove(&key);
            tree.validate();
            assert_eq!(value2, Some(value));
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(tree.hash(..), expected_hash);
        }
    }
}

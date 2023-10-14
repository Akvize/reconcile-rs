use std::cmp::Ordering;
use std::hash::Hash;
use std::ops::{Bound, Neg, RangeBounds};

use crate::diff::{Diffable, HashRangeQueryable};
use crate::hash::hash;
use crate::range_compare::{range_compare, RangeOrdering};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Direction {
    Left = 0,
    Right = 1,
    None,
}

impl Neg for Direction {
    type Output = Self;
    fn neg(self) -> Self::Output {
        match self {
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
            Direction::None => Direction::None,
        }
    }
}

#[derive(Debug)]
struct Node<K, V> {
    key: K,
    value: V,
    self_hash: u64,
    children: [Option<Box<Node<K, V>>>; 2],
    taller_subtree: Direction,
    tree_hash: u64,
    tree_size: usize,
}

impl<K: Hash, V: Hash> Node<K, V> {
    fn new(key: K, value: V) -> Self {
        let hash = hash(&key, &value);
        Node {
            key,
            value,
            self_hash: hash,
            children: [None, None],
            taller_subtree: Direction::None,
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
}

impl<K: Ord, V> Node<K, V> {
    fn dir(&self, key: &K) -> Direction {
        match key.cmp(&self.key) {
            Ordering::Less => Direction::Left,
            Ordering::Greater => Direction::Right,
            Ordering::Equal => Direction::None,
        }
    }
}

/// Rotate the sub-tree rooted at `anchor`.
///
/// This will move the node at `anchor` downwards, in the provided direction `dir`. The child in
/// the opposite direction will be moved at `anchor`. The parent originally at `anchor` will then
/// be moved itself as a child of this new root. Room for doing this is done by moving the
/// corresponding sub-tree rooted at the child to the parent, where the child has been removed.
///
/// Seriously, just look at the diagram at Wikipedia, it will make more sense:
/// https://en.wikipedia.org/wiki/AVL_tree#Simple_rotation
fn rotate<K, V>(anchor: &mut Option<Box<Node<K, V>>>, dir: Direction) {
    assert_ne!(dir, Direction::None);
    // remove the node from anchor
    let mut node = anchor.take().unwrap();
    // remove the child to be used as the new root from its parent
    let mut child = node.children[-dir as usize].take().unwrap();
    node.tree_hash ^= child.tree_hash;
    node.tree_size -= child.tree_size;
    // move one sub-tree of the new root to the freed side of the parent
    if let Some(grandchild) = child.children[dir as usize].take() {
        child.tree_hash ^= grandchild.tree_hash;
        child.tree_size -= grandchild.tree_size;
        node.tree_hash ^= grandchild.tree_hash;
        node.tree_size += grandchild.tree_size;
        node.children[-dir as usize] = Some(grandchild);
    }
    // re-root the node at the child
    child.tree_hash ^= node.tree_hash;
    child.tree_size += node.tree_size;
    child.children[dir as usize] = Some(node);
    // set the new-root
    *anchor = Some(child);
}

/// Restore the AVL tree invariant at `anchor`, that says that the different in height of the left
/// and right part of the sub-tree rooted at `anchor` is at most one.
///
/// This needs to be call after a node has been inserted or removed from the tree and the height of
/// the sub-tree rooted at `anchor` has changed. For an insertion, it returns `true` when the
/// height of the sub-tree has increased; for a deletion, it returns `false` when the height has
/// decreased.
///
/// `dir` indicates either the direction where a node has been inserted, or the direction opposite to
/// where a node has been removed
fn rebalance<K, V>(anchor: &mut Option<Box<Node<K, V>>>, dir: Direction) -> bool {
    let node = anchor.as_mut().unwrap();
    if node.taller_subtree == Direction::None {
        // Case 1: the node was balanced
        // the node becomes unbalanced
        node.taller_subtree = dir;
        // height of the sub-tree rooted at node is increased (insertion) / unchanged (deletion)
        true
    } else if node.taller_subtree != dir {
        // Case 2: the node was unbalanced in the other direction
        // the node becomes balanced
        node.taller_subtree = Direction::None;
        // height of the sub-tree rooted at node is unchanged (insertion) / decreased (deletion)
        false
    } else {
        // Case 3: the node was already unbalanced in the same direction, need to rebalance
        // NOTE: since the sub-tree is unbalanced in this redirection, there is always a child here
        let child = node.children[dir as usize].as_mut().unwrap();
        // The basic goal is to bring the child up one node, by rotating in the place of its
        // parent. The other parent's sub-tree, and the child's two sub-trees must then be
        // re-rooted appropriately
        if child.taller_subtree == Direction::None {
            // Sub-case 1: the child is currently balanced
            // NOTE: this does not happen during insertion
            // In this case, simply rotating child in place of its parent. One of the sub-trees of
            // the child will be re-rooted at the parent; with the rotation, the resulting sub-tree
            // of the child will see its height increased by one. The other sub-tree of the child
            // will stay rooted at the child, and the height of the resulting sub-tree will thus
            // be reduced by one. After the deletion and rebalancing, the unbalance is thus inverted.
            //
            // the unbalance is inverted
            child.taller_subtree = -dir;
            // the parent is moved down once, and one of the sub-trees of the child is re-rooted
            // here; that new sub-tree is taller that stays rooted at the parent, so it causes
            // unbalance
            node.taller_subtree = dir;
            // perform the actual rotation
            rotate(anchor, -dir);
            // the height decreases (deletion) after the rebalancing
            true
        } else if child.taller_subtree == dir {
            // Sub-case 2: the child is currently balanced in the same direction as the parent
            // By keeping the longer sub-tree with the child, and re-rooting the shorter sub-tree
            // of the child to the parent after the rotation, the total height will be decreased by
            // one, and both nodes become balanced
            child.taller_subtree = Direction::None;
            node.taller_subtree = Direction::None;
            rotate(anchor, -dir);
            // the change in height is absorbed by the rebalancing (insertion) / not (deletion)
            false
        } else {
            // Sub-case 3: the child is currently balanced in the opposite direction as the parent
            // Simply moving the sub-trees as in the previous sub-cases will not be enough. Indeed,
            // the taller sub-tree of the child would be re-rooted at the parent, with the parent
            // at the same depth as the original position of the child, the total height is not
            // changed. Thus, we need to look at the grand-child on the taller side to perform two
            // rotations, one to rebalance the child, one to rebalance the parent.
            //
            // We still need to look at the grandchild to know the resulting unbalances of parent
            // and child after rebalancing
            let grandchild = child.children[-dir as usize].as_mut().unwrap();
            if grandchild.taller_subtree == Direction::None {
                // Sub-sub-case 1: the grand-child is balanced
                // NOTE: just like sub-case 1, this does not happen during insertion
                grandchild.taller_subtree = Direction::None;
                child.taller_subtree = Direction::None;
                node.taller_subtree = Direction::None;
            } else if grandchild.taller_subtree == dir {
                // Sub-sub-case 2: the grand-child is unbalanced in the same direction as the
                // parent is, and in the opposite direction to the child
                grandchild.taller_subtree = Direction::None;
                child.taller_subtree = Direction::None;
                node.taller_subtree = -dir;
            } else {
                // Sub-sub-case 3: the grand-child is unbalanced in the opposite direction to the
                // parent, and in the same direction as the child is
                grandchild.taller_subtree = Direction::None;
                child.taller_subtree = dir;
                node.taller_subtree = Direction::None;
            }
            // rebalance the child (by replacing it with grandchild)
            rotate(&mut node.children[dir as usize], dir);
            // rebalance the parent (by replacing it with grandchild in its turn)
            rotate(anchor, -dir);
            // the change in height is absorbed by the rebalancing (insertion) / not (deletion)
            false
        }
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

    fn node_at_key<'a>(&'a self, key: &'a K) -> Option<&'a Node<K, V>> {
        fn aux<'a, K: Hash + Ord, V: Hash>(
            node: &'a Node<K, V>,
            key: &'a K,
        ) -> Option<&'a Node<K, V>> {
            match node.dir(key) {
                Direction::None => Some(node),
                Direction::Left => node.children[0].as_ref().and_then(|left| aux(left, key)),
                Direction::Right => node.children[1].as_ref().and_then(|right| aux(right, key)),
            }
        }
        self.root.as_ref().and_then(|node| aux(node, key))
    }

    fn node_at_index(&self, mut index: usize) -> &Node<K, V> {
        if index >= self.len() {
            panic!(
                "index out of bounds: the len is {} but the index is {index}",
                self.len()
            );
        }
        let mut maybe_node = self.root.as_ref();
        while let Some(node) = maybe_node {
            if let Some(left) = node.children[0].as_ref() {
                if index < left.tree_size {
                    maybe_node = Some(left);
                    continue;
                } else {
                    index -= left.tree_size;
                }
            }
            if index == 0 {
                return node;
            }
            index -= 1;
            maybe_node = node.children[1].as_ref();
        }
        unreachable!();
    }

    pub fn get<'a>(&'a self, key: &'a K) -> Option<&'a V> {
        self.node_at_key(key).map(|node| &node.value)
    }

    pub fn position(&self, key: &K) -> Option<usize> {
        fn aux<K: Hash + Ord, V: Hash>(node: &Node<K, V>, key: &K) -> Option<usize> {
            match node.dir(key) {
                Direction::None => node.children[0]
                    .as_ref()
                    .map(|left| left.tree_size)
                    .or(Some(0)),
                Direction::Left => node.children[0].as_ref().and_then(|left| aux(left, key)),
                Direction::Right => node.children[1].as_ref().and_then(|right| {
                    aux(right, key).map(|index| node.tree_size - right.tree_size + index)
                }),
            }
        }
        self.root.as_ref().and_then(|node| aux(node, key))
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        // return:
        // - hash difference
        // - the node that was at key, if any
        // - whether the height has increased in the sub-tree
        fn aux<K: Hash + Ord, V: Hash>(
            anchor: &mut Option<Box<Node<K, V>>>,
            key: K,
            value: V,
        ) -> (u64, Option<V>, bool) {
            if let Some(node) = anchor {
                match node.dir(&key) {
                    Direction::None => {
                        let (diff_hash, old_node) = node.replace(value);
                        (diff_hash, Some(old_node), false)
                    }
                    dir => {
                        let (diff_hash, old_node, height_increased) =
                            aux(&mut node.children[dir as usize], key, value);
                        node.tree_hash ^= diff_hash;
                        if old_node.is_none() {
                            node.tree_size += 1;
                        }
                        let height_increased = height_increased && rebalance(anchor, dir);
                        (diff_hash, old_node, height_increased)
                    }
                }
            } else {
                let node = Box::new(Node::new(key, value));
                let hash = node.self_hash;
                *anchor = Some(node);
                (hash, None, true)
            }
        }
        aux(&mut self.root, key, value).1
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        // return:
        // - the leftmost node removed from the tree
        // - whether the height of the tree has decreased
        fn take_leftmost<K, V>(anchor: &mut Option<Box<Node<K, V>>>) -> (Box<Node<K, V>>, bool) {
            let node = anchor.as_mut().unwrap();
            if node.children[0].as_mut().unwrap().children[0].is_some() {
                let (ret, height_decreased) = take_leftmost(&mut node.children[0]);
                node.tree_hash ^= ret.self_hash;
                node.tree_size -= 1;
                // we have removed a node from the left sub-tree, whose height might have
                // decreased, so we rebalance on the right
                let height_decreased = height_decreased && !rebalance(anchor, Direction::Right);
                (ret, height_decreased)
            } else {
                // splice the leftmost node
                // remove the leftmost node and its (right) sub-tree
                let mut leftmost = node.children[0].take().unwrap();
                node.tree_hash ^= leftmost.self_hash;
                node.tree_size -= 1;
                // re-root the leftmost node's (right) sub-tree to its parent's left
                if let Some(right) = leftmost.children[1].take() {
                    leftmost.tree_hash ^= right.tree_hash;
                    leftmost.tree_size -= right.tree_size;
                    node.children[0] = Some(right);
                }
                assert_eq!(leftmost.tree_size, 1);
                assert_eq!(leftmost.self_hash, leftmost.tree_hash);
                // whether the leftmost node had a (right) sub-tree or not, the height of the
                // sub-tree rooted at seen from its parent has decreased by one
                let height_decreased = !rebalance(anchor, Direction::Right);
                (leftmost, height_decreased)
            }
        }
        // return:
        // - hash difference
        // - the removed value, if any
        // - hether the height has decreased in the sub-tree
        fn aux<K: Hash + Ord, V>(
            anchor: &mut Option<Box<Node<K, V>>>,
            key: &K,
        ) -> (u64, Option<V>, bool) {
            if let Some(node) = anchor {
                match node.dir(key) {
                    Direction::None => {
                        let mut node = anchor.take().unwrap();
                        match (node.children[0].take(), node.children[1].take()) {
                            (None, None) => (node.self_hash, Some(node.value), true),
                            (None, Some(right)) => {
                                *anchor = Some(right);
                                (node.self_hash, Some(node.value), true)
                            }
                            (Some(left), None) => {
                                *anchor = Some(left);
                                (node.self_hash, Some(node.value), true)
                            }
                            (Some(left), Some(mut right)) => {
                                if right.children[0].is_some() {
                                    // this is ugly, but it's easier than making take_leftmost work
                                    // differently
                                    let mut tmp = Some(right);
                                    let (mut next_node, height_decreased) = take_leftmost(&mut tmp);
                                    let right = tmp.unwrap();
                                    next_node.tree_hash ^= left.tree_hash ^ right.tree_hash;
                                    next_node.tree_size += left.tree_size + right.tree_size;
                                    next_node.children[0] = Some(left);
                                    next_node.children[1] = Some(right);
                                    next_node.taller_subtree = node.taller_subtree;
                                    *anchor = Some(next_node);
                                    let height_decreased =
                                        height_decreased && !rebalance(anchor, Direction::Left);
                                    (node.self_hash, Some(node.value), height_decreased)
                                } else {
                                    right.tree_hash ^= left.tree_hash;
                                    right.tree_size += left.tree_size;
                                    right.children[0] = Some(left);
                                    right.taller_subtree = node.taller_subtree;
                                    *anchor = Some(right);
                                    let height_decreased = !rebalance(anchor, Direction::Left);
                                    (node.self_hash, Some(node.value), height_decreased)
                                }
                            }
                        }
                    }
                    dir => {
                        let (diff_hash, old_node, height_decreased) =
                            aux(&mut node.children[dir as usize], key);
                        node.tree_hash ^= diff_hash;
                        if old_node.is_some() {
                            node.tree_size -= 1;
                        }
                        let height_decreased = height_decreased && !rebalance(anchor, -dir);
                        (diff_hash, old_node, height_decreased)
                    }
                }
            } else {
                (0, None, false)
            }
        }
        aux(&mut self.root, key).1
    }

    pub fn check_invariants(&self) {
        // return:
        // - the cumulated hash of the sub-tree
        // - the number of nodes of the sub-tree
        // - the height of the sub-tree
        fn aux<K: Hash + Ord, V: Hash>(
            anchor: &Option<Box<Node<K, V>>>,
            min: Option<&K>,
            max: Option<&K>,
        ) -> (u64, usize, usize) {
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
                let (left_hash, left_size, left_height) =
                    aux(&node.children[0], min, Some(&node.key));
                let (right_hash, right_size, right_height) =
                    aux(&node.children[1], Some(&node.key), max);
                let tree_hash = left_hash ^ right_hash ^ self_hash;
                if tree_hash != node.tree_hash {
                    panic!("Tree hashing invariant violated");
                }
                let tree_size = left_size + right_size + 1;
                if tree_size != node.tree_size {
                    panic!("Tree size invariant violated");
                }
                match node.taller_subtree {
                    Direction::None => assert_eq!(left_height, right_height),
                    Direction::Left => assert_eq!(left_height, right_height + 1),
                    Direction::Right => assert_eq!(left_height + 1, right_height),
                }
                let tree_height = left_height.max(right_height);
                (tree_hash, tree_size, 1 + tree_height)
            } else {
                (0, 0, 0)
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
        let mut items: Vec<_> = iter.into_iter().collect();
        if items.is_empty() {
            return tree;
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let mut items: Vec<_> = items.into_iter().map(Some).collect();
        let mut parts = vec![&mut items[..]];
        let mut new_parts = Vec::new();
        while !parts.is_empty() {
            for part in parts.drain(..) {
                let (left, mid_and_right) = part.split_at_mut(part.len() / 2);
                if !left.is_empty() {
                    new_parts.push(left);
                }
                assert!(!mid_and_right.is_empty());
                let (mid, right) = mid_and_right.split_at_mut(1);
                let (k, v) = mid[0].take().unwrap();
                tree.insert(k, v);
                assert_eq!(mid.len(), 1);
                if !right.is_empty() {
                    new_parts.push(right);
                }
            }
            (parts, new_parts) = (new_parts, parts);
            new_parts.clear();
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
            if let Some(left) = node.children[0].take() {
                self.stack.push(node);
                self.stack.push(left);
                self.next()
            } else {
                if let Some(right) = node.children[1].take() {
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
                if let Some(left) = node.children[0].as_ref() {
                    self.stack.push((node, true));
                    self.stack.push((left, false));
                    return self.next();
                }
            }
            if let Some(right) = node.children[1].as_ref() {
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

impl<K: Hash + Ord, V: Hash> HashRangeQueryable for HTree<K, V> {
    type Key = K;
    fn hash<R: RangeBounds<K>>(&self, range: &R) -> u64 {
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
                    ret ^= aux(
                        &node.children[0],
                        range,
                        subtree_lower_bound,
                        Some(&node.key),
                    );
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
                    ret ^= aux(
                        &node.children[1],
                        range,
                        Some(&node.key),
                        subtree_upper_bound,
                    );
                }
                ret
            } else {
                0
            }
        }
        aux(&self.root, range, None, None)
    }

    fn insertion_position(&self, key: &K) -> usize {
        fn aux<K: Hash + Ord, V: Hash>(node: &Node<K, V>, key: &K) -> usize {
            match node.dir(key) {
                Direction::None => node.children[0]
                    .as_ref()
                    .map(|left| left.tree_size)
                    .unwrap_or(0),
                Direction::Left => node.children[0]
                    .as_ref()
                    .map(|left| aux(left, key))
                    .unwrap_or(0),
                Direction::Right => node.children[1]
                    .as_ref()
                    .map(|right| node.tree_size - right.tree_size + aux(right, key))
                    .unwrap_or(node.tree_size),
            }
        }
        self.root.as_ref().map(|node| aux(node, key)).unwrap_or(0)
    }

    fn key_at(&self, index: usize) -> &K {
        &self.node_at_index(index).key
    }

    fn len(&self) -> usize {
        self.root
            .as_ref()
            .map(|node| node.tree_size)
            .unwrap_or_default()
    }
}

pub struct ItemRange<'a, K, V, R: RangeBounds<K>> {
    range: &'a R,
    stack: Vec<(&'a Node<K, V>, bool)>,
}

impl<'a, K: Ord, V, R: RangeBounds<K>> Iterator for ItemRange<'a, K, V, R> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, left_explored)) = self.stack.pop() {
            if !left_explored {
                if let Some(left) = node.children[0].as_ref() {
                    self.stack.push((node, true));
                    self.stack.push((left, false));
                    return self.next();
                }
            }
            if !self.range.contains(&node.key) {
                self.stack.clear();
                return None;
            }
            if let Some(right) = node.children[1].as_ref() {
                self.stack.push((right, false));
            }
            Some((&node.key, &node.value))
        } else {
            None
        }
    }
}

impl<K: Ord, V> HTree<K, V> {
    pub fn get_range<'a, R: RangeBounds<K>>(&'a self, range: &'a R) -> ItemRange<'a, K, V, R> {
        let mut stack = Vec::new();
        let mut maybe_node = self.root.as_ref();
        while let Some(node) = maybe_node {
            match range_compare(&node.key, range) {
                RangeOrdering::Less => maybe_node = node.children[1].as_ref(),
                RangeOrdering::Greater => maybe_node = node.children[0].as_ref(),
                RangeOrdering::Inside => {
                    stack.push((node.as_ref(), true));
                    maybe_node = node.children[0].as_ref();
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
    let (diffs1, diffs2) = first.diff(second);
    for diff in diffs1 {
        for (k, v) in first.get_range(&diff) {
            second.insert(k.clone(), v.clone());
        }
    }
    for diff in diffs2 {
        for (k, v) in second.get_range(&diff) {
            first.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::{Bound, RangeBounds};

    use rand::{seq::SliceRandom, Rng, SeedableRng};

    use crate::diff::{Diffable, HashRangeQueryable};

    use super::{reconciliate, HTree};

    #[test]
    fn test_simple() {
        // empty
        let mut tree = HTree::new();
        assert_eq!(tree.hash(&..), 0);
        tree.check_invariants();

        // 1 value
        tree.insert(50, "Hello");
        tree.check_invariants();
        let hash1 = tree.hash(&..);
        assert_ne!(hash1, 0);

        // 2 values
        tree.insert(25, "World!");
        tree.check_invariants();
        let hash2 = tree.hash(&..);
        assert_ne!(hash2, 0);
        assert_ne!(hash2, hash1);

        // 3 values
        tree.insert(75, "Everyone!");
        tree.check_invariants();
        let hash3 = tree.hash(&..);
        assert_ne!(hash3, 0);
        assert_ne!(hash3, hash1);
        assert_ne!(hash3, hash2);

        // back to 2 values
        tree.remove(&75);
        tree.check_invariants();
        let hash4 = tree.hash(&..);
        assert_eq!(hash4, hash2);
    }

    #[test]
    fn test_compare() {
        let tree1 = HTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Everyone!")]);
        let tree2 = HTree::from_iter([(75, "Everyone!"), (50, "Hello"), (25, "World!")]);
        let tree3 = HTree::from_iter([(75, "Everyone!"), (25, "World!"), (50, "Hello")]);
        let tree4 = HTree::from_iter([(75, "Everyone!"), (25, "World!"), (40, "Hello")]);
        let tree5 = HTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Goodbye!")]);

        assert_eq!(tree1.hash(&..), tree1.hash(&..));
        assert_eq!(tree1.hash(&..), tree2.hash(&..));
        assert_eq!(tree1.hash(&..), tree3.hash(&..));
        assert_ne!(tree1.hash(&..), tree4.hash(&..));
        assert_ne!(tree1.hash(&..), tree5.hash(&..));

        assert_eq!(tree1, tree1);
        assert_eq!(tree1, tree2);
        assert_eq!(tree1, tree3);
        assert_ne!(tree1, tree4);
        assert_ne!(tree1, tree5);

        assert_eq!(tree1.diff(&tree1), (vec![], vec![]));
        assert_eq!(tree1.diff(&tree2), (vec![], vec![]));
        assert_eq!(tree1.diff(&tree3), (vec![], vec![]));
        assert_eq!(
            tree1.diff(&tree4),
            (
                vec![(Bound::Included(40), Bound::Excluded(75))],
                vec![(Bound::Included(40), Bound::Excluded(75))],
            ),
        );
        assert_eq!(
            tree1.diff(&tree5),
            (
                vec![(Bound::Included(75), Bound::Unbounded)],
                vec![(Bound::Included(75), Bound::Unbounded)],
            ),
        );

        let range = tree1.get_range(&(Bound::Included(40), Bound::Excluded(50)));
        assert_eq!(range.collect::<Vec<_>>(), vec![]);
        let range = tree1.get_range(&(Bound::Included(50), Bound::Excluded(75)));
        assert_eq!(range.collect::<Vec<_>>(), vec![(&50, &"Hello")]);
        let range = tree4.get_range(&(Bound::Included(40), Bound::Excluded(50)));
        assert_eq!(range.collect::<Vec<_>>(), vec![(&40, &"Hello")]);
        let range = tree4.get_range(&(Bound::Included(50), Bound::Excluded(75)));
        assert_eq!(range.collect::<Vec<_>>(), vec![]);

        let mut tree1 = tree1;
        let mut tree4 = tree4;
        reconciliate(&mut tree1, &mut tree4);
        assert_eq!(tree1, tree4);
        assert_eq!(
            tree1.get_range(&..).collect::<Vec<_>>(),
            [
                (&25, &"World!"),
                (&40, &"Hello"),
                (&50, &"Hello"),
                (&75, &"Everyone!")
            ]
        )
    }

    #[test]
    fn big_test() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut tree1 = HTree::new();
        let mut key_values = Vec::new();

        let mut expected_hash = 0;

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen::<u64>();
            let value: u64 = rng.gen();
            let old = tree1.insert(key, value);
            assert!(old.is_none());
            tree1.check_invariants();
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(tree1.hash(&..), expected_hash);
            key_values.push((key, value));
        }

        // in the tree, the items should now be sorted
        key_values.sort();

        let mut tree2 = HTree::from_iter(key_values.iter().copied());
        assert_eq!(tree1, tree2);

        // check for partial ranges
        let mid = key_values[key_values.len() / 2].0;
        assert_ne!(tree1.hash(&(mid..)), tree1.hash(&..));
        assert_ne!(tree1.hash(&..mid), tree1.hash(&..));
        assert_eq!(tree1.hash(&..mid) ^ tree1.hash(&(mid..)), tree1.hash(&..));

        for _ in 0..100 {
            let index = rng.gen::<usize>() % key_values.len();
            let key = key_values[index].0;
            assert_eq!(*tree1.key_at(index), key);
            assert_eq!(tree1.position(&key), Some(index));
            assert_eq!(tree1.insertion_position(&key), index);
        }
        assert_eq!(tree1.insertion_position(&0), 0);
        assert_eq!(tree1.insertion_position(&u64::MAX), tree1.len());

        // test iteration
        let items: Vec<(u64, u64)> = tree1.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(items.len(), key_values.len());
        assert_eq!(items, key_values);

        // test get_range
        let from_index = rng.gen_range(0..key_values.len());
        let to_index = rng.gen_range(from_index..key_values.len());
        let from_key = tree1.key_at(from_index);
        let to_key = tree1.key_at(to_index);
        fn test_range<
            R: RangeBounds<u64>,
            SI: std::slice::SliceIndex<[(u64, u64)], Output = [(u64, u64)]>,
        >(
            key_values: &Vec<(u64, u64)>,
            tree: &HTree<u64, u64>,
            range: R,
            slice_index: SI,
        ) {
            assert_eq!(
                tree.get_range(&range)
                    .map(|(k, v)| (*k, *v))
                    .collect::<Vec<_>>(),
                key_values[slice_index]
            );
        }
        test_range(&key_values, &tree1, from_key..to_key, from_index..to_index);
        test_range(
            &key_values,
            &tree1,
            from_key..=to_key,
            from_index..=to_index,
        );
        test_range(&key_values, &tree1, ..to_key, ..to_index);
        test_range(&key_values, &tree1, ..=to_key, ..=to_index);
        test_range(&key_values, &tree1, from_key.., from_index..);
        test_range(&key_values, &tree1, .., ..);

        // test diff
        let key: u64 = rng.gen::<u64>();
        let value: u64 = rng.gen();
        let old = tree2.insert(key, value);
        assert!(old.is_none());
        let mut diffs1 = Vec::new();
        let mut diffs2 = Vec::new();
        let mut segments = tree1.start_diff();
        while !segments.is_empty() {
            segments = tree2.diff_round(&mut diffs2, segments);
            segments = tree1.diff_round(&mut diffs1, segments);
        }
        assert_eq!(diffs1.len(), 0);
        assert_eq!(diffs2.len(), 1);
        let items: Vec<_> = tree2.get_range(&diffs2[0]).collect();
        assert_eq!(items, vec![(&key, &value)]);

        // remove some
        key_values.shuffle(&mut rng);
        for _ in 0..1000 {
            let (key, value) = key_values.pop().unwrap();
            let value2 = tree1.remove(&key);
            tree1.check_invariants();
            assert_eq!(value2, Some(value));
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(tree1.hash(&..), expected_hash);
        }
    }
}

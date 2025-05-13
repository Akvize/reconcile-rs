use std::{hash::Hash, marker::PhantomData};

use crate::hrtree::{HRTree, Node};

impl<K: Hash + Ord, V: Hash> FromIterator<(K, V)> for HRTree<K, V> {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (K, V)>,
    {
        let mut tree = HRTree::new();
        let mut items: Vec<_> = iter.into_iter().collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in items {
            tree.insert(k, v);
        }
        tree
    }
}

enum IntoIterLayer<K, V> {
    Node(Box<Node<K, V>>),
    Element(K, V),
}

pub struct IntoIter<K, V> {
    stack: Vec<IntoIterLayer<K, V>>,
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoIterLayer::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack
                        .push(IntoIterLayer::Node(children.pop().unwrap()));
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterLayer::Element(k, v));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoIterLayer::Node(c));
                    }
                } else {
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterLayer::Element(k, v));
                    }
                }
                self.next()
            }
            Some(IntoIterLayer::Element(k, v)) => Some((k, v)),
            None => None,
        }
    }
}

impl<K, V> IntoIterator for HRTree<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            stack: vec![IntoIterLayer::Node(self.root)],
        }
    }
}

pub struct Iter<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, usize)>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, children_passed)) = self.stack.pop() {
            if children_passed < node.keys.len() {
                self.stack.push((node, children_passed + 1));
            }
            if let Some(children) = node.children.as_ref() {
                self.stack.push((&children[children_passed], 0));
            }
            if children_passed > 0 {
                Some((
                    &node.keys[children_passed - 1],
                    &node.values[children_passed - 1],
                ))
            } else {
                self.next()
            }
        } else {
            None
        }
    }
}

impl<'a, K, V> IntoIterator for &'a HRTree<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        Iter {
            stack: vec![(&self.root, 0)],
        }
    }
}

impl<K, V> HRTree<K, V> {
    pub fn iter(&self) -> Iter<K, V> {
        self.into_iter()
    }
}

pub struct IterMut<'a, K, V> {
    stack: Vec<(*mut Node<K, V>, usize)>,
    _marker: PhantomData<&'a mut V>,
}

impl<'a, K: 'a + Hash + Ord, V: Hash> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node_ptr, idx)) = self.stack.pop() {
            unsafe {
                let node = &mut *node_ptr;
                if idx < node.keys.len() {
                    // Prepare next
                    self.stack.push((node_ptr, idx + 1));
                    // Traverse right subtree
                    if let Some(children) = node.children.as_mut() {
                        let mut child_ptr: *mut Node<K, V> = &mut *children[idx + 1];
                        while let Some(gc) = (*child_ptr).children.as_mut() {
                            self.stack.push((child_ptr, 0));
                            child_ptr = &mut *gc[0];
                        }
                        self.stack.push((child_ptr, 0));
                    }
                    return Some((&node.keys[idx], &mut node.values[idx]));
                }
            }
        }
        None
    }
}

impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    pub fn iter_mut(&'a mut self) -> IterMut<'a, K, V> {
        let mut stack = Vec::new();
        // Unsafe pointer to root
        let mut cur_ptr: *mut Node<K, V> = &mut *self.root;
        unsafe {
            // Descend to leftmost leaf
            while let Some(children) = (*cur_ptr).children.as_mut() {
                stack.push((cur_ptr, 0));
                cur_ptr = &mut *children[0];
            }
            stack.push((cur_ptr, 0));
        }
        IterMut {
            stack,
            _marker: PhantomData,
        }
    }
}

enum IntoValuesLayer<K, V> {
    Node(Box<Node<K, V>>),
    Element(V),
}

pub struct IntoValues<K, V> {
    stack: Vec<IntoValuesLayer<K, V>>,
}

impl<K, V> Iterator for IntoValues<K, V> {
    type Item = V;
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoValuesLayer::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack
                        .push(IntoValuesLayer::Node(children.pop().unwrap()));
                    while !node.values.is_empty() {
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoValuesLayer::Element(v));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoValuesLayer::Node(c));
                    }
                } else {
                    while !node.values.is_empty() {
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoValuesLayer::Element(v));
                    }
                }
                self.next()
            }
            Some(IntoValuesLayer::Element(v)) => Some(v),
            None => None,
        }
    }
}

impl<K, V> HRTree<K, V> {
    pub fn into_values(self) -> IntoValues<K, V> {
        IntoValues {
            stack: vec![IntoValuesLayer::Node(self.root)],
        }
    }
}

pub struct Values<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, usize)>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, children_passed)) = self.stack.pop() {
            if children_passed < node.keys.len() {
                self.stack.push((node, children_passed + 1));
            }
            if let Some(children) = node.children.as_ref() {
                self.stack.push((&children[children_passed], 0));
            }
            if children_passed > 0 {
                Some(&node.values[children_passed - 1])
            } else {
                self.next()
            }
        } else {
            None
        }
    }
}

impl<K, V> HRTree<K, V> {
    pub fn values(&self) -> Values<'_, K, V> {
        Values {
            stack: vec![(&self.root, 0)],
        }
    }
}

pub struct ValuesMut<'a, K, V> {
    stack: Vec<(*mut Node<K, V>, usize)>,
    _marker: PhantomData<&'a mut V>,
}

impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    /// Returns a mutable iterator over values in-order.
    pub fn values_mut(&'a mut self) -> ValuesMut<'a, K, V> {
        let mut stack = Vec::new();
        let mut cur_ptr: *mut Node<K, V> = &mut *self.root;
        unsafe {
            while let Some(children) = (*cur_ptr).children.as_mut() {
                stack.push((cur_ptr, 0));
                cur_ptr = &mut *children[0];
            }
            stack.push((cur_ptr, 0));
        }
        ValuesMut {
            stack,
            _marker: PhantomData,
        }
    }
}

impl<'a, K: Hash + Ord, V: Hash> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node_ptr, idx)) = self.stack.pop() {
            unsafe {
                let node = &mut *node_ptr;
                if idx < node.keys.len() {
                    self.stack.push((node_ptr, idx + 1));
                    if let Some(children) = node.children.as_mut() {
                        let mut child_ptr: *mut Node<K, V> = &mut *children[idx + 1] as *mut _;
                        while let Some(gc) = (*child_ptr).children.as_mut() {
                            self.stack.push((child_ptr, 0));
                            child_ptr = &mut *gc[0];
                        }
                        self.stack.push((child_ptr, 0));
                    }
                    return Some(&mut node.values[idx]);
                }
            }
        }
        None
    }
}

enum IntoKeysLayer<K, V> {
    Node(Box<Node<K, V>>),
    Element(K),
}

pub struct IntoKeys<K, V> {
    stack: Vec<IntoKeysLayer<K, V>>,
}

impl<K, V> Iterator for IntoKeys<K, V> {
    type Item = K;
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoKeysLayer::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack
                        .push(IntoKeysLayer::Node(children.pop().unwrap()));
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        self.stack.push(IntoKeysLayer::Element(k));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoKeysLayer::Node(c));
                    }
                } else {
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        self.stack.push(IntoKeysLayer::Element(k));
                    }
                }
                self.next()
            }
            Some(IntoKeysLayer::Element(k)) => Some(k),
            None => None,
        }
    }
}

impl<K, V> HRTree<K, V> {
    pub fn into_keys(self) -> IntoKeys<K, V> {
        IntoKeys {
            stack: vec![IntoKeysLayer::Node(self.root)],
        }
    }
}

pub struct Keys<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, usize)>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, children_passed)) = self.stack.pop() {
            if children_passed < node.keys.len() {
                self.stack.push((node, children_passed + 1));
            }
            if let Some(children) = node.children.as_ref() {
                self.stack.push((&children[children_passed], 0));
            }
            if children_passed > 0 {
                Some(&node.keys[children_passed - 1])
            } else {
                self.next()
            }
        } else {
            None
        }
    }
}

impl<K, V> HRTree<K, V> {
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys {
            stack: vec![(&self.root, 0)],
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::{Rng, SeedableRng};

    use super::HRTree;
    use once_cell::sync::Lazy;

    const TREE_SIZE: usize = 1000;

    static BASE_ITEMS: Lazy<Vec<(u64, u64)>> = Lazy::new(|| {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        (0..TREE_SIZE)
            .map(|i| (i as u64, rng.gen::<u64>()))
            .collect()
    });

    fn make_tree() -> HRTree<u64, u64> {
        HRTree::from_iter(BASE_ITEMS.clone())
    }

    #[test]
    fn test_into_iter() {
        let tree = make_tree();
        assert_eq!(
            tree.clone().into_iter().collect::<Vec<_>>(),
            BASE_ITEMS.clone()
        );
    }

    #[test]
    fn test_iter() {
        let tree = make_tree();
        assert_eq!(
            tree.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>(),
            BASE_ITEMS.clone()
        );
    }

    #[test]
    fn test_iter_mut_0_modification() {
        let mut tree = make_tree();
        let collected: Vec<_> = tree.iter_mut().map(|(_, v)| *v).collect();
        let expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn test_iter_mut_1_modification() {
        let mut tree = make_tree();

        let num = rand::random::<usize>().rem_euclid(TREE_SIZE);
        let (key, value) = BASE_ITEMS[num];
        let mut expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        expected[num] = value;

        for (k, v) in tree.iter_mut() {
            if *k == key {
                *v = value;
            }
        }
        let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn test_into_values() {
        let values: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        let tree = make_tree();
        assert_eq!(tree.clone().into_values().collect::<Vec<_>>(), values);
    }

    #[test]
    fn test_values() {
        let values: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        let tree = make_tree();
        assert_eq!(tree.values().copied().collect::<Vec<_>>(), values);
    }

    #[test]
    fn test_values_mut_0_modification() {
        let mut tree = make_tree();
        let collected: Vec<_> = tree.values_mut().map(|v| *v).collect();
        let expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn test_values_mut_1_modification() {
        let mut tree = make_tree();

        let num = rand::random::<usize>().rem_euclid(TREE_SIZE);
        let (_, value) = BASE_ITEMS[num];
        let mut expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        expected[num] = value;

        for (n, v) in tree.values_mut().enumerate() {
            if n == num {
                *v = value;
            }
        }
        let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn test_into_keys() {
        let keys: Vec<_> = BASE_ITEMS.iter().map(|&(k, _)| k).collect();
        let tree = make_tree();
        assert_eq!(tree.clone().into_keys().collect::<Vec<_>>(), keys);
    }

    #[test]
    fn test_keys() {
        let keys: Vec<_> = BASE_ITEMS.iter().map(|&(k, _)| k).collect();
        let tree = make_tree();
        assert_eq!(tree.keys().copied().collect::<Vec<_>>(), keys);
    }

    #[test]
    fn test_empty_tree_iterators() {
        let tree: HRTree<u64, u64> = HRTree::new();
        assert!(tree.clone().into_iter().next().is_none());
        assert!(tree.iter().next().is_none());
        assert!(tree.clone().into_values().next().is_none());
        assert!(tree.values().next().is_none());
        assert!(tree.clone().into_keys().next().is_none());
        assert!(tree.keys().next().is_none());
    }
}

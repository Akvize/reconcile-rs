use std::hash::Hash;

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

enum IntoIterItem<K, V> {
    Node(Box<Node<K, V>>),
    Element(K, V),
}

pub struct IntoIter<K, V> {
    stack: Vec<IntoIterItem<K, V>>,
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoIterItem::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack.push(IntoIterItem::Node(children.pop().unwrap()));
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterItem::Element(k, v));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoIterItem::Node(c));
                    }
                } else {
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterItem::Element(k, v));
                    }
                }
                self.next()
            }
            Some(IntoIterItem::Element(k, v)) => Some((k, v)),
            None => None,
        }
    }
}

impl<K, V> IntoIterator for HRTree<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            stack: vec![IntoIterItem::Node(self.root)],
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
            if children_passed <= node.keys.len() {
                if let Some(children) = node.children.as_ref() {
                    self.stack.push((&children[children_passed], 0));
                }
            }
            if 0 < children_passed && children_passed <= node.keys.len() {
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

enum IntoValuesItem<K, V> {
    Node(Box<Node<K, V>>),
    Element(V),
}

pub struct IntoValues<K, V> {
    stack: Vec<IntoValuesItem<K, V>>,
}

impl<K, V> Iterator for IntoValues<K, V> {
    type Item = V;
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoValuesItem::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack
                        .push(IntoValuesItem::Node(children.pop().unwrap()));
                    while !node.values.is_empty() {
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoValuesItem::Element(v));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoValuesItem::Node(c));
                    }
                } else {
                    while !node.values.is_empty() {
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoValuesItem::Element(v));
                    }
                }
                self.next()
            }
            Some(IntoValuesItem::Element(v)) => Some(v),
            None => None,
        }
    }
}

impl<K, V> HRTree<K, V> {
    pub fn into_values(self) -> IntoValues<K, V> {
        IntoValues {
            stack: vec![IntoValuesItem::Node(self.root)],
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::{Rng, SeedableRng};

    use super::HRTree;

    #[test]
    fn test_iter() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut key_values = Vec::new();

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen::<u64>();
            let value: u64 = rng.gen();
            key_values.push((key, value));
        }
        let tree = HRTree::from_iter(key_values.clone());
        key_values.sort();
        let values: Vec<_> = key_values.iter().map(|(_, v)| *v).collect();

        // test into_iter()
        assert_eq!(tree.clone().into_iter().collect::<Vec<_>>(), key_values);

        // test iter()
        assert_eq!(
            tree.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>(),
            key_values
        );

        // test into_values()
        assert_eq!(tree.clone().into_values().collect::<Vec<_>>(), values);
    }
}

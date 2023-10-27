pub mod rhtree;

use diff::{Diffs, Diffable};

pub trait Reconcilable: Diffable {
    type Value;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> u64;
    fn send_updates(&self, diffs: Diffs<Self::Key>) -> Vec<(Self::Key, Self::Value)>;
}

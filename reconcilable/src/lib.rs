pub mod rhtree;

use diff::{DiffRanges, Diffable};

pub trait Reconcilable: Diffable {
    type Value;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> Option<u64>;
    fn send_updates(&self, diff_ranges: DiffRanges<Self::Key>) -> Vec<(Self::Key, Self::Value)>;
}

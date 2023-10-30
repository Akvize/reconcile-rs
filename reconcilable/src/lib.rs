pub mod rhtree;

use diff::{Diffable, Diffs};

pub trait Reconcilable: Diffable {
    type Value;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> Option<u64>;
    fn send_updates(&self, diffs: Diffs<Self::Key>) -> Vec<(Self::Key, Self::Value)>;
}

use crate::diff::Diffs;

pub trait Reconcilable {
    type Key;
    type Value;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>);
    fn send_updates(&self, diffs: Diffs<Self::Key>) -> Vec<(Self::Key, Self::Value)>;
}
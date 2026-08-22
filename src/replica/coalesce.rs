// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Broadcast coalescing (#187): batch local writes made within
//! [`coalesce_window`](crate::replicated_map::Config::coalesce_window) of each other into one
//! flush instead of one broadcast per write, collapsing same-key writes to the greatest
//! [`Timestamp`] via [`Entry::merge`] before anything reaches the wire. Disabled by default
//! (`coalesce_window == Duration::ZERO`): every write flushes immediately, byte-for-byte the
//! pre-#187 behavior — see [`Replica::queue_broadcast`].

use std::hash::Hash;
use std::time::Duration;

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};

use super::{Message, Replica};

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Route a batch of local writes to peers: immediately when coalescing is disabled (the
    /// default), or merged into the pending buffer and flushed together once
    /// [`coalesce_window`](Inner::coalesce_window) elapses since the buffer was last empty.
    ///
    /// Every propagating write path ([`insert`](Self::insert),
    /// [`broadcast_update`](Self::broadcast_update), [`insert_bulk`](Self::insert_bulk)) funnels
    /// through this, so the two behaviors stay interchangeable at exactly one call site.
    ///
    /// # Panics
    ///
    /// With coalescing enabled, only the call that finds the pending buffer empty spawns the
    /// detached flush task — and so is the one that needs an ambient Tokio runtime, per
    /// [`insert`](Self::insert)'s `# Panics`. A write that joins an already-scheduled window
    /// returns without touching the reactor and cannot panic this way.
    pub(super) fn queue_broadcast(&self, key_values: Vec<(K, Entry<Timestamp, V>)>) {
        if key_values.is_empty() {
            return;
        }
        let window = *self.coalesce_window.read();
        if window.is_zero() {
            let messages = key_values
                .into_iter()
                .map(Message::Update::<K, Entry<Timestamp, V>, State<V>>)
                .collect();
            self.broadcast(messages);
            return;
        }
        // Merge-and-check-empty under one write guard: two inserts racing to be "the one that
        // schedules the flush" are serialized here, so exactly one of them observes `was_empty`
        // and only that one spawns the task below.
        let was_empty = {
            let mut pending = self.coalesce_pending.write();
            let was_empty = pending.is_empty();
            for (key, value) in key_values {
                pending
                    .entry(key)
                    .and_modify(|existing: &mut Entry<Timestamp, V>| {
                        *existing = existing.merge(&value);
                    })
                    .or_insert(value);
            }
            was_empty
        };
        if was_empty {
            self.spawn_coalesce_flush(window);
        }
    }

    /// Sleep `window`, then broadcast whatever accumulated in the pending buffer meanwhile as one
    /// batch, one send loop. Self-contained — mirrors [`broadcast`](Self::broadcast)'s own
    /// detached-task shape — so a write issued before [`run`](Self::run) starts, or after it
    /// stops, still flushes on its own schedule rather than depending on that loop's lifecycle.
    fn spawn_coalesce_flush(&self, window: Duration) {
        let engine = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(window).await;
            let batch: Vec<_> = std::mem::take(&mut *engine.coalesce_pending.write())
                .into_iter()
                .collect();
            if !batch.is_empty() {
                let messages = batch
                    .into_iter()
                    .map(Message::Update::<K, Entry<Timestamp, V>, State<V>>)
                    .collect();
                engine.broadcast(messages);
            }
        });
    }
}

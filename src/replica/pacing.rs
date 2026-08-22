// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::HashSet;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use rbsr::EnumerationRange;
use serde::Serialize;
use tokio::time::sleep;
use tracing::{debug, error, instrument, trace, warn};

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::observability;
use crate::transport::Transport;
use gossip::auth;
use gossip::replay;

use super::{Message, Replica, BUFFER_SIZE, MAX_SENDTO_RETRIES};

/// Which channel a paced bulk dump resolves ranges against (#516): a `differences` batch that
/// loses the per-peer dump-slot race is stashed by channel, since the dated and value-only
/// channels share one slot but resolve ranges against different trees and message variants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DumpChannel {
    /// `self.map`, resolves to [`Message::Update`].
    Dated,
    /// `self.projection`, resolves to [`Message::ValueUpdate`].
    ValueOnly,
}

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Bundle this engine's outbound ports and send state for the batched-message helpers
    /// ([`send_messages_to`] / [`send_messages_paced`]). See [`SendPorts`].
    pub(super) fn send_ports(&self) -> SendPorts<'_, dyn Transport> {
        SendPorts {
            transport: &*self.transport,
            authenticator: &self.authenticator,
            sender_counter: &self.sender_counter,
        }
    }

    /// Claim both a per-peer in-flight slot and a global dump slot, or `None` if either is taken.
    ///
    /// Called **before** snapshotting the range, so a skipped dump allocates nothing; the guards
    /// release on drop, panic included.
    pub(super) fn try_claim_dump_slot(
        &self,
        peer: SocketAddr,
    ) -> Option<(BulkInFlightGuard, BulkDumpCountGuard)> {
        // Per-peer guard: at most one dump per peer at a time.
        if !self.bulk_in_flight.write().insert(peer) {
            return None;
        }
        // Global budget: at most `max_concurrent_bulk_dumps` across all peers. The
        // compare-exchange loop increments only if currently below the cap. If at cap, release the
        // per-peer mark before returning so that slot is not leaked.
        let budget = self.max_concurrent_bulk_dumps;
        let claimed = self
            .bulk_dumps_in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n < budget {
                    Some(n + 1)
                } else {
                    None
                }
            })
            .is_ok();
        if !claimed {
            self.bulk_in_flight.write().remove(&peer);
            trace!("skipped bulk dump to {peer}: global dump budget ({budget}) exhausted");
            return None;
        }
        Some((
            BulkInFlightGuard {
                set: Arc::clone(&self.bulk_in_flight),
                peer,
            },
            BulkDumpCountGuard {
                counter: Arc::clone(&self.bulk_dumps_in_flight),
            },
        ))
    }

    /// Stash a `differences` batch that lost the per-peer dump-slot race (#516) instead of
    /// letting the caller drop it. Drained by whichever task is currently holding `peer`'s slot,
    /// via [`spawn_paced_send`](Self::spawn_paced_send)'s own loop — never dependent on a new
    /// incoming datagram or the idle `reconcile_interval` timeout.
    pub(super) fn stash_pending_dump(
        &self,
        channel: DumpChannel,
        peer: SocketAddr,
        ranges: Vec<EnumerationRange<K>>,
    ) {
        let stash = match channel {
            DumpChannel::Dated => &self.pending_dumps,
            DumpChannel::ValueOnly => &self.pending_value_dumps,
        };
        stash.write().entry(peer).or_default().extend(ranges);
    }

    /// Send a bulk batch of differing values to one peer on a detached, **rate-paced** task —
    /// the cold-sync path.
    ///
    /// Three mechanisms bound it, all against the same amplification: pacing to
    /// [`bulk_send_rate`](Inner::bulk_send_rate) off the receive loop, one dump per peer (an
    /// `Update` triggers no reply, so the holder's reconcile timer would otherwise re-dump ranges
    /// in transit), and a global [`try_claim_dump_slot`](Self::try_claim_dump_slot) budget
    /// bounding total in-flight snapshot memory.
    ///
    /// Before releasing `peer`'s slot, drains [`stash_pending_dump`](Self::stash_pending_dump)'s
    /// stash for `channel`/`peer` and sends that too, looping until nothing more is pending
    /// (#516): a `differences` batch discovered while this task was already sending must not wait
    /// for a fresh round to be noticed.
    pub(super) fn spawn_paced_send(
        &self,
        messages: Vec<Message<K, Entry<Timestamp, V>, State<V>>>,
        peer: SocketAddr,
        peer_guard: BulkInFlightGuard,
        global_guard: BulkDumpCountGuard,
        channel: DumpChannel,
    ) {
        let transport = Arc::clone(&self.transport);
        let authenticator = self.authenticator.clone();
        let sender_counter = Arc::clone(&self.sender_counter);
        let rate = self.bulk_send_rate;
        let map = Arc::clone(&self.map);
        let projection = Arc::clone(&self.projection);
        let pending_dumps = Arc::clone(&self.pending_dumps);
        let pending_value_dumps = Arc::clone(&self.pending_value_dumps);
        tokio::spawn(async move {
            // Hold both RAII guards for the lifetime of this task, releasing them only once
            // nothing more is pending for `peer` on `channel` — even if aborted or panicking.
            let _peer_guard = peer_guard;
            let _global_guard = global_guard;
            let ports = SendPorts {
                transport: &*transport,
                authenticator: &authenticator,
                sender_counter: &sender_counter,
            };
            let mut send_buf = Vec::new();
            let mut messages = messages;
            loop {
                send_messages_paced(&messages, &ports, &peer, &mut send_buf, rate).await;
                let stash = match channel {
                    DumpChannel::Dated => &pending_dumps,
                    DumpChannel::ValueOnly => &pending_value_dumps,
                };
                let Some(ranges) = stash.write().remove(&peer).filter(|r| !r.is_empty()) else {
                    break;
                };
                messages = Vec::new();
                match channel {
                    DumpChannel::Dated => {
                        let guard = map.read();
                        for range in ranges {
                            for (k, v) in guard.range(range) {
                                messages.push(Message::Update((k.clone(), v.clone())));
                            }
                        }
                    }
                    DumpChannel::ValueOnly => {
                        let guard = projection.read();
                        for range in ranges {
                            for (k, v) in guard.range(range) {
                                messages.push(Message::ValueUpdate((k.clone(), v.clone())));
                            }
                        }
                    }
                }
                if messages.is_empty() {
                    break;
                }
            }
        });
    }
}

/// The three things a batched send needs, which always travel together: the [`Transport`], the
/// authenticator, and the per-sender replay counter.
pub(crate) struct SendPorts<'a, T: ?Sized> {
    pub(crate) transport: &'a T,
    pub(crate) authenticator: &'a auth::Authenticator,
    pub(crate) sender_counter: &'a replay::SenderCounter,
}

pub(crate) async fn send_to_retry<T: Transport + ?Sized>(
    transport: &T,
    authenticator: &auth::Authenticator,
    sender_counter: &replay::SenderCounter,
    buf: &[u8],
    target: SocketAddr,
) -> std::io::Result<usize> {
    // Allocate a sequence number and stamp, then frame the datagram once and reuse it across
    // retries. `seal` always frames — even disabled adds the wire-version byte.
    let seq = sender_counter.next_seq();
    let stamp = sender_counter.next_stamp();
    let wire = authenticator.seal(seq, stamp, buf);
    let mut res = Ok(0);
    for _ in 0..MAX_SENDTO_RETRIES {
        res = transport.send_to(&wire, &target).await;
        if res.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    match &res {
        Ok(sent) => observability::record_bytes_sent(*sent),
        Err(err) => {
            error!("send_to failed after {MAX_SENDTO_RETRIES} retries: {err}");
            observability::record_send_failure();
        }
    }
    res
}

/// Send `messages` to `peer` back-to-back: the small latency-sensitive batches. Bulk dumps use
/// [`send_messages_paced`].
pub(crate) async fn send_messages_to<K, V, P, T>(
    messages: &[Message<K, V, P>],
    ports: &SendPorts<'_, T>,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
) where
    K: Serialize,
    V: Serialize,
    P: Serialize,
    T: Transport + ?Sized,
{
    send_messages_paced(messages, ports, peer, send_buf, None).await
}

/// Send `messages` to `peer` as ≤64 KiB datagrams, metered to `rate` bytes/sec when set.
///
/// Sleeps between datagrams, so it must run **off** the receive loop
/// ([`Replica::spawn_paced_send`]) — pacing inline would stall reception for every other peer.
#[instrument(name = "reconcile.send", skip_all, fields(peer = %peer, count = messages.len()))]
pub(crate) async fn send_messages_paced<K, V, P, T>(
    messages: &[Message<K, V, P>],
    ports: &SendPorts<'_, T>,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
    rate: Option<usize>,
) where
    K: Serialize,
    V: Serialize,
    P: Serialize,
    T: Transport + ?Sized,
{
    debug!("sending {} messages to {peer}", messages.len());
    // Reserve room for the authentication tag so the sealed datagram still fits a UDP payload.
    let max_payload = BUFFER_SIZE - ports.authenticator.overhead();
    send_buf.clear();
    // Anchor the pacing schedule once, so it self-corrects rather than drifting per datagram.
    let start = Instant::now();
    let mut sent_bytes: usize = 0;
    for message in messages {
        let last_size = send_buf.len();
        gossip::bincode::encode(message, send_buf)
            .expect("serializing a protocol Message into an in-memory buffer cannot fail");
        let this_message_len = send_buf.len() - last_size;
        if send_buf.len() > max_payload {
            // Flush whatever was accumulated *before* this message, if anything — a real,
            // correctly-sized datagram, unaffected by whether this message itself fits.
            if last_size > 0 {
                trace!("sending {} bytes to {peer}", last_size);
                if let Err(err) = send_to_retry(
                    ports.transport,
                    ports.authenticator,
                    ports.sender_counter,
                    &send_buf[..last_size],
                    *peer,
                )
                .await
                {
                    warn!("failed to send datagram to {peer}: {err}; continuing");
                } else {
                    trace!("sent {} bytes to {peer}", last_size);
                }
                sent_bytes += last_size;
                pace(rate, start, sent_bytes).await;
            }
            send_buf.drain(..last_size);
            if this_message_len > max_payload {
                // This message's own encoding exceeds `max_payload` on its own — no datagram it
                // could ever be packed into, alone or otherwise. Sending it anyway (either as a
                // bogus empty datagram when it was first in the batch, or as an oversized one
                // otherwise) never converges the key and only ever fails with EMSGSIZE. Drop it,
                // counted and logged distinctly from a transport send failure so it is alertable
                // rather than silently retried forever.
                error!(
                    "dropping oversized message to {peer}: encodes to {this_message_len} bytes, \
                     exceeding the {max_payload}-byte datagram budget; this key will never \
                     converge on this peer until a smaller value is written"
                );
                observability::record_value_oversized();
                send_buf.clear();
            }
        }
    }
    // Empty exactly when the batch was empty, or ended with an oversized message that was just
    // dropped above — either way, an empty datagram is not a real send.
    if !send_buf.is_empty() {
        trace!("sending last {} bytes to {peer}", send_buf.len());
        if let Err(err) = send_to_retry(
            ports.transport,
            ports.authenticator,
            ports.sender_counter,
            send_buf,
            *peer,
        )
        .await
        {
            warn!("failed to send final datagram to {peer}: {err}; continuing");
        } else {
            trace!("sent last {} bytes to {peer}", send_buf.len());
        }
    }
}

/// Sleep, if necessary, so that having sent `sent_bytes` since `start` does not exceed `rate`
/// bytes/sec. A `None` (or zero) rate is a no-op. The schedule is anchored to `start`, so it
/// self-corrects and does not drift; the caller does not pace after the final datagram.
async fn pace(rate: Option<usize>, start: Instant, sent_bytes: usize) {
    let Some(rate) = rate.filter(|&r| r > 0) else {
        return;
    };
    let expected = Duration::from_secs_f64(sent_bytes as f64 / rate as f64);
    if let Some(delay) = expected.checked_sub(start.elapsed()) {
        sleep(delay).await;
    }
}

/// RAII marker that a bulk dump to `peer` is in flight. Clearing on `Drop` means a panicking send
/// task cannot wedge a peer into permanently transferring.
pub(super) struct BulkInFlightGuard {
    set: Arc<RwLock<HashSet<SocketAddr>>>,
    peer: SocketAddr,
}

impl Drop for BulkInFlightGuard {
    fn drop(&mut self) {
        self.set.write().remove(&self.peer);
    }
}

/// RAII counter-decrement for the global concurrent-dump budget. Decrements the shared atomic on
/// `Drop`, guaranteeing the slot is freed even if the task holding it panics or is aborted. See
/// [`Replica::try_claim_dump_slot`].
pub(super) struct BulkDumpCountGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for BulkDumpCountGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Release);
    }
}

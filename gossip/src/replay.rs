// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-peer replay protection for the authenticated modes (AGENTS.md §8).
//!
//! Every authenticated datagram carries a 16-byte replay header (`seq || stamp`, little-endian,
//! ms since epoch) inside the authenticated region. A `seq` already seen or behind the sliding
//! bitmap is rejected, as is a `stamp` deviating from local time by more than
//! [`FRESHNESS_WINDOW_DEFAULT`]. Unauthenticated mode carries no header and is exempt.
//!
//! Three rules the code must keep:
//!
//! - **Replay state outlives membership.** A decommissioned peer keeps its filter entry, or a
//!   captured datagram re-adds it to `members` and re-poisons causal stability. The staleness
//!   purge is sound only because no datagram can raise `stamp_at_max` without being accepted or
//!   triggering `reset`.
//! - **Restart beats regression.** For `seq <= max_seq`, `stamp > stamp_at_max` means a genuine
//!   restart and resets the state; otherwise the bitmap decides. *Residual*: a restart within the
//!   same millisecond is indistinguishable from a replay and is dropped.
//! - **Post-restart tail guard.** `PeerState::max_stamp_seen`, never rewound by `reset`, blocks a
//!   forward-path datagram with a strictly lower stamp; strict `<` because same-millisecond bursts
//!   share a stamp. Relies on [`SenderCounter::next_stamp`]'s in-process floor. *Residual*: a
//!   sender restarting with its clock behind its own stamps is treated as a replay until the clock
//!   catches up.
//!
//! Split across siblings by concern: `wire` owns [`Seq`]/[`Stamp`]'s encoding, ordering and
//! freshness check; `bitmap` owns the sliding out-of-order acceptance window; `peer_state` owns
//! the per-peer accept/restart decision; `sender` owns [`SenderCounter`]'s monotonic issuance;
//! `filter` owns [`ReplayFilter`]'s per-peer map, staleness purge and public entry points. This
//! file keeps the public type definitions (their module location is their `cargo public-api`-
//! visible path — see AGENTS.md §11) plus the shared support (`WINDOW_SIZE`, `phys_now_ms`) every
//! sibling draws on.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;

use peer_state::PeerState;

mod bitmap;
mod filter;
mod peer_state;
mod sender;
mod wire;

/// Length of the replay header prepended to the authenticated portion of every datagram.
///
/// `seq (8 bytes) || stamp (8 bytes)`.
pub const REPLAY_HEADER_LEN: usize = 16;

/// Default freshness window: datagrams whose sender wall-clock stamp deviates from local physical
/// time by more than this value in either direction are rejected.
pub const FRESHNESS_WINDOW_DEFAULT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Size of the out-of-order acceptance bitmap: a `seq` up to this far behind `max_seq` is accepted
/// as legitimate UDP reordering (one bit per relative sequence number); older is rejected.
const WINDOW_SIZE: u64 = 1024;

/// Read the local physical time as milliseconds since the Unix epoch.
fn phys_now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// A per-sender monotonic sequence number carried in the replay header. This module owns its wire
/// encoding and its ordering semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

/// A sender wall-clock stamp (milliseconds since the Unix epoch) carried in the replay header.
///
/// This module owns its wire encoding and its freshness check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp(u64);

/// Sender-side replay state, one per node. `stamp_floor` keeps minted stamps monotonic within the
/// process — the guarantee the receiver's tail guard relies on, lost on restart (module docs).
#[derive(Debug)]
pub struct SenderCounter {
    seq: AtomicU64,
    stamp_floor: AtomicU64,
}

/// Receiver-side per-peer replay filter.
///
/// Entries are purged once `now - stamp_at_max > window`, at which point no replayable datagram
/// could clear the freshness check anyway. `enabled` mirrors the owning
/// [`crate::auth::Authenticator`]'s mode, fixed at construction; a disabled filter accepts
/// everything, so no caller decides whether replay-checking applies.
#[derive(Debug)]
pub struct ReplayFilter {
    peers: Mutex<HashMap<IpAddr, PeerState>>,
    freshness_window: Duration,
    enabled: bool,
}

#[cfg(test)]
mod tests;

// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/// A fresh, real bindable port for a test that needs one but does not care which. `Config::port`
/// must be nonzero — gossip has no per-peer port discovery, so `0` can never converge — but many
/// single-node/no-real-peer tests only used `0` for its other property, an OS-assigned port that
/// never collides with a concurrently running test.
///
/// A process-local counter cannot reproduce that collision-freedom: `cargo nextest` runs every
/// test in its own process, so a `static` counter starts fresh in each one, and two tests in
/// different processes can compute the identical "next" port and race to bind it. Probing the OS
/// for a genuinely free port instead — bind `:0`, read back what the kernel picked, drop the
/// socket — is what `cargo test`'s thread model and `nextest`'s process model both leave free at
/// the moment this returns.
pub(crate) fn next_ephemeral_test_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("OS should hand out an ephemeral port")
        .local_addr()
        .expect("a bound socket reports its own address")
        .port()
}

mod auth_attack;
mod causal_stability;
mod clock_drift;
mod clock_port;
mod coalescing;
mod deadlock_regressions;
mod dump_budget;
mod equal_stamp_redelivery;
mod handle_messages_return_value;
mod immediate_broadcast;
mod in_memory_convergence;
mod pacing;
mod pending_dump_requeue;
mod reserved_wire_tags;
mod socket_buffers;
mod tombstone_ack_bounds;
mod tombstone_ack_resend_counting;

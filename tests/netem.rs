// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Tests for `benches/netem/mod.rs`, the seeded delay/loss/reordering `Transport` decorator the
//! `*_rtt` benchmark lanes are built on.
//!
//! A benchmark's own instrument earns the same tests as the code it measures: a decorator that
//! quietly injected the wrong delay, or lost a datagram it claimed to deliver, would publish a
//! wrong number rather than fail. The module lives under `benches/` because that is what it serves,
//! and is included here because that is where `cargo test` reaches it.
//!
//! The last test is the one `ReplicatedMap::new_with_transport`'s rustdoc has been promising: "a
//! lossy [transport], to test convergence under adversity".

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use reconcile::{
    replicated_map::Config, InMemoryNetwork, InMemoryTransport, ReplicatedMap, Transport,
};

#[path = "../benches/netem/mod.rs"]
mod netem;

use netem::{Impairments, Link, Netem, NetemTransport, Probability, Rtt, Seed};

/// How long a test waits for a datagram the model was not asked to drop. Generous: this is a
/// "rather fail than hang" bound, never a schedule.
const PATIENCE: Duration = Duration::from_secs(5);

/// A fresh address block, so tests running concurrently never collide on an address. Tests that
/// assert on the *seed* need fixed addresses instead — the PRNG stream is per directed link.
fn fresh_addr(last: u8, port: u16) -> SocketAddr {
    static BLOCK: AtomicU32 = AtomicU32::new(1);
    let block = BLOCK.fetch_add(1, Ordering::Relaxed);
    let ip: IpAddr = format!("127.4.{}.{last}", block % 256).parse().unwrap();
    SocketAddr::new(ip, port)
}

/// One in-process network with a netem-wrapped sender and a bare receiver on it. The network is
/// exposed so a test can bind further endpoints.
struct Fixture {
    network: InMemoryNetwork,
    sender: NetemTransport<InMemoryTransport>,
    receiver: InMemoryTransport,
    destination: SocketAddr,
}

impl Fixture {
    fn new(netem: Netem, port: u16) -> Fixture {
        Fixture::between(netem, fresh_addr(1, port), fresh_addr(2, port))
    }

    fn between(netem: Netem, source: SocketAddr, destination: SocketAddr) -> Fixture {
        let network = InMemoryNetwork::new();
        let sender = NetemTransport::new(Arc::new(network.bind(source)), netem);
        let receiver = network.bind(destination);
        Fixture {
            network,
            sender,
            receiver,
            destination,
        }
    }

    async fn send(&self, bytes: &[u8]) {
        self.sender.send_to(bytes, &self.destination).await.unwrap();
    }

    /// Receive one datagram, or fail rather than hang.
    async fn recv(&self) -> Vec<u8> {
        let mut buf = [0u8; 64];
        let (n, _) = tokio::time::timeout(PATIENCE, self.receiver.recv_from(&mut buf))
            .await
            .expect("a datagram the model did not drop must arrive")
            .expect("in-memory receive cannot fail");
        buf[..n].to_vec()
    }
}

#[test]
fn rtt_halves_into_a_one_way_delay() {
    assert_eq!(Rtt::from_millis(50.0).one_way(), Duration::from_millis(25));
    assert_eq!(Rtt::from_millis(0.1).one_way(), Duration::from_micros(50));
    assert_eq!(Rtt::ZERO.one_way(), Duration::ZERO);
}

#[test]
fn labels_are_integer_arithmetic_not_float_formatting() {
    assert_eq!(Rtt::ZERO.label(), "rtt=0ms");
    assert_eq!(Rtt::from_millis(0.1).label(), "rtt=0.1ms");
    assert_eq!(Rtt::from_millis(1.0).label(), "rtt=1ms");
    assert_eq!(Rtt::from_millis(50.0).label(), "rtt=50ms");
    assert_eq!(Probability::percent(0.1).label(), "loss=0.1%");
    assert_eq!(Probability::percent(1.0).label(), "loss=1%");
}

#[test]
fn an_out_of_range_probability_saturates_instead_of_existing() {
    assert_eq!(Probability::percent(-5.0), Probability::ZERO);
    assert_eq!(Probability::percent(f64::NAN), Probability::ZERO);
    assert_eq!(Probability::percent(400.0), Probability::ALWAYS);
    assert_eq!(Rtt::from_millis(-1.0), Rtt::ZERO);
}

#[tokio::test]
async fn a_perfect_link_delivers_every_datagram_in_order() {
    let fixture = Fixture::new(Netem::uniform(Link::PERFECT, Seed::DEFAULT), 7_001);
    for i in 0..16u8 {
        fixture.send(&[i]).await;
    }
    for i in 0..16u8 {
        assert_eq!(fixture.recv().await, vec![i]);
    }
    let impairments = fixture.sender.impairments();
    assert_eq!(impairments.offered(), 16);
    assert_eq!(impairments.dropped(), 0);
}

#[tokio::test]
async fn delivery_waits_out_the_injected_one_way_delay() {
    let rtt = Rtt::from_millis(20.0);
    let fixture = Fixture::new(Netem::uniform(Link::at(rtt), Seed::DEFAULT), 7_002);
    let start = Instant::now();
    fixture.send(b"slow").await;
    // The delay is propagation, not back-pressure: the send returns before the datagram lands.
    assert!(start.elapsed() < rtt.one_way());
    assert_eq!(fixture.recv().await, b"slow");
    assert!(
        start.elapsed() >= rtt.one_way(),
        "delivered after {:?}, inside the {:?} one-way delay",
        start.elapsed(),
        rtt.one_way()
    );
}

/// The 0.1 ms lane is a fifth of tokio's timer resolution, which is why the pump yields out the
/// last millisecond instead of sleeping it. The upper bound is loose on purpose: this asserts the
/// sub-millisecond lane is not silently quantized up to a timer tick, it does not police a
/// schedule.
#[tokio::test]
async fn a_sub_millisecond_delay_is_not_rounded_up_to_the_timer_resolution() {
    let rtt = Rtt::from_millis(0.1);
    let fixture = Fixture::new(Netem::uniform(Link::at(rtt), Seed::DEFAULT), 7_003);
    let start = Instant::now();
    for i in 0..20u8 {
        fixture.send(&[i]).await;
        fixture.recv().await;
    }
    let mean = start.elapsed() / 20;
    assert!(
        mean >= rtt.one_way(),
        "mean delivery {mean:?} is below the injected {:?}",
        rtt.one_way()
    );
    assert!(
        mean < Duration::from_micros(500),
        "mean delivery {mean:?} suggests the 50 µs delay was rounded up to a timer tick"
    );
}

/// Jitter swings the one-way delay uniformly by ±its own width, clamped at zero. Bounds rather
/// than an expected sequence: what has to hold is that the delay genuinely *varies* around the
/// configured one and stays inside the configured envelope — a link that quietly ignored jitter,
/// or overshot it, would be an instrument reporting somebody else's network.
#[tokio::test]
async fn jitter_swings_the_delay_around_the_configured_one() {
    const DATAGRAMS: u32 = 12;
    let delay = Rtt::from_millis(10.0).one_way();
    let fixture = Fixture::new(
        Netem::uniform(
            Link::at(Rtt::from_millis(10.0)).with_jitter(delay),
            Seed::DEFAULT,
        ),
        7_009,
    );
    let (mut shortest, mut longest) = (Duration::MAX, Duration::ZERO);
    for i in 0..DATAGRAMS {
        let start = Instant::now();
        fixture.send(&[i as u8]).await;
        fixture.recv().await;
        let observed = start.elapsed();
        shortest = shortest.min(observed);
        longest = longest.max(observed);
    }
    assert!(
        shortest < delay && longest > delay,
        "delays spanned {shortest:?}..{longest:?}, which does not straddle the configured {delay:?}"
    );
    // The envelope is `delay ± jitter`, so twice the delay here, plus the harness's own overhead.
    assert!(
        longest < delay * 2 + Duration::from_millis(2),
        "delay {longest:?} exceeds the configured envelope of {:?}",
        delay * 2
    );
}

#[tokio::test]
async fn loss_drops_datagrams_at_about_the_configured_rate() {
    const DATAGRAMS: u64 = 4_000;
    let netem = Netem::uniform(
        Link::PERFECT.with_loss(Probability::percent(10.0)),
        Seed::DEFAULT,
    );
    let fixture = Fixture::new(netem, 7_004);
    for _ in 0..DATAGRAMS {
        fixture.send(b"maybe").await;
    }
    let impairments = fixture.sender.impairments();
    assert_eq!(impairments.offered(), DATAGRAMS);
    // ±3 points around 10 %, i.e. ~6 standard deviations at this sample size: this fails on a
    // broken model, not on an unlucky seed.
    assert!(
        (0.07..=0.13).contains(&impairments.loss_fraction()),
        "realized loss {} is not near the configured 10 %",
        impairments.loss_fraction()
    );
}

#[tokio::test]
async fn a_replayed_seed_replays_the_same_losses() {
    // Fixed addresses: the PRNG stream is derived per directed link, so a replay has to be the
    // same link. Each run gets its own network, so reusing the addresses is safe.
    let (source, destination) = (
        "127.5.0.1:7005".parse().unwrap(),
        "127.5.0.2:7005".parse().unwrap(),
    );
    async fn dropped(seed: Seed, source: SocketAddr, destination: SocketAddr) -> u64 {
        let netem = Netem::uniform(Link::PERFECT.with_loss(Probability::percent(25.0)), seed);
        let fixture = Fixture::between(netem, source, destination);
        for _ in 0..500 {
            fixture.send(b"maybe").await;
        }
        fixture.sender.impairments().dropped()
    }
    // Same seed, same link, same datagram sequence — an identical drop count is the property the
    // benchmarks record a seed for.
    let first = dropped(Seed::DEFAULT, source, destination).await;
    assert_eq!(first, dropped(Seed::DEFAULT, source, destination).await);
    assert_ne!(
        first,
        dropped(Seed::new(0xa5a5_a5a5), source, destination).await
    );
}

#[tokio::test]
async fn a_per_destination_override_beats_the_default_link() {
    let far = fresh_addr(3, 7_006);
    let netem = Netem::uniform(Link::PERFECT, Seed::DEFAULT)
        .with_link_to(far.ip(), Link::at(Rtt::from_millis(40.0)));
    let fixture = Fixture::new(netem, 7_006);
    let far_receiver = fixture.network.bind(far);

    // The far peer is sent to first; the near one still answers first, because the links differ.
    let start = Instant::now();
    fixture.sender.send_to(b"far", &far).await.unwrap();
    fixture.send(b"near").await;
    assert_eq!(fixture.recv().await, b"near");
    assert!(start.elapsed() < Duration::from_millis(20));

    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(PATIENCE, far_receiver.recv_from(&mut buf))
        .await
        .expect("the far peer's datagram must still arrive")
        .unwrap();
    assert_eq!(&buf[..n], b"far");
    assert!(start.elapsed() >= Duration::from_millis(20));
}

#[tokio::test]
async fn reordering_lands_a_datagram_behind_one_sent_after_it() {
    let rtt = Rtt::from_millis(20.0);
    let fixture = Fixture::new(
        Netem::uniform(
            Link::at(rtt).with_reorder(Probability::ALWAYS),
            Seed::DEFAULT,
        ),
        7_007,
    );
    // A second sender onto the same receiver, on a plain link of the same delay: a link that
    // displaces *every* datagram is only a slower link, so the overtaking one comes from elsewhere.
    let plain = NetemTransport::new(
        Arc::new(fixture.network.bind(fresh_addr(3, 7_007))),
        Netem::uniform(Link::at(rtt), Seed::DEFAULT),
    );

    fixture.send(b"first").await;
    plain
        .send_to(b"second", &fixture.destination)
        .await
        .unwrap();
    assert_eq!(fixture.recv().await, b"second");
    assert_eq!(fixture.recv().await, b"first");
}

/// Convergence under adversity: two nodes on a delayed, lossy link still reach the same
/// fingerprint. This is what `Transport`'s port-shaped design exists for, and why a dropped
/// datagram is modelled as a successful send — the sender is given no way to tell.
#[tokio::test(flavor = "multi_thread")]
async fn a_replicated_map_converges_over_a_lossy_delayed_link() {
    let network = InMemoryNetwork::new();
    let (a_addr, b_addr) = (fresh_addr(1, 7_008), fresh_addr(2, 7_008));
    // 40 %: high enough that the first attempt at every stage — discovery, the diff round, the
    // value dump — is more likely than not to need repeating, so the test exercises the retry path
    // rather than getting lucky on a clean link.
    let link = Link::at(Rtt::from_millis(2.0)).with_loss(Probability::percent(40.0));
    let store = |addr: SocketAddr| {
        let transport = NetemTransport::new(
            Arc::new(network.bind(addr)),
            Netem::uniform(link, Seed::DEFAULT),
        );
        let impairments = transport.impairments();
        let store = ReplicatedMap::<u32, u32>::new_with_transport(
            Config::default()
                .with_port(addr.port())
                .with_listen_addr(addr.ip())
                .with_net("127.0.0.1/8".parse().unwrap())
                // A lost datagram is repaired by the next anti-entropy round, so this cadence is
                // the test's runtime. The 1 s default would make it a minute-long test.
                .with_reconcile_interval(Duration::from_millis(20))
                .with_insecure_no_key(),
            Arc::new(transport),
        );
        (store, impairments)
    };
    let ((a, a_losses), (b, b_losses)) = (store(a_addr), store(b_addr));
    let entries: Vec<(u32, u32)> = (0..200u32).map(|k| (k, k.wrapping_mul(3))).collect();
    a.insert_bulk(&entries);
    let target = a.fingerprint(..);
    b.seed_peer(a_addr.ip());

    let tasks = [
        tokio::spawn(a.clone().run(CancellationToken::new())),
        tokio::spawn(b.clone().run(CancellationToken::new())),
    ];
    tokio::time::timeout(Duration::from_secs(60), async {
        while b.fingerprint(..) != target {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("anti-entropy must converge over a lossy link, however many rounds it takes");
    let dropped = |losses: &Impairments| losses.dropped();
    assert!(
        dropped(&a_losses) + dropped(&b_losses) > 0,
        "the link was configured to lose 10 % of datagrams but lost none, so this test proved \
         nothing about convergence under loss"
    );

    for task in tasks {
        task.abort();
    }
}

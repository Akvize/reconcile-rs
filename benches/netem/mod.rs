// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A seeded network-emulation decorator over the [`Transport`] port: injected one-way delay,
//! jitter, loss and reordering, configurable per directed link ([#280]).
//!
//! Every other benchmark in this repository runs at RTT ≈ 0, which prices the axis RBSR is good at
//! (bytes) and zeroes the axis it is worst at (`SOTA.md` §1.3: sequential round-trips). This module
//! is the missing instrument, not a fix: it makes the round-trip column cost something.
//!
//! # Why not `turmoil`
//!
//! [#280] asks for `turmoil` to be evaluated first. It is the right tool for a different job:
//!
//! | `turmoil` ([tokio-rs/turmoil]) | what that costs here |
//! |---|---|
//! | time is simulated and advanced a `Builder::tick_duration` at a time (`simulation_duration` is "in simulated time") | Criterion reports wall-clock; a benchmark run inside the simulator would report the simulator's own tick arithmetic — the injected constant, read back |
//! | hosts are futures registered on a `Sim` and driven by `sim.run()` to a fixed simulated duration | Criterion owns the iteration loop, so `iter_custom` has no way to hand its samples to `sim.run()` |
//! | "runs multiple concurrent hosts within a single thread" | `gossip_propagation` deliberately measures N real per-node loops contending on one runtime (`benches/README.md`) |
//! | networking is `turmoil::net`, a drop-in replacement for `tokio::net` | `gossip::UdpTransport` wraps `tokio::net::UdpSocket`, so a `turmoil` lane needs its own [`Transport`] impl **as well as** the simulator — this decorator plus more, not instead of it |
//!
//! So: bespoke, and no new dependency at all ([`rand`] is already a dev-dependency). The knobs
//! below deliberately mirror `turmoil`'s (`min_message_latency`/`max_message_latency`/`fail_rate`),
//! so swapping a `turmoil` lane in later is a substitution rather than a rewrite.
//!
//! # Model
//!
//! Impairment is applied **send-side**, keyed by destination: a datagram is drawn against its
//! link's model, then either dropped or queued for delivery at `now + delay`. A per-node pump task
//! delivers due datagrams in due order to the wrapped transport. Consequences worth knowing:
//!
//! - `send_to` returns immediately, as UDP does — the delay is propagation, not back-pressure.
//! - a drop is `Ok(n)`, not an error: a lost datagram is indistinguishable from a delivered one at
//!   the sender, which is the property the protocol has to survive.
//! - jitter reorders on its own; [`Link::with_reorder`] adds an explicit displacement on top.
//! - delivery order is by `(due, send order)`, so a run is reproducible down to the scheduler.
//!
//! # Determinism
//!
//! Each directed link gets its own PRNG stream, seeded from [`Seed`] mixed with the two endpoint
//! addresses, and every datagram consumes exactly three draws (loss, jitter, reorder) whatever the
//! outcome. So the impairment sequence *per link* replays exactly, given the same datagram
//! sequence on it. What is **not** reproducible bit-for-bit is the interleaving of concurrent tasks
//! on a multi-threaded runtime — record the seed (the benchmarks print it) and read the results as
//! reproducible to within the usual benchmark noise.
//!
//! [#280]: https://github.com/Akvize/reconcile-rs/issues/280
//! [tokio-rs/turmoil]: https://github.com/tokio-rs/turmoil

// This module has two including targets — `benches/system.rs` and `tests/netem.rs` — and each
// exercises a different subset of it (the benchmarks sweep delay and loss; the tests also cover
// jitter, reordering and the per-link overrides). Under `-D warnings` the unused half would fail
// whichever target is being built, so the instrument is allowed to be larger than either caller.
#![allow(dead_code)]

use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use reconcile::Transport;

/// Tokio's timer resolution: [`tokio::time::sleep`] rounds up to the next millisecond. The pump
/// therefore sleeps to within this much of a delivery and yields out the remainder — without it the
/// 0.1 ms lane would measure the timer wheel rather than the link.
const TIMER_RESOLUTION: Duration = Duration::from_millis(1);

/// A probability in `[0, 1]`.
///
/// Saturating rather than fallible, as `rbsr::FanOut::new` is: an out-of-range or NaN input is
/// clamped at construction, so an invalid instance cannot exist and no call site has to check.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Probability(f64);

impl Probability {
    /// Never.
    pub const ZERO: Probability = Probability(0.0);

    /// Always.
    pub const ALWAYS: Probability = Probability(1.0);

    /// A percentage — `Probability::percent(0.1)` is one datagram in a thousand. Percent rather
    /// than a bare fraction because that is the unit loss is quoted in.
    pub fn percent(percent: f64) -> Probability {
        // `f64::max` returns the non-NaN operand, so this also maps NaN to zero.
        Probability((percent.max(0.0) / 100.0).min(1.0))
    }

    /// The probability as a fraction of one.
    pub fn as_fraction(self) -> f64 {
        self.0
    }

    /// The Criterion benchmark-id parameter: `loss=0.1%`, `loss=1%`. Tenths of a percent in
    /// integer arithmetic, for the same reason as [`Rtt::label`].
    pub fn label(self) -> String {
        let tenths = (self.0 * 1_000.0).round() as u64;
        if tenths.is_multiple_of(10) {
            format!("loss={}%", tenths / 10)
        } else {
            format!("loss={}.{}%", tenths / 10, tenths % 10)
        }
    }

    /// Draw once. Always consumes exactly one value from `rng`, so a link's stream position does
    /// not depend on its outcomes.
    fn hits(self, rng: &mut StdRng) -> bool {
        rng.gen_bool(self.0)
    }
}

/// A round-trip time.
///
/// The decorator injects **one-way** delay in each direction, and a sweep is stated in RTT because
/// that is what an operator measures. Owning the halving on the type is the whole point of it:
/// confusing the two is a silent factor of two in every number this instrument produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rtt(Duration);

impl Rtt {
    /// The loopback lane: what every other benchmark in this repository runs at.
    pub const ZERO: Rtt = Rtt(Duration::ZERO);

    /// A round-trip time in milliseconds. Fractional, so the sweep can reach 0.1 ms; negative and
    /// NaN saturate to zero.
    pub fn from_millis(millis: f64) -> Rtt {
        Rtt(Duration::from_secs_f64(millis.max(0.0) / 1_000.0))
    }

    /// The one-way propagation delay injected in each direction — half the round trip.
    pub fn one_way(self) -> Duration {
        self.0 / 2
    }

    /// The Criterion benchmark-id parameter: `0ms`, `0.1ms`, `50ms`. Integer arithmetic on
    /// microseconds, so the label is identical on every machine (a float format is not).
    pub fn label(self) -> String {
        let micros = self.0.as_micros();
        let (millis, tenths) = (micros / 1_000, (micros % 1_000) / 100);
        if tenths == 0 {
            format!("rtt={millis}ms")
        } else {
            format!("rtt={millis}.{tenths}ms")
        }
    }
}

/// The seed of an emulation run. Record it with the results: it is what makes the losses replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seed(u64);

impl Seed {
    /// The seed the benchmarks run at, absent a reason to vary it.
    pub const DEFAULT: Seed = Seed(0x5eed_0280);

    pub fn new(seed: u64) -> Seed {
        Seed(seed)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// The impairment applied to one directed link.
///
/// Directed: `a → b` and `b → a` are configured separately, which is what makes an asymmetric or
/// geographic topology expressible (`Config::remote_interval`/`remote_fanout` model the same split
/// on the protocol side).
#[derive(Clone, Copy, Debug)]
pub struct Link {
    delay: Duration,
    jitter: Duration,
    loss: Probability,
    reorder: Probability,
}

impl Link {
    /// A perfect link: no delay, no loss. The RTT-≈-0 lane, i.e. what every other benchmark in
    /// this repository measures — still routed through the pump, so it prices the harness itself
    /// and every other lane's delta is against it rather than against a different code path.
    pub const PERFECT: Link = Link {
        delay: Duration::ZERO,
        jitter: Duration::ZERO,
        loss: Probability::ZERO,
        reorder: Probability::ZERO,
    };

    /// A link whose one-way delay is half of `rtt`.
    pub fn at(rtt: Rtt) -> Link {
        Link {
            delay: rtt.one_way(),
            ..Link::PERFECT
        }
    }

    /// Uniform swing of ±`jitter` around the one-way delay, clamped at zero. Jitter reorders on its
    /// own: a datagram drawn short overtakes one drawn long.
    pub fn with_jitter(mut self, jitter: Duration) -> Link {
        self.jitter = jitter;
        self
    }

    /// Per-datagram drop probability.
    pub fn with_loss(mut self, loss: Probability) -> Link {
        self.loss = loss;
        self
    }

    /// Per-datagram probability of an extra one-way delay's displacement, which lands the datagram
    /// behind those sent after it. Relative to the link's own propagation time, so a zero-delay
    /// link cannot be reordered — on such a link there is no flight to be overtaken in.
    pub fn with_reorder(mut self, reorder: Probability) -> Link {
        self.reorder = reorder;
        self
    }

    /// Draw this datagram's fate. Exactly three draws, whatever the outcome, so a link's stream
    /// position is a function of how many datagrams crossed it and nothing else.
    fn draw(self, rng: &mut StdRng) -> Option<Duration> {
        let lost = self.loss.hits(rng);
        let swing: f64 = rng.gen_range(-1.0..=1.0);
        let reordered = self.reorder.hits(rng);
        if lost {
            return None;
        }
        let offset = self.jitter.mul_f64(swing.abs());
        let jittered = if swing < 0.0 {
            self.delay.saturating_sub(offset)
        } else {
            self.delay + offset
        };
        Some(if reordered {
            jittered + self.delay
        } else {
            jittered
        })
    }
}

/// One node's view of the network: a default link to every peer, plus per-destination overrides.
#[derive(Clone, Debug)]
pub struct Netem {
    default_link: Link,
    per_destination: HashMap<IpAddr, Link>,
    seed: Seed,
}

impl Netem {
    /// The same `link` to every destination.
    pub fn uniform(link: Link, seed: Seed) -> Netem {
        Netem {
            default_link: link,
            per_destination: HashMap::new(),
            seed,
        }
    }

    /// Override the link to one destination — the seam an asymmetric or geographic topology is
    /// built from (a node with two far peers and six near ones is six calls).
    pub fn with_link_to(mut self, destination: IpAddr, link: Link) -> Netem {
        self.per_destination.insert(destination, link);
        self
    }

    fn link_to(&self, destination: &SocketAddr) -> Link {
        self.per_destination
            .get(&destination.ip())
            .copied()
            .unwrap_or(self.default_link)
    }
}

/// Mix a seed and both endpoints into a distinct PRNG stream per directed link.
///
/// Explicit SplitMix64 rather than `DefaultHasher`, whose output is documented as unstable across
/// Rust releases — these benchmarks are supposed to be reproducible across machines and over time
/// (`benches/README.md`), and a seed that only holds within one toolchain is not a seed.
fn stream_seed(seed: Seed, source: SocketAddr, destination: SocketAddr) -> u64 {
    fn mix(state: &mut u64, value: u64) {
        *state = state
            .wrapping_add(value)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        *state = z ^ (z >> 31);
    }
    fn endpoint(state: &mut u64, addr: SocketAddr) {
        match addr.ip() {
            IpAddr::V4(v4) => mix(state, u32::from(v4) as u64),
            IpAddr::V6(v6) => {
                let bits = u128::from(v6);
                mix(state, (bits >> 64) as u64);
                mix(state, bits as u64);
            }
        }
        mix(state, addr.port() as u64);
    }
    let mut state = seed.get();
    endpoint(&mut state, source);
    endpoint(&mut state, destination);
    state
}

/// A datagram in flight, ordered by when it is due and, for a tie, by send order.
struct Pending {
    due: Instant,
    seq: u64,
    destination: SocketAddr,
    bytes: Vec<u8>,
}

impl Ord for Pending {
    fn cmp(&self, other: &Pending) -> Ordering {
        self.due.cmp(&other.due).then(self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Pending) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Pending {
    fn eq(&self, other: &Pending) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Pending {}

/// The in-flight queue, shared between [`NetemTransport::send_to`] and the pump task.
#[derive(Default)]
struct InFlight {
    /// `Reverse`, so the [`BinaryHeap`] pops the *earliest* due datagram.
    queue: Mutex<BinaryHeap<Reverse<Pending>>>,
    wake: Notify,
}

/// What the model did to this node's outbound traffic — the instrument's own accounting, so a lane
/// can assert it got the loss rate it asked for instead of trusting it.
#[derive(Clone, Debug, Default)]
pub struct Impairments {
    offered: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl Impairments {
    /// Datagrams handed to [`Transport::send_to`].
    pub fn offered(&self) -> u64 {
        self.offered.load(AtomicOrdering::Relaxed)
    }

    /// Datagrams the loss model swallowed.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(AtomicOrdering::Relaxed)
    }

    /// The realized loss fraction, or `0.0` before anything was sent.
    pub fn loss_fraction(&self) -> f64 {
        let offered = self.offered();
        if offered == 0 {
            0.0
        } else {
            self.dropped() as f64 / offered as f64
        }
    }
}

/// A [`Transport`] decorator that delays, drops and reorders what the wrapped one sends.
///
/// Construct inside a Tokio runtime: it spawns the per-node delivery pump, which is aborted when
/// the transport drops.
pub struct NetemTransport<T> {
    inner: Arc<T>,
    netem: Netem,
    local: SocketAddr,
    /// One PRNG per directed link, created on first use and advanced in send order.
    streams: Mutex<HashMap<SocketAddr, StdRng>>,
    in_flight: Arc<InFlight>,
    seq: AtomicU64,
    impairments: Impairments,
    pump: JoinHandle<()>,
}

impl<T: Transport<Addr = SocketAddr>> NetemTransport<T> {
    /// Wrap `inner` in `netem`.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime, or if `inner` has no local address (neither is reachable
    /// from the benchmarks, which bind an `InMemoryTransport` first).
    pub fn new(inner: Arc<T>, netem: Netem) -> NetemTransport<T> {
        let local = inner
            .local_addr()
            .expect("a netem-wrapped transport must already be bound");
        let in_flight = Arc::new(InFlight::default());
        let pump = tokio::spawn(pump(Arc::clone(&inner), Arc::clone(&in_flight)));
        NetemTransport {
            inner,
            netem,
            local,
            streams: Mutex::new(HashMap::new()),
            in_flight,
            seq: AtomicU64::new(0),
            impairments: Impairments::default(),
            pump,
        }
    }

    /// What the model has done to this node's outbound traffic so far.
    pub fn impairments(&self) -> Impairments {
        self.impairments.clone()
    }
}

impl<T> Drop for NetemTransport<T> {
    fn drop(&mut self) {
        // The pump owns an `Arc` of the wrapped transport and outlives nothing: a benchmark that
        // rebuilds its cluster once per iteration would otherwise accumulate one live task per
        // node per sample.
        self.pump.abort();
    }
}

#[async_trait::async_trait]
impl<T: Transport<Addr = SocketAddr>> Transport for NetemTransport<T> {
    type Addr = SocketAddr;

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // Reception is untouched: impairment is applied once per datagram, on the sending side,
        // where the link's direction is known from the destination.
        self.inner.recv_from(buf).await
    }

    async fn send_to(&self, buf: &[u8], destination: &SocketAddr) -> io::Result<usize> {
        let link = self.netem.link_to(destination);
        let delay = {
            let mut streams = self.streams.lock();
            let stream = streams.entry(*destination).or_insert_with(|| {
                StdRng::seed_from_u64(stream_seed(self.netem.seed, self.local, *destination))
            });
            link.draw(stream)
        };
        self.impairments
            .offered
            .fetch_add(1, AtomicOrdering::Relaxed);
        let Some(delay) = delay else {
            // A dropped datagram is a successful send: UDP gives the sender no other answer, and
            // that indistinguishability is exactly what the protocol has to survive.
            self.impairments
                .dropped
                .fetch_add(1, AtomicOrdering::Relaxed);
            return Ok(buf.len());
        };
        self.in_flight.queue.lock().push(Reverse(Pending {
            due: Instant::now() + delay,
            seq: self.seq.fetch_add(1, AtomicOrdering::Relaxed),
            destination: *destination,
            bytes: buf.to_vec(),
        }));
        self.in_flight.wake.notify_one();
        Ok(buf.len())
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// What the pump does next, decided under the queue lock and acted on outside it.
enum Step {
    Deliver(Pending),
    WaitUntil(Instant),
    Idle,
}

/// Deliver queued datagrams in due order, one node's worth.
async fn pump<T: Transport<Addr = SocketAddr>>(inner: Arc<T>, in_flight: Arc<InFlight>) {
    loop {
        let step = {
            let mut queue = in_flight.queue.lock();
            match queue.peek().map(|Reverse(head)| head.due) {
                Some(due) if due <= Instant::now() => {
                    Step::Deliver(queue.pop().expect("just peeked").0)
                }
                Some(due) => Step::WaitUntil(due),
                None => Step::Idle,
            }
        };
        match step {
            // A send error is what a real one is here: the datagram is gone, and the protocol is
            // required to tolerate that (`Replica::run` logs and counts, never fails).
            Step::Deliver(datagram) => {
                let _ = inner.send_to(&datagram.bytes, &datagram.destination).await;
            }
            Step::Idle => in_flight.wake.notified().await,
            Step::WaitUntil(due) => wait_until(due, &in_flight.wake).await,
        }
    }
}

/// Wait for `due`, or for a nearer datagram to arrive.
///
/// Sleeps to within [`TIMER_RESOLUTION`] and yields out the rest: tokio rounds a sleep up to the
/// next millisecond, which is five times the entire one-way delay of the 0.1 ms lane.
async fn wait_until(due: Instant, wake: &Notify) {
    if let Some(coarse) = due.checked_sub(TIMER_RESOLUTION) {
        if coarse > Instant::now() {
            tokio::select! {
                _ = tokio::time::sleep_until(coarse) => {}
                // Something nearer-due may have been queued behind us; re-decide.
                _ = wake.notified() => return,
            }
        }
    }
    while Instant::now() < due {
        tokio::task::yield_now().await;
    }
}

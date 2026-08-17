# Benchmarks

Three Criterion targets, all `harness = false`, none feature-gated:

| Target | What it measures |
|---|---|
| `bench` | `FingerprintTreeMap` micro-benchmarks (fill, single insert/remove, cumulated range-fingerprint) vs `BTreeMap`, plus the dated-vs-value-only fill and the single-difference `ReplicatedMap` send/reconcile latency. |
| `system` | End-to-end, **public-API** system benchmarks (below), including the injected-RTT/loss lane. |
| `protocol` | Wire *and* local cost of one full RBSR reconciliation, **per refinement policy** — total wire bytes at four value sizes, then messages, advertised ranges, refinement bytes, datagrams, IP fragments, IDLIST elements and RSOS query counts, as a function of store size `n`, difference size `d`, and how the differences cluster (below). |

No target runs in CI — CI only *compile-checks* them (`RUSTFLAGS='--cfg reconcile_internal_testing' cargo bench --no-run`). Run them locally when you want numbers.

## Running the system benchmarks

```sh
# Everything (point-read, memory, bulk-load, cold-sync, gossip fan-out/propagation, the
# injected-RTT/loss lane, durable-rejoin):
cargo bench --bench system

# A single benchmark or size (Criterion treats the argument as a regex over the benchmark id):
cargo bench --bench system -- point_read
cargo bench --bench system -- 'cold_sync/1000'
cargo bench --bench system -- 'gossip_fanout/64'

# A fast pass while iterating (lower statistical confidence):
cargo bench --bench system -- --quick
```

Criterion writes HTML reports and raw CSV under `target/criterion/`; open `target/criterion/report/index.html`. Every corpus is seeded deterministically, so runs are comparable across machines and over time.

## What each benchmark covers

- **`point_read`** — `ReplicatedMap::get` latency vs `HashMap` and `BTreeMap` across dataset sizes. The store walks the B-tree (`O(log n)`), so this quantifies the read-path cost against the flat-map baselines (drives #171).
- **`memory_footprint`** — prints the fixed per-entry footprint of the dated cell `Entry<Timestamp, V>` vs the value-only mirror projection `State<V>` across value payload sizes; the delta is the mirror's per-entry saving (drives #170). Printed, not timed.
- **`bulk_load`** — `insert_bulk` throughput (entries/s) filling an empty store, across sizes (drives #173).
- **`cold_sync`** — wall time for an **empty** node to converge with a **full** one purely via anti-entropy: the full node is pre-loaded before it has any peer (nothing is broadcast eagerly), then the empty node seeds it and pulls the whole dataset through the range-diff protocol; timed until fingerprints match (drives #168). Loopback, i.e. RTT ≈ 0; `cold_sync_rtt` below prices the difference (**+1.0 × RTT**, flat in `N`).
- **`gossip_fanout`** — bytes/datagrams *one node* sends for a single write, as peer count `N` grows (`2..128`, full-mesh-seeded, on an in-process `InMemoryNetwork` — no real sockets). `Replica::broadcast` (`src/replica.rs`) sends every local write to **all** known peers with no bound (only the separate, periodic WAN anti-entropy round is capped by `remote_fanout`), so this is expected, and confirmed, to be O(N) per node. Prints the exact datagram/byte count per write (deterministic, like `memory_footprint`) alongside the timed send-loop cost (drives #174's scaling gap).
- **`gossip_propagation`** — wall time from a write on one node to **every** other node observing it, as `N` grows (`2..32`, smaller range than `gossip_fanout` — see the caveat below). Unlike `gossip_fanout`, every node runs its real receive/reconcile loop throughout: the steady-state counterpart to `cold_sync`'s from-scratch convergence (drives #174's scaling gap). Also RTT ≈ 0; `gossip_propagation_rtt` prices that (**+0.5 × RTT** — one hop, not a chain).
- **`netem_calibration`**, **`cold_sync_rtt`**, **`gossip_propagation_rtt`** — the injected-RTT/loss lane. Its own section below.
- **`durable_rejoin`** — time to reload an `N`-entry `FileSnapshot` from disk, i.e. the cost a restarting node pays before rejoining (drives #172).

## Gossip-scaling benchmark caveats

`gossip_fanout` and `gossip_propagation` simulate `N` nodes as in-process tasks sharing one Tokio
runtime and one OS thread pool, communicating over `InMemoryNetwork` rather than real UDP sockets —
deliberately: real sockets risk port exhaustion and self-inflicted loopback packet loss/reordering
at higher `N`, which would corrupt the traffic/latency measurement with retransmit noise unrelated to
the protocol. The tradeoff is that past a few dozen–hundred simulated peers, the benchmark
increasingly measures its own scheduler and lock contention (`peers`/`map` `RwLock`s) rather than
genuine network behavior — `gossip_propagation`'s `N` range is kept smaller than `gossip_fanout`'s for
exactly this reason (every node runs a live loop; `gossip_fanout` only exercises the send path).
Nodes are full-mesh-seeded via `seed_peer` up front rather than left to discover peers through gossip,
so both benchmarks isolate fan-out/propagation cost from peer-discovery convergence time (already
covered separately by `cold_sync`).

`gossip_propagation`'s write key is a counter that must live **outside** the routine Criterion calls
per sample. Declared inside it, it restarts at zero every sample, so every sample after the first
re-writes a key the cluster already holds, completes immediately, and reports the poll loop — which
is what it did until the RTT lane made the discrepancy visible (a 25 ms one-way link that measured
4 µs). Numbers from before that fix are not comparable.

No comparable open-source gossip/SWIM library surveyed for this
(chitchat, foca, memberlist) ships an automated, reproducible N-node scaling benchmark in-repo — the
closest real precedent is HashiCorp's one-off [Consul 66k-node scale test](https://www.hashicorp.com/en/blog/consul-scale-test-report-to-observe-gossip-stability),
a real-hardware exercise, not a runnable harness. These two benchmarks are closer to establishing a
methodology than following one.

## The injected-RTT / loss lane

```sh
cargo bench --bench system -- netem                    # calibration report only
cargo bench --bench system -- cold_sync_rtt
cargo bench --bench system -- gossip_propagation_rtt
```

Every other benchmark in this repository runs at RTT ≈ 0 and loss = 0, which prices the axis RBSR is
good at (bytes) and zeroes the one it is worst at (`SOTA.md` §1.3: sequential round-trips). These
three lanes are the instrument that answer [#280](https://github.com/Akvize/reconcile-rs/issues/280).

`benches/netem/mod.rs` is a seeded `Transport` decorator — one-way delay, jitter, loss, reordering,
configurable per **directed** link — over the same `InMemoryNetwork` `gossip_propagation` uses. Its
module docs carry the model, the determinism guarantee and why it is bespoke rather than
[`turmoil`](https://github.com/tokio-rs/turmoil) (short version: turmoil's clock is simulated and
tick-quantized, so a Criterion sample inside it would report the simulator's arithmetic, and its
`turmoil::net` shim would still need a `Transport` impl on top). No new dependency; `tests/netem.rs`
tests it, including convergence over a 40 %-loss link.

**Both `rtt=0ms` columns below are the same harness with a perfect link, not the loopback
benchmarks** — the decorator, the pump and the in-memory fabric are in every lane, so a delta is the
injected network and nothing else. Cross-harness it is not comparable: `cold_sync/1000` on real
loopback UDP is 2.18 ms against `cold_sync_rtt/n=1000/rtt=0ms`'s 0.98 ms.

### Calibration

`netem_calibration` prints this before every run; read it first, because a delay lane that silently
quantized to tokio's 1 ms timer resolution would look like a protocol result. Measured on a 4-core
Xeon @ 2.10 GHz, seed `0x5eed0280`:

| lane | injected one-way | observed mean | lane | configured | realized |
|---|---:|---:|---|---:|---:|
| `rtt=0ms` | 0 | 39 µs | `loss=0.1%` | 0.100 % | 0.110 % |
| `rtt=0.1ms` | 50 µs | 70 µs | `loss=1%` | 1.000 % | 0.940 % |
| `rtt=1ms` | 500 µs | 536 µs | | | |
| `rtt=10ms` | 5 ms | 5.22 ms | | | |
| `rtt=50ms` | 25 ms | 25.30 ms | | | |

So the harness floor is ≈ 39 µs per one-way delivery and the injected delay is faithful from 50 µs
up. That floor is the resolution of every number below.

### Results: what RTT ≈ 0 was hiding

| benchmark | rtt=0ms | 0.1 ms | 1 ms | 10 ms | 50 ms | delta vs rtt=0 |
|---|---:|---:|---:|---:|---:|---|
| `cold_sync_rtt/n=1000` | 984 µs | 1.07 ms | 2.05 ms | 11.50 ms | 51.50 ms | **+1.01 × RTT** (52× at 50 ms) |
| `cold_sync_rtt/n=10000` | 11.53 ms | 11.48 ms | 12.48 ms | 22.38 ms | 62.48 ms | **+1.02 × RTT** (5.4× at 50 ms) |
| `gossip_propagation_rtt/N=8` | 103 µs | 140 µs | 595 µs | 5.33 ms | 25.69 ms | **+0.50 × RTT** (250× at 50 ms) |

Both deltas are constants in RTT, and both are integer numbers of one-way hops:

- **A write propagates in one hop, not a chain.** `Replica::broadcast` sends every local write to
  every known peer directly, so `gossip_propagation` costs exactly RTT/2 — flat in `N`, and
  unaffected by `SOTA.md`'s O(log n)-rounds argument, which is about *reconciliation*, not gossip.
- **Cold sync costs exactly one round trip, whatever the dataset size.** Not O(log n): an empty peer
  has nothing to refine. Its outer range differs, the answer is the whole dataset, and the exchange
  is two one-way hops — the empty node's initial ranges out, the values back. The `n=1000` and
  `n=10000` rows add the same 1 × RTT to very different baselines.
- **So `cold_sync` never exercised the O(log n) refinement chain**, at any RTT. That chain runs when
  the difference is *small relative to the store* — the regime RBSR exists for. `benches/protocol.rs`
  counts its rounds, and the rows above are what converts that count into seconds: **one protocol
  round trip costs one RTT, measured, with no hidden multiplier**. Refinement depth, message count,
  round trips and the resulting wall clock at 50 ms RTT, across n = 10³…10⁶ at `b` = 16 (`d` = 1, one
  element missing; `SOTA.md` §2.2 draws the "worst family on latency" conclusion from this table
  without repeating the numbers):

  | quantity | n = 10³ | 10⁴ | 10⁵ | 10⁶ |
  |---|---:|---:|---:|---:|
  | `⌈log₁₆ n⌉` — refinement depth, the model | 3 | 4 | 5 | 5 |
  | one-way messages, measured — incl. the opening exchange and the closing item transfer | 6 | 6 | 6 | 8 |
  | round trips = half the row above | 3 | 3 | 3 | **4** |
  | wall clock at 50 ms RTT | 150 ms | 150 ms | 150 ms | **200 ms** |

  A single missing element in a 10⁶-entry store is therefore ~4 × RTT — ~200 ms at 50 ms RTT —
  against 3.8 kB of traffic.
- Pricing that end-to-end rather than by composition needs a difference the two peers disagree on
  *without* disagreeing on timestamps, which only `just_insert`/`just_remove` can build. Those are
  `reconcile_internal_testing`-gated seams, and `system.rs` is deliberately gate-free — so that lane
  belongs next to `service_reconcile` in the `bench` target, not here.

### Results: loss, at `rtt=1ms`

| benchmark | no loss | loss=0.1% | loss=1% |
|---|---:|---:|---:|
| `cold_sync_rtt/n=1000` | 2.05 ms | 1.97 ms | 20.5 ms |
| `cold_sync_rtt/n=10000` | 12.48 ms | 16.2 ms | 112 ms |
| `gossip_propagation_rtt/N=8` | 595 µs | 6.55 ms | 100 ms |

**A lost datagram costs a full `reconcile_interval`, not a retransmit.** There is no retransmission:
the exchange is only repaired by the next anti-entropy round, so the penalty is the cadence —
`Config::reconcile_interval`, 1 s by default. Every cell above is that mechanism and nothing else:
the mean is `P(any datagram of the exchange lost) × ~1 s`, which for `gossip_propagation_rtt`'s 7
receivers is `1 − 0.99⁷ ≈ 6.8 %` → ~100 ms (measured 100 ms), and at 0.1 % is `≈ 0.7 %` → ~7 ms
(measured 6.55 ms). Hence the wide confidence intervals: the distribution is bimodal, not noisy.

Operationally: **on a lossy path, `reconcile_interval` is the latency knob, not RTT.** 1 s of it
dwarfs the 50 ms top of the RTT sweep by 20×.

Every lane is seeded (`Seed::DEFAULT`, printed with the results) and the impairment stream per
directed link replays exactly; what does not replay bit-for-bit is task interleaving on a
multi-threaded runtime, so read these as reproducible to within the usual benchmark noise. Like
every target here, the lane stays out of CI — compile-checked only.

## The `protocol` benchmark

```sh
cargo bench --bench protocol            # cost tables + the timed drive loops
cargo bench --bench protocol -- --quick

# The printed tables only (no timed group): give Criterion a filter that matches no benchmark id.
cargo bench --bench protocol -- 'no_such_benchmark'
```

`reconciliation_cost` prints, for each `(policy, n, d, clustering)`, **total wire bytes** — the
refinement traffic plus the values the IDLIST outcomes ship — at four value payload sizes
(8 / 64 / 512 / 4096 B), then the breakdown under it: refinement bytes (bincode, the same encoder
the real transport uses), advertised `RangeAggregate`s, one-way messages, datagrams, IP fragments,
the largest single message, the IDLIST ranges and the elements they ship, and the local
`Aggregate`/`Rank`/`Select` counts summed over both peers. Deterministic, so it is printed rather
than timed — like `system`'s `memory_footprint`, whose payload-size axis it borrows. The timed
`reconciliation_drive` group alongside it measures the local CPU cost of driving a whole run per
policy, the quantity arXiv:2603.19820 models as `T_loc`.

**One unit, because the two halves are traded against each other.** A policy that splits less
advertises fewer ranges but reaches its IDLIST cutoff on wider ranges, and every enumerated element
is a *value* on the wire — nearly all of them elements the peer already holds. Reported in two
units, that trade is unreadable: at n = 10⁵, d = 100 scattered the paper's `t`=32 policy saves 46 %
of the refinement bytes against the default and ships **5 036 elements instead of 100**, which looks
like a win in the first column and a loss in the second. Summed, it is 1.52× the default's bytes at
8-byte values and 36× at 4 KB. The message column stays separate on purpose: no byte total prices a
round trip, and *this* target runs at RTT ≈ 0 — weigh it at the measured rate of **1.00 × RTT per
round trip** ([#280](https://github.com/Akvize/reconcile-rs/issues/280) — the injected-RTT lane
above).

One drive prices every value size: the payload is not read by any SKIP/IDLIST/SPLIT decision, so
only the per-element price moves with it. The harness checks that rather than assuming it —
`payload_size_does_not_move_the_trace` reconciles the same case over `u64`, 8-byte and 4 KB values
and compares every decision before a table is printed.

**Why it exists.** RBSR's published bounds — `O(d log n)` communication, `O(log n)` sequential
rounds — assume the fixed branching factor `b` of the paper's Algorithm 2. `rbsr` makes the fan-out
a swappable `RefinementPolicy`, so what a given configuration costs is a measurement rather than a
quotation: this target supplies it. Interpretation of the numbers below lives in `SOTA.md` §2.2; the
decisions they drove are on issues [#257](https://github.com/Akvize/reconcile-rs/issues/257) and
[#315](https://github.com/Akvize/reconcile-rs/issues/315), and in `rbsr/src/policy.rs`'s own rustdoc.

Refinement bytes and one-way messages for `SqrtFanOut` (`√m`) against the default `FixedFanOut(16)`
and the paper's `t`=32 enumeration threshold, same harness, `d` = 1, one element missing (refinement
traffic only — the first two ship one ≈33 B element on top of it; `t`=32 ships 7–51 and is why the
totals below differ):

| n | `√m` refine B | `√m` msgs | `b`=16 refine B | `b`=16 msgs | `t`=32/`b`=16 refine B | msgs |
|---:|---:|---:|---:|---:|---:|---:|
| 10³ | 2 041 | 6 | 1 701 | 6 | 1 476 | 4 |
| 10⁴ | 5 395 | 8 | 2 195 | 6 | 1 520 | 4 |
| 10⁵ | 16 553 | 6 | 2 789 | 6 | 2 294 | 5 |
| 10⁶ | **53 046** | 8 | **3 834** | 8 | **3 246** | 6 |

For a **single** missing element in a 10⁶-entry store, `b = 16` spends 3.8 kB over 78 ranges against
`SqrtFanOut`'s ~53 kB over ~1 048 ranges (~×3.2 per decade of n on the `√m` bytes), at the *same* 8
one-way messages — log₁₆ 10⁶ ≈ 5 is already the iterated-square-root depth at that size, so the
`Θ(log log n)` round advantage `√m` offers asymptotically does not appear below n ≈ 10¹² (the message
counts above are identical at n = 10⁶: 8 and 8). Local query cost scales with it: ~13× the
`Aggregate`/`Rank`/`Select` queries at n = 10⁶ (2 094/2 092/1 040 against 155/152/70). The gap closes
as `d` grows and scatters — at `d` = 100 over 10⁶ elements the two are within 7 % (270 940 B against
253 153 B), because ~√n ranges stop being overhead once the difference genuinely needs that many;
`√m` is worst exactly in the small-`d` regime RBSR exists for.

The timed `reconciliation_drive` group widens the gap further, in a column no RTT caveat touches:
2.10 ms under `√m` against 45.0 µs at `b` = 16 (≈47×) at n = 10⁶, 460 µs against 25.2 µs at 10⁵, and
only 1.6× apart at 10³ — steeper than the query-count ratio because a `√n` fan-out's queries are
individually dearer (wide `Aggregate`s, spread-out `Select`s touch far more of the tree than a narrow
descent).

The widest single round at d = 1 is 50 781 B (inside the 65 507-byte datagram ceiling, ~35 IP
fragments at a 1500-byte MTU, any one of which loses the whole round); at d = 100 over 10⁶ elements
it reaches 160 908 B over 3 300 ranges, i.e. three datagrams and ~189 fragments —
`send_messages_paced` chunks past the ceiling rather than failing.

`fan_out_sweep` then varies the branching factor alone (`FixedFanOut`, `b` = 2…256). Bytes and local
work follow `b / ln b`, which is derived rather than fitted: refinement advertises `b` aggregates per
level over `log_b n = ln n / ln b` levels, so `refinement ≈ aggregate_size · ln n · (b / ln b)`, whose
derivative `(ln b − 1)/(ln b)²` vanishes at `b = e ≈ 2.718` — `b` = 3 over the integers, with `b` = 2
and `b` = 4 tied above it (2.885 each). Earlier sweeps ran powers of two and stepped over it;
`FAN_OUTS` now carries `b` = 3. At `n = 10⁶`, `d = 1` scattered:

| `b` | 2 | **3** | 4 | 8 | 16 | 32 | 64 | 128 | 256 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| refinement B | 2 061 | **1 868** | 1 960 | 2 613 | 3 834 | 5 021 | 9 668 | 15 856 | 25 880 |
| ranges, measured | 42 | **38** | 40 | 53 | 78 | 101 | 196 | 320 | 520 |
| `b · ln n / ln b`, predicted | 39.9 | **37.7** | 39.9 | 53.2 | 79.7 | 127.6 | 212.6 | 364.5 | 637.8 |
| one-way messages | 22 | 14 | 12 | 10 | 8 | 6 | 6 | 6 | 6 |

`b` = 3 is the minimum, 4.7 % under `b` = 4 and 9.4 % under `b` = 2. The range row is the sharper
test — the model predicts an absolute count with **no fitted constant** — and it holds to ~2 ranges
through `b` = 16. Past that it over-predicts, because the descent bottoms out before the last level
can use its full fan-out; that truncation is also why the implied byte constant drifts (≈ 680 at
`b` = 3…8, ≈ 560 at 256) while the small-`b` end does not.

One-way messages fall as `log_b n` until they hit a floor — 6 at `n = 10⁶`, reached at
`b = 32` — past which extra `b` is paid for and buys nothing. The widest single round grows linearly
in `b` and is the hard ceiling: at `n = 10⁵`, `d = 100` it already exceeds one datagram at `b = 16`.
Across every measured `(n, d, clustering)`, `b = 16` is the only swept value never worse than `√m` on
rounds, while spending 13.8× fewer bytes, ~45× less `T_loc` and a 63× narrower widest round than
`√m`; `b = 4` wins on bytes and CPU but costs two round-trips — break-even at an RTT of ≈8 µs at
1 Gb/s, i.e. only worth it when the "network" is in-process. It is the bandwidth-over-latency value
to reach for.

`threshold_sweep` does the same for the other Algorithm 1 parameter, the enumeration threshold
(`EnumerateBelowThreshold`, `t` = 1…256, `b` held at 16), against `FixedFanOut(16)` — today's
default, and the only baseline the question "should `t` exist at all?" can be answered against. Each
row carries its total as a ratio to that baseline at every value size, plus its **break-even**: the
element price at which the refinement it saves would exactly pay for the elements it ships. Read
that against the element price printed above the tables — the same unit, measured the same way. `t`
is a step function, not a dial: a range's span walks the ladder `n / b^k`, so every `t` between two
rungs picks the same rung and costs exactly the same. At n = 10⁵, d = 100 scattered, `t` = 32 saves
46 % of the refinement bytes (88 817 B against 162 993 B) and ships 5 036 elements instead of 100 —
a trade that needs an element to cost ≤ 15 B, where the cheapest this wire format can carry is 30 B
(a varint key, a 19-byte `Timestamp`, two framing bytes, then the payload). Totalled: 1.52× the
default's bytes at 8-byte values, 36× at 4 KB. No swept `t` saves more than 4 % anywhere, all of it
at 8-byte values, and none beats the default at 64 B or above. The outcome, and why the default did
not move: [#315](https://github.com/Akvize/reconcile-rs/issues/315).

The split rule itself is pinned by unit tests in `rbsr/src/protocol.rs`
(`default_split_fan_out_is_constant_at_sixteen`, `sqrt_fan_out_is_still_the_square_root_of_the_range_size`,
`split_children_partition_the_parent_range`), so changing the *default* fails CI rather than
silently changing every cluster's bandwidth profile — this benchmark quantifies such a change, it
does not guard it. `split_children_partition_the_parent_range` is policy-independent and must hold
under any fan-out rule.

## Not covered yet

- **External comparisons** (e.g. point-read vs Redis over loopback) are intentionally out of scope here to keep the default `cargo bench` dependency-light. They belong behind an optional, non-CI Cargo feature in a follow-up.
- **No other reconciliation implementation has ever been run in this harness** — not Negentropy, not RIBLT, not AELMDB. This is the *algorithm-family* half of the row above, and it is a separate gap with a separate consequence: every comparison these benchmarks support is `reconcile-rs` against `reconcile-rs` (policy against policy, `b` against `b`, RTT lane against RTT lane). Where `SOTA.md` §1.3 and §2.2 place this crate beside another family, the other family's numbers are **quoted from its paper**, on other hardware and sometimes in another cost model. That is enough to orient a design choice and not enough to support "X beats Y", which is why `SOTA.md` §1.3 now says so at the table. [#174](https://github.com/Akvize/reconcile-rs/issues/174) dropped the external-comparison criterion deliberately (version drift, cross-process flakiness, CI weight); reinstating it is the prerequisite for any like-for-like claim, and is a decision, not an omission. [#362](https://github.com/Akvize/reconcile-rs/issues/362) reinstates the half those three reasons do not reach — the *counted* columns against Negentropy, from a committed fixture pinning its commit SHA, with no timing, no build dependency and no CI.
- The `img/perf-*.png` graphs in the repo root are produced out-of-band from the `bench` target's Criterion output; regenerating them is a manual step (there is no committed plotting pipeline).

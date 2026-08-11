# Benchmarks

Three Criterion targets, all `harness = false`, none feature-gated:

| Target | What it measures |
|---|---|
| `bench` | `FingerprintTreeMap` micro-benchmarks (fill, single insert/remove, cumulated range-fingerprint) vs `BTreeMap`, plus the dated-vs-value-only fill and the single-difference `ReplicatedMap` send/reconcile latency. |
| `system` | End-to-end, **public-API** system benchmarks (below). |
| `protocol` | Wire *and* local cost of one full RBSR reconciliation, **per refinement policy** — messages, advertised ranges, bytes, datagrams, IP fragments, IDLIST elements and RSOS query counts, as a function of store size `n`, difference size `d`, and how the differences cluster (below). |

No target runs in CI — CI only *compile-checks* them (`cargo bench --no-run --features internal-testing`). Run them locally when you want numbers.

## Running the system benchmarks

```sh
# Everything (point-read, memory, bulk-load, cold-sync, gossip fan-out/propagation, durable-rejoin):
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
- **`cold_sync`** — wall time for an **empty** node to converge with a **full** one purely via anti-entropy: the full node is pre-loaded before it has any peer (nothing is broadcast eagerly), then the empty node seeds it and pulls the whole dataset through the range-diff protocol; timed until fingerprints match (drives #168). This is a loopback measurement — real-network transmission delays make it larger.
- **`gossip_fanout`** — bytes/datagrams *one node* sends for a single write, as peer count `N` grows (`2..128`, full-mesh-seeded, on an in-process `InMemoryNetwork` — no real sockets). `Replica::broadcast` (`src/replica.rs`) sends every local write to **all** known peers with no bound (only the separate, periodic WAN anti-entropy round is capped by `remote_fanout`), so this is expected, and confirmed, to be O(N) per node. Prints the exact datagram/byte count per write (deterministic, like `memory_footprint`) alongside the timed send-loop cost (drives #174's scaling gap).
- **`gossip_propagation`** — wall time from a write on one node to **every** other node observing it, as `N` grows (`2..32`, smaller range than `gossip_fanout` — see the caveat below). Unlike `gossip_fanout`, every node runs its real receive/reconcile loop throughout: the steady-state counterpart to `cold_sync`'s from-scratch convergence (drives #174's scaling gap).
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
covered separately by `cold_sync`). No comparable open-source gossip/SWIM library surveyed for this
(chitchat, foca, memberlist) ships an automated, reproducible N-node scaling benchmark in-repo — the
closest real precedent is HashiCorp's one-off [Consul 66k-node scale test](https://www.hashicorp.com/en/blog/consul-scale-test-report-to-observe-gossip-stability),
a real-hardware exercise, not a runnable harness. These two benchmarks are closer to establishing a
methodology than following one.

## The `protocol` benchmark

```sh
cargo bench --bench protocol            # cost table + the timed drive loop
cargo bench --bench protocol -- --quick

# The printed table only (no timed group): give Criterion a filter that matches no benchmark id.
cargo bench --bench protocol -- 'no_such_benchmark'
```

`reconciliation_cost` prints, for each `(policy, n, d, clustering)`, the exact volume two peers
exchange to converge: bytes (bincode, the same encoder the real transport uses), advertised
`RangeAggregate`s, one-way messages, datagrams, IP fragments, the largest single message, the IDLIST
ranges **and the elements they actually ship**, and the local `Aggregate`/`Rank`/`Select` counts
summed over both peers. Deterministic, so it is printed rather than timed — like `system`'s
`memory_footprint`. The timed `reconciliation_drive` group alongside it measures the local CPU cost
of driving a whole run per policy, the quantity arXiv:2603.19820 models as `T_loc`.

**Read the columns together.** A policy that splits less advertises fewer ranges but reaches its
IDLIST cutoff on wider ranges, and every enumerated element is a *value* on the wire — nearly all of
them elements the peer already holds. At n = 10⁵, d = 100 scattered, the paper's `t`=32 policy saves
40 % of the refinement bytes and ships **5 036 elements instead of 100**. Reporting ranges without
elements would pick the wrong default. The message column is the round-trip count, and every
benchmark here runs at RTT ≈ 0 ([#280](https://github.com/Akvize/reconcile-rs/issues/280)): weigh it
by your own RTT before concluding.

**Why it exists.** RBSR's published bounds — `O(d log n)` communication, `O(log n)` sequential
rounds — assume the fixed branching factor `b` of the paper's Algorithm 2. `rbsr`'s default
`SqrtFanOut` instead cuts at `step = ⌊√m⌋`, so neither bound describes the default configuration.
Since [#257](https://github.com/Akvize/reconcile-rs/issues/257) made the rule a swappable
`RefinementPolicy`, this benchmark compares them head to head instead of comparing a measurement
against an estimate. Headline, for a **single** missing element in a 10⁶-entry store: ~53 kB over
~1 048 ranges under `√m` against **3.8 kB over 78 ranges** for `b = 16` — at the *same* 8 one-way
messages, because log₁₆ 10⁶ ≈ 5 is already the iterated-square-root depth at that size. The
`Θ(log log n)` round advantage `√m` is supposed to buy does not appear below n ≈ 10¹². The timed
`reconciliation_drive` group widens it further, in a column no RTT caveat touches: 2.10 ms against
45.0 µs, ≈47×. The widest
single round at d = 1 is 50 781 B (inside the 65 507-byte datagram ceiling, ~35 IP fragments at a
1500-byte MTU); at d = 100 it reaches 160 908 B, i.e. three datagrams. Discussion in `SOTA.md` §2.2.

`fan_out_sweep` then varies the branching factor alone (`FixedFanOut`, `b` = 2…256), which is the
question "if the default becomes a fixed `b`, which one". Bytes and local work follow `b / ln b`
(minimized near `b = 3`); one-way messages fall as `log_b n` until they hit a floor — 6 at
`n = 10⁶`, reached at `b = 32` — past which extra `b` is paid for and buys nothing. The widest single
round grows linearly in `b` and is the hard ceiling: at `n = 10⁵`, `d = 100` it already exceeds one
datagram at `b = 16`. `b = 16` is the only swept value never worse than the current `√m` default on
rounds across every measured `(n, d, clustering)`; `b = 4` is the bytes-and-CPU optimum, two
round-trips behind.

The split rule itself is pinned by unit tests in `rbsr/src/protocol.rs`
(`split_fan_out_is_square_root_of_the_range_size`, `split_children_partition_the_parent_range`), so
changing the *default* fails CI rather than silently changing every cluster's bandwidth profile —
this benchmark quantifies the change, it does not guard it. `split_children_partition_the_parent_range`
is policy-independent and must survive any fan-out change.

## Not covered yet

- **External comparisons** (e.g. point-read vs Redis over loopback) are intentionally out of scope here to keep the default `cargo bench` dependency-light. They belong behind an optional, non-CI Cargo feature in a follow-up.
- The `img/perf-*.png` graphs in the repo root are produced out-of-band from the `bench` target's Criterion output; regenerating them is a manual step (there is no committed plotting pipeline).

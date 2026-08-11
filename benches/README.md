# Benchmarks

Three Criterion targets, all `harness = false`, none feature-gated:

| Target | What it measures |
|---|---|
| `bench` | `FingerprintTreeMap` micro-benchmarks (fill, single insert/remove, cumulated range-fingerprint) vs `BTreeMap`, plus the dated-vs-value-only fill and the single-difference `ReplicatedMap` send/reconcile latency. |
| `system` | End-to-end, **public-API** system benchmarks (below). |
| `protocol` | Wire cost of one full RBSR reconciliation — messages, advertised ranges and bytes as a function of store size `n` and difference size `d` (below). |

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
```

`reconciliation_cost` prints, for each `(n, d)` pair, the exact volume two peers exchange to
converge: bytes (bincode, the same encoder the real transport uses), advertised `RangeAggregate`s,
one-way messages, the largest single message, and IDLIST count. Deterministic, so it is printed
rather than timed — like `system`'s `memory_footprint`. The timed `reconciliation_drive` group
alongside it measures the local CPU cost of driving a whole run, the quantity arXiv:2603.19820
models as `T_loc`.

**Why it exists.** RBSR's published bounds — `O(d log n)` communication, `O(log n)` sequential
rounds — assume the fixed branching factor `b` of the paper's Algorithm 2. `rbsr::protocol_round`
instead cuts at `step = ⌊√m⌋`, so neither bound describes this implementation. The benchmark
replaces an estimate with a measurement; the headline is that a **single** missing element in a
10⁶-entry store costs ~53 kB over ~1 000 advertised ranges, against ~4 kB for `b = 16`, while the
message count stays flat (6–8) across three decades of `n`. The widest single round is 50 781 B —
inside the 65 507-byte datagram ceiling, but ~35 IP fragments at a 1500-byte MTU. Discussion in
`SOTA.md` §2.2, decision in [#257](https://github.com/Akvize/reconcile-rs/issues/257).

The split rule itself is pinned by unit tests in `rbsr/src/protocol.rs`
(`split_fan_out_is_square_root_of_the_range_size`, `split_children_partition_the_parent_range`), so
changing it fails CI rather than silently changing every cluster's bandwidth profile — this
benchmark quantifies the change, it does not guard it.

## Not covered yet

- **External comparisons** (e.g. point-read vs Redis over loopback) are intentionally out of scope here to keep the default `cargo bench` dependency-light. They belong behind an optional, non-CI Cargo feature in a follow-up.
- The `img/perf-*.png` graphs in the repo root are produced out-of-band from the `bench` target's Criterion output; regenerating them is a manual step (there is no committed plotting pipeline).

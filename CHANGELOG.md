# Changelog

All notable changes to `reconcile` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**MSRV policy:** an MSRV bump is a *minor* version increment (e.g. 0.2 → 0.3).
Patch releases never raise the MSRV.

---

## [Unreleased] — 0.3.0

**This release contains multiple breaking changes that require coordinated
upgrades across the cluster.  Nodes running 0.2.x and 0.3.0 simultaneously
will not interoperate correctly.**

### Breaking — wire format (authenticated mode, anti-replay)

The authenticated-mode wire format now includes a per-datagram anti-replay
header (issue #199).  All nodes in a cluster **must** be upgraded to 0.3.0 in
lockstep; a 0.2.x node will reject 0.3.0 datagrams (and vice versa) because the
MAC covers the new header field.  Rolling upgrades are not safe — quiesce the
cluster, upgrade all nodes, then restart.

### Breaking — `with_persistence` now returns `Result`

`ReconcileStore::with_persistence` (previously infallible) now returns
`Result<ReconcileStore<K, V>, LoadError>`.  Call sites must handle the error;
`LoadError::Corrupt` **must not** be silently ignored (doing so drops tombstones
and re-enables the resurrection hazard of issue #109).  See the updated
`README.md` Persistence section for the recommended error-handling pattern.

### Breaking — public API removals (fingerprint-unsafe mutable iterators)

The mutable iterator types that allowed in-place value mutation without
updating the range fingerprint have been removed from the public API
(issue #197): `IterMut`, `ValuesMut`, `HRTree::iter_mut()`, `HRTree::values_mut()`.
Any mutation through them silently desynced the fingerprints replicas compare.
Replace call sites with the hash-safe mutation path —
`ReconcileStore::get_mut` (or `HRTree::with_mut`) — which recomputes the
fingerprint on every edit.

### Breaking — wire and on-disk format (`Entry` / `State` domain type, issue #143)

The internal `(Timestamp, Option<V>)` tuple is replaced by an intention-revealing
`Entry<Timestamp, V>` cell with a `State<V>` (`Present(V)` / `Tombstone`) payload
(architecture step 4, `ARCHITECTURE.md` §6). This **changes both the gossip wire
format and the `FileSnapshot` on-disk format** — a coordinated break riding this
release alongside the anti-replay change above. Concretely:

- The value-shape helper traits `Reconcilable`, `MaybeTombstone` (`reconcilable.rs`)
  and `Timestamped` (`clock.rs`) are removed; their behaviour is now inherent on
  `Entry` (`is_tombstone` / `value` / `merge`, the last carrying the LWW policy)
  and plain `stamp` field access.
- `Projectable` / `ValueOnly<V>` are dissolved: the dateless mirror's value-only
  projection **is** `State<V>` (`Entry::project` → `State<V>`), so the dated store
  and the dateless `ReconcileMirror` share one domain type. `State`'s `Hash` stays
  timestamp-less (invariant #8); `Entry`'s covers the stamp (invariant #7).
- The public pre-insert hook `ReconcileStore::add_pre_insert` now takes
  `&Entry<Timestamp, V>` (was `&(Timestamp, Option<V>)`), and `PersistedState` /
  `DatedEntries` carry `Entry<Timestamp, V>`.
- `reconcile::{Entry, State}` are now exported (replacing `Projectable` / `ValueOnly`).
- `FileSnapshot` now writes a versioned header (`RCNL` magic + `u32` format version).
  **Existing (0.2.x, header-less) snapshots are not forward-compatible** and are
  rejected cleanly with `LoadError::Corrupt` and a descriptive message rather than
  being silently misinterpreted (which would drop tombstones and re-enable the
  resurrection hazard of issue #109).

### Added — `Transport` and `Codec` ports (hexagonal I/O boundary, issue #144)

The reconciliation engine no longer calls `tokio::net` or `bincode` directly: it
drives its datagram I/O and wire encoding through two new public ports
(architecture step 5, `ARCHITECTURE.md` §3.4). **No wire-format change** — the
frozen bincode `DefaultOptions` config and `Message` tag order are preserved.

- `Transport` port + `UdpTransport` adapter (`reconcile::{Transport, UdpTransport}`):
  datagram send/recv/local-addr over `Addr = SocketAddr`. `UdpTransport::bind`
  owns the socket bind and `SO_RCVBUF` / `SO_SNDBUF` sizing.
- `Codec` port + `BincodeCodec` adapter (`reconcile::{Codec, BincodeCodec}`):
  `encode` to a buffer and `decode_stream` with a `max_items` cap that bounds the
  datagram-expansion DoS (issue #151).
- Authentication stays **ahead of** the codec: the MAC is verified on raw bytes
  before any decode (invariant #5) — the `Codec` never absorbs authentication.
- The engine is generic over the codec (default `BincodeCodec`) and holds the
  transport as `Arc<dyn Transport<Addr = SocketAddr>>`, so an in-memory transport
  makes convergence testable with no real sockets (a deterministic convergence
  proptest now runs over it). New dependency: `async-trait` (for the `Transport`
  port) and tokio's `sync` feature (for the in-memory test transport).

### Changed — MSRV raised to 1.85

The minimum supported Rust version is **1.85** (declared in `Cargo.toml`
`rust-version`; enforced by the `msrv` CI job).  Per policy this is a minor bump.

---

## [0.2.1] — 2026-05

### Added
- Per-datagram payload confidentiality via XChaCha20-Poly1305 AEAD
  (`encryption` Cargo feature, issue #96 / PR #131).
- Lightweight dateless read-only mirror `ReconcileMirror` — converges with a
  dated cluster over the same UDP port without keeping per-value timestamps
  (issue #128 / PR #133).
- Runtime observability: `tracing` spans across the engine, network, and
  reconciliation paths; `metrics` facade (opt-in) with optional Prometheus
  endpoint (`metrics-prometheus` feature, issue #94 / PR #130).
- DNS-based peer discovery (`DnsDiscovery`) for Kubernetes headless Services:
  no RBAC, no API access, correct tombstone-GC semantics (issue #147).
- Multi-location (multi-CIDR) geography-aware gossip with bounded WAN fan-out
  and decentralised topology; live reconfiguration via `set_nets` / `add_net` /
  `remove_net` / `set_remote_interval` / `set_remote_fanout` / … (issue #53).
- Pluggable `Persistence` trait with `InMemory` and `FileSnapshot` backends;
  chunked fuzzy snapshots, directory fsync, tmp-file hygiene, configurable
  cadence and change threshold (issue #122 / PR #202-area).
- Fallible constructors (`ReconcileStore::new` returns `io::Result`; `LoadError`
  distinguishes corrupt snapshots from transient I/O — issue #148).
- Per-peer bounded state with configurable `max_peers` cap.
- Wall-time floor guard: decommissions peers whose tombstone acks are pending
  but the peer has been absent past the tombstone timeout (discovery grace
  period, issue #201-area).
- HLC restart monotonicity: the clock is seeded from the persisted snapshot on
  startup so the HLC total order is never violated across restarts (issue #195).
- `TimeoutWheel` same-millisecond expiry correctness fix (issue #196).
- HLC far-future stamp and counter-wrap hardening (issue #198).
- Anti-replay per-peer sequence tracking (foundation, issue #199-area).
- Feature-matrix CI: dedicated `mac-hmac`, `encryption`, and `macos` jobs;
  scalable poll budget via `RECONCILE_TEST_TIME_MULTIPLIER` (issue #203).
- MSRV declared as `rust-version = "1.85"` in `Cargo.toml`; MSRV CI job
  (`dtolnay/rust-toolchain@1.85`, issue #189).
- `cargo publish --dry-run` in the `lint-and-test` CI job (issue #204).

### Fixed
- Fingerprint-desyncing mutable iterators removed from public API (issue #197).
- `pre_insert` hook now runs outside the write lock on both the local and
  network paths, with a regression test (issue #149 / commit `f4b5028`).
- Removed debug `println!` from the hot reconciliation path (issue #113).
- `DefaultHasher` replaced on the wire: fingerprints are now 256-bit additive
  BLAKE3 (issue #111).
- Malformed-packet panics hardened: crafted `HashSegment` / inverted ranges /
  missing bounds are rejected before deserialisation (issues #107, #112).
- Tombstone resurrection closed: GC is now gated on causal stability — a
  tombstone is only collected once every monotonic cluster member has
  acknowledged its exact version hash (issue #109).
- HLC conflict resolution replaces unsafe physical-clock LWW (issue #110).

[Unreleased]: https://github.com/Akvize/reconcile-rs/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/Akvize/reconcile-rs/releases/tag/v0.2.1

# Migrating from 0.2.1 to 0.3.0

`reconcile 0.2.1` predates the workspace split (AGENTS.md §11): it vendors what are now `rsos`,
`rbsr`, `lww-register` and `gossip` directly. `0.3.0` is not wire- or disk-compatible with it —
read this before upgrading a running cluster.

## Renamed types

| 0.2.1 | 0.3.0 |
|---|---|
| `ReconcileStore` | `ReplicatedMap` |
| `HRTree` | `FingerprintTreeMap` |

`Entry`/`State` were refactored (#243) and several `just_*` accessors were demoted (#180) — see
`ARCHITECTURE.md` for the current shape.

## Dependency line

`reconcile` now depends on four workspace crates, all published to crates.io: `rsos`, `rbsr`,
`lww-register` and `gossip` (published under the name `reconcile-gossip` — the plain name was
taken; source still says `use gossip::…`). No action needed if you only depend on `reconcile` —
`cargo add reconcile` pulls them in transitively.

## Wire format

The gossip wire format changed. **A `0.3.0` node cannot reconcile with a `0.2.1` node.** Roll out
by fully draining and replacing `0.2.1` nodes with `0.3.0` nodes — do not mix versions in one
cluster.

## On-disk snapshots

Snapshots now carry an 8-byte header (`RCNL` magic + little-endian `u32` format version,
`src/snapshot.rs`). **A pre-0.3.0 snapshot is rejected at load time**, not silently misread — you
get an `InvalidData` I/O error naming the mismatch. There is no automatic converter: delete the old
snapshot file and let the node re-seed its state from the cluster via anti-entropy, or drain the
data through the `0.2.1` public API before deleting if you need it preserved outside the cluster.

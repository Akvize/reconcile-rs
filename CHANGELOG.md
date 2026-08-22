# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); version numbers follow
[Semantic Versioning](https://semver.org/) (pre-1.0: a minor bump can carry breaking changes).

## [0.4.0] - 2026-08-22

Publishes the two decisions found auditing pre-freeze wire gaps (#382, #463) — both
additive-only-until-this-point, so this is the last release either could land in without costing a
2.0 — and re-baselines the registry snapshot `cargo semver-checks` (#311) diffs against, which
`v0.3.0` and Gate A's (#206) subsequent breaking changes left stale. See
[MIGRATING.md](MIGRATING.md).

### Changed

- **BREAKING**: `rsos::Fingerprint`'s wire encoding is now raw `[u8; 32]` instead of four
  varint-encoded `u64` limbs (#382) — `WIRE_VERSION` bumped `1` → `2` accordingly, since an older
  peer would otherwise silently misdecode the new fixed-width encoding rather than being rejected.
- Wire tags 5 and 6 are now reserved, skippable message slots (#463) — additive, no `WIRE_VERSION`
  bump needed for this part; see README "Wire versioning" for exactly what it does and does not buy.

## [0.3.0] - 2026-08-19

`0.2.1` predates the workspace split — this is the first release of the split shape. See
[MIGRATING.md](MIGRATING.md) for the full upgrade path; the highlights:

### Changed

- **BREAKING**: workspace split into five crates — `rsos`, `rbsr`, `lww-register`,
  `reconcile-gossip` (imported as `gossip`) and the `reconcile` facade. See `ARCHITECTURE.md` §2.
- **BREAKING**: `ReconcileStore` renamed to `ReplicatedMap`; `HRTree` renamed to
  `FingerprintTreeMap`.
- **BREAKING**: `Entry`/`State` domain-type refactor (#243); several `just_*` accessors demoted
  (#180).
- **BREAKING**: gossip wire format changed — a `0.3.0` node cannot reconcile with a `0.2.1` node.
- **BREAKING**: on-disk snapshot format now carries a magic + format-version header; a pre-0.3.0
  snapshot is rejected at load time rather than silently misread (`src/snapshot.rs`).

### Added

- `CHANGELOG.md`, `SECURITY.md`, `MIGRATING.md`.
- `rust-version` (MSRV 1.85) declared on all five manifests, plus `docs.rs` build metadata so
  feature-gated items render on the published docs.

## [0.2.1] - 2026-06-12

Last release before the workspace split. See the
[GitHub release notes](https://github.com/Akvize/reconcile-rs/releases/tag/v0.2.1).

[0.4.0]: https://github.com/Akvize/reconcile-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Akvize/reconcile-rs/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/Akvize/reconcile-rs/releases/tag/v0.2.1

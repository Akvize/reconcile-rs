# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); version numbers follow
[Semantic Versioning](https://semver.org/) (pre-1.0: a minor bump can carry breaking changes).

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

[0.3.0]: https://github.com/Akvize/reconcile-rs/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/Akvize/reconcile-rs/releases/tag/v0.2.1

# API gap analysis — `reconcile-rs`

> **Audit document, point-in-time.** A complete review of the *public API surface* of all five
> workspace crates, measured against the API a dependent would need. It carries findings and a
> prioritised backlog; it is **not** a living status file. When a finding is fixed or promoted to a
> tracking issue, record that in [`PROGRESS.md`](./PROGRESS.md) and strike it here.
>
> - **Audited tree:** commit `597b94e`, 2026-08-10. `cargo check --workspace --all-features` green.
> - **Scope:** every `pub` item reachable from a dependent, in `rsos`, `rbsr`, `lww-register`,
>   `gossip` and `reconcile`. Not a correctness or security audit — those live in
>   [`PROGRESS.md`](./PROGRESS.md) §2. Where an API defect *has* correctness consequences, it is
>   marked **⚠**.
> - **Companion documents:** [`ARCHITECTURE.md`](./ARCHITECTURE.md) (what the surface is meant to
>   be), [`SOTA.md`](./SOTA.md) §2.4 (the design targets), [`PROGRESS.md`](./PROGRESS.md) (live
>   status and the issues this maps onto).

---

## 1. Method and reference frames

A gap is only meaningful relative to a consumer. This audit uses four personas, each with its own
yardstick, and every finding names the persona it hurts.

| # | Persona | Depends on | Yardstick |
|---|---|---|---|
| **P1** | Application developer building a service | `reconcile` | Rust API Guidelines; the feature baseline of an embedded IMDG (Hazelcast *Replicated Map*, Pekko *Distributed Data*) |
| **P2** | Crate author reusing the data structure | `rsos` | `std::collections::BTreeMap`'s API; the RSOS contract of arXiv:2603.19820 Def. 3.4/3.5/3.9 |
| **P3** | Crate author reusing the algorithm | `rbsr` | RBSR as specified in arXiv:2603.19820 §4 (Algorithm 1/2), incl. its tuning parameters |
| **P4** | Integrator implementing a port | `gossip`, `lww-register` | Can the port be implemented today, from outside, without private items? |

P2/P3 are not hypothetical: `ARCHITECTURE.md` §3.2 states that `rsos` and `rbsr` are
"published-intent, reusable crates" whose primitives are `pub` precisely "for a consumer who depends
on `rsos`/`rbsr` directly instead of through `reconcile`". `lww-register` and `gossip` disclaim
stability, but their four **ports** are documented extension points (`ARCHITECTURE.md` §3.2) and are
audited as such.

Severity is about the *dependent's* cost, not the implementation's:

- **P0** — blocks a documented use case, or is a semantic trap that silently produces a wrong result.
- **P1** — forces a dependent into a workaround (an extra dependency, a clone, a fork).
- **P2** — polish, discoverability, hygiene.

---

## 2. Cross-cutting findings

These apply to more than one crate and are cheapest to fix once, workspace-wide.

### X1 · No `missing_docs` lint anywhere — P1 (all crates)

No crate root carries `#![deny(missing_docs)]` or `#![warn(missing_docs)]`, and CI's
`RUSTDOCFLAGS=-Dwarnings` only catches *broken intra-doc links*, never *absent* docs. The result is
undocumented items on the primary published surface — `ReplicatedMap::{fingerprint, get, remove,
start_reconciliation}`, `Config`'s type-level doc, `Config::{with_port, with_listen_addr}`,
`RandomProbe::new` among others.

This is exactly the class AGENTS.md §10 says must become a failing command rather than a prose
guideline: **add `#![deny(missing_docs)]` to all five crate roots**, fix the fallout, and the rule
enforces itself from then on.

### X2 · Foreign types leak into public signatures — P1 (`reconcile`, `gossip`)

A public signature naming a type from a non-re-exported dependency forces the dependent to add that
exact dependency at a compatible version, and turns every upgrade of it into a breaking change of
*our* API. Current leaks:

| Item | Leaked type | From |
|---|---|---|
| `ReplicatedMap::get`, `ReadReplicaMap::get` | `parking_lot::MappedRwLockReadGuard` | `parking_lot` 0.12 |
| `RandomProbe::new` | `parking_lot::RwLock`, `rand::rngs::StdRng` | `parking_lot` 0.12, `rand` 0.8 |
| `gossip::bincode::{encode, decode_stream}` | `::bincode::Result`/`ErrorKind` | `bincode` 1.x |
| `prometheus::{install_recorder, serve}` | `metrics_exporter_prometheus::{BuildError, PrometheusHandle}` | `metrics-exporter-prometheus` 0.16 |
| `Transport` (`#[async_trait]`) | the `async-trait` macro | `async-trait` 0.1 |

`rand` 0.8 is already a major version behind; with `StdRng` in a public constructor, moving to 0.9
is a breaking change of `gossip`'s API rather than an internal bump.

**Fix:** either re-export the crate (`pub use parking_lot;`, `pub use async_trait::async_trait;`)
or, better for the guard types, wrap in an owned newtype (`reconcile::ValueRef<'_, V>` derefing to
`V`) so the dependency is not part of the contract at all.

### X3 · No MSRV, no docs.rs metadata — P2 (all crates)

No manifest declares `rust-version` ([#189](https://github.com/Akvize/reconcile-rs/issues/189)), and
none carries `[package.metadata.docs.rs]`. Without the latter, docs.rs builds with default features
only: `metrics`, `metrics-prometheus`, `encryption`, `zeroize` and `dns-hickory` are invisible on the
rendered documentation, and feature-gated items appear with no "available on feature X" badge.

**Fix:** in every manifest,

```toml
[package.metadata.docs.rs]
all-features = true
rustdoc-args = ["--cfg", "docsrs"]
```

plus `#![cfg_attr(docsrs, feature(doc_auto_cfg))]` on the roots that gate items.

### X4 · Published-crate discoverability — P2 (`reconcile`)

The root manifest — the *only* crate actually offered to users — declares no `keywords` and no
`categories`, while the four internal crates (`rsos`, `rbsr`, `lww-register`, `gossip`) declare both.
The one crate that needs to be found on crates.io is the one that cannot be.

### X5 · Growing public structs without `#[non_exhaustive]` — P1

`reconcile::replicated_map::Config` (15 public fields, plus a builder) and
`lww_register::PersistedState` (3 public fields, a snapshot format explicitly expected to grow) are
both fully public and both exhaustive. Adding a field to either is a breaking change today, and
`Config` additionally offers two construction paths (struct literal *and* builder) whose invariants
differ — see F-R5.

### X6 · `io::Result` used as a general-purpose error type — P1 (ports)

`Persistence::{load, save}` and `Discovery::discover` return `std::io::Result`. Neither port is
inherently about file or socket I/O: an S3, Postgres or Consul adapter has to launder its real error
through `io::Error::other(...)`, losing the type, and a caller cannot distinguish "not found" from
"corrupt" from "backend unreachable" — a distinction `PROGRESS.md`'s open
[#202](https://github.com/Akvize/reconcile-rs/issues/202) (crash-loop on transient load failure)
turns on directly.

**Fix:** a `#[non_exhaustive]` error enum per port, or at minimum
`Box<dyn std::error::Error + Send + Sync>`.

---

<!-- PER-CRATE SECTIONS FOLLOW -->

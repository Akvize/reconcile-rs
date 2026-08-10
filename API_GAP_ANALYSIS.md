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

## 0. Verdict

**The map APIs are not complete.** That was the working hypothesis going in, and it does not survive
contact: `FingerprintTreeMap` carries three P0 defects (§3), and one of them — equality decided on
the fingerprint alone, ignoring size — is the same class of bug as F1/#106, reintroduced in the map's
own `PartialEq` after being fixed everywhere else. The *read* surface added by #268/#269/#179/#180 is
genuinely good and symmetric; what is missing sits underneath it, in trait impls, generic bounds and
panic-safety.

Crate by crate:

| Crate | State of the public API | Headline gap |
|---|---|---|
| `rsos` | Good read surface on a defective foundation | `PartialEq` ignores size; `range` fails to compile on runtime bounds; `with_mut` is not panic-safe |
| `rbsr` | The loop closes; the contract does not | `RsosView` has no inter-method laws → a third-party backend is a remote-panic surface; subspace reconciliation is unreachable |
| `lww-register` | The cleanest crate here | `Persistence` is sync, snapshot-only, and its caller panics on load failure |
| `gossip` | Adapter layer fine; extension points half-open | `Mac` is unimplementable from outside ⇒ **key rotation is impossible** |
| `reconcile` | Map surface good, **lifecycle surface absent** | No shutdown, no readiness, no peer introspection, no forced flush |

Three findings are worth naming individually because they are not API polish:

1. **`with_discovery`'s precondition is a `debug_assert!`** (`src/replicated_map.rs:429`) — a no-op in
   release. A speculative discovery source is silently accepted and its absences decommission live
   members, releasing the causal-stability gate `ARCHITECTURE.md` §5 invariant 6 rests on.
2. **Every write panics outside a Tokio runtime** — sync `&self` methods calling `tokio::spawn`, with
   nothing in the signature or docs to say so.
3. **`zeroize` protects one copy of the cluster key in five** (§5, F-P7), because the public boundary
   passes a raw `[u8; 32]` and `Config: Copy` structurally forbids a `Drop`.

The single highest-leverage change is the mechanical one: `#![deny(missing_docs)]` plus
`#[non_exhaustive]`/`#[must_use]`/docs.rs metadata across all five crates. It costs `rsos` zero doc
debt today, one doc line in `gossip`, and it converts a class of prose guideline into a failing
command — which is what AGENTS.md §10 asks for.

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

## 3. `rsos` — the map crate (persona P2)

**Verdict: not complete.** The working hypothesis going in was that the map APIs were done and only
needed confirming. They are not: `FingerprintTreeMap` carries three P0 defects — one semantic
(equality decided on the fingerprint alone), one that fails to compile for a legitimate call
(`range` on runtime-built bounds), and one that leaves the structure silently wrong (`with_mut`
under a panicking callback). The *read* surface added by
[#268](https://github.com/Akvize/reconcile-rs/issues/268)/[#269](https://github.com/Akvize/reconcile-rs/issues/269)
is genuinely good; what is missing sits underneath it, in the trait impls and the bounds.

### F-S1 ⚠ · `PartialEq`/`Eq` decide equality on the fingerprint alone — P0

```rust
impl<K, V> PartialEq for FingerprintTreeMap<K, V> {         // fingerprint_tree_map.rs:658
    fn eq(&self, other: &Self) -> bool {
        self.root.subtree.fingerprint() == other.root.subtree.fingerprint()
    }
}
impl<K, V> Eq for FingerprintTreeMap<K, V> {}                // :664
```

Two problems, both load-bearing:

1. **It is the exact defect F1/#106 fixed everywhere else.** `ARCHITECTURE.md` §5 invariant 3 and
   `SOTA.md` §2.4 P0-2 both say emptiness and equality are decided on **size**, never on the
   fingerprint — and `Aggregate`'s own docs (`aggregate.rs:86-98`) spell out that a non-empty range
   can legitimately fingerprint to `ZERO`. The protocol honours this; the map's own `==` does not.
2. **`Eq` is unbounded in `K` and `V`.** `FingerprintTreeMap<u64, f64>: Eq` holds (verified
   compiling) — a bound `BTreeMap` cannot express, because `f64: !Eq`. Any code generic over
   `T: Eq` can therefore be handed a map whose values have no equality at all.

**Fix (one line):** `self.root.subtree == other.root.subtree` — `Aggregate: PartialEq` already
bundles size with the fingerprint. Then bound `Eq` on `K: Eq, V: Eq` (a true element-wise `PartialEq`
is the fuller fix, but the aggregate comparison is what the protocol semantics actually mean).

### F-S2 · `range(&R)` does not compile for runtime-built ranges — P0

`pub fn range<'a, R: RangeBounds<K>>(&'a self, range: &'a R) -> ItemRange<'a, K, V, R>`
(`fingerprint_tree_map.rs:859`) ties the range borrow to the borrow of `self`. With **runtime**
bounds, `m.range(&(lo..hi))` is a hard **E0716** — "temporary value dropped while borrowed". It only
appears to work in the repo's own tests and doc examples because *literal* ranges are const-promoted
to `&'static`.

This is a compile-blocking defect for the ordinary case, not an ergonomic wrinkle. `aggregate(&R)`
(`:684`) has the same shape.

**Fix:** take the range by value like `BTreeMap::range(range: R)`.

### F-S3 ⚠ · `with_mut` leaves stale fingerprints if the callback panics — P0

`with_mut` (`:363-402`) mutates the value, then re-lifts and propagates the delta to every ancestor.
If the callback unwinds between those steps, the mutation is kept and the propagation never runs.
Verified: after `catch_unwind`, `get(&10) == Some(999)` while `aggregate(&..)` is unchanged and
`check_invariants()` fails with "per-element fingerprint cache invalid". The map is **not** poisoned
— it stays live and silently wrong, and the wrongness is exactly what the reconciliation protocol
reads.

**Fix:** make the propagation unwind-safe (descend collecting the node path, apply the delta from a
`Drop` guard around the callback). Until then, a `# Panics` section saying so. Threading a return
value through — `fn with_mut<R, F: FnOnce(Option<&mut V>) -> R>(&mut self, key: &K, f: F) -> R` — is
worth doing in the same change.

### F-S4 · Every read method is gratuitously bounded by `Serialize` — P1

`get`, `contains_key`, `position`, `rank`, `select`, `len`, `is_empty`, `aggregate`, `remove`,
`clear`, `retain` all sit in `impl<K: Serialize + Ord, V: Serialize>` (`:323`, `:672`) although
**none of them serializes anything** — `remove`'s inner helper is `fn aux<K: Ord, V>` (`:518`),
`aggregate`'s is `<K: Ord, V, R>` (`:685`). Verified: a generic `fn<K: Ord, V>` cannot call `m.len()`
or `m.remove(k)`, while `m.range()`/`m.first_key_value()` (whose impl block is `impl<K: Ord, V>`,
`:849`) work fine. The split is arbitrary and visible.

**Fix:** move every non-lifting method to `impl<K: Ord, V>`; leave only `insert`, `with_mut`,
`check_invariants` and `FromIterator` under `Serialize`. `clear()` becomes `*self = Self::default()`.
This also dissolves the `new()`-vs-`Default` bound mismatch (`:325` vs `:315`), which today lets
`FingerprintTreeMap::<NotSerde, NotSerde>::default()` compile into a map on which even `len()` is
unavailable.

### F-S5 · Borrowed-key lookup is missing — P1

`get`/`contains_key`/`remove`/`rank`/`position` take `&K`, not std's
`Q: Ord + ?Sized where K: Borrow<Q>`. Verified: on a `FingerprintTreeMap<String, u32>`,
`m.get("abc")` is E0308 — every lookup on a string-keyed map costs an allocation. `Rsos::rank`/
`delete` (`rsos_trait.rs:70, 102`) inherit the same shape.

### F-S6 · The `Rsos` trait states no summary law — P1

`Rsos` (`rsos_trait.rs:53-103`) names `Aggregate` and `Fingerprint` but never says that
`aggregate(range).fingerprint()` must equal the `⊗`-fold of `rsos::lift` over the range, and bounds
neither `K: Serialize` nor `Value: Serialize`. A third-party backend can therefore return arbitrary
fingerprints, satisfy the trait, and — through `rbsr`'s blanket `RsosView` impl — silently
mis-reconcile against a `FingerprintTreeMap` peer. This is F-B1's law, one layer up.

### F-S7 · An `rsos`-only dependent has no get-or-insert — P1

Refusing `entry()` is well argued *for a raw `&mut V`* (`:358-362`): a bare handle lets a caller skip
the re-lift. But the cited replacement for the other half of what `entry()` buys —
"`ReplicatedMap::upsert`/`get_or_insert_with`" — lives in `reconcile`, which is precisely what a P2
dependent does not have. **The refusal does not hold for this crate's own audience.**

**Fix:** a closure-shaped entry that never hands out a bare `&mut V` —
`fn entry(&mut self, key: K) -> Entry<'_, K, V>` exposing only
`or_insert`/`or_insert_with`/`or_default`/`and_modify(FnOnce(&mut V))`, each re-lifting internally.
(A `Drop`-guard design would be unsound under `mem::forget`; the closure shape is not.)

### F-S8 · `Fingerprint` is a wire token with no byte API — P1

`Fingerprint(pub [u64; 4])` (`fingerprint.rs:85`) exposes its representation as a public field, yet
offers no supported byte conversion: `from_bytes` is private (`:92`), there is no `to_le_bytes`, and
`Display` (`:177`) has no `FromStr` to round-trip it. Derives omit `Ord`/`PartialOrd`. A third party
*can* build a `lift`-compatible fingerprint (verified byte-identical) — but only by re-implementing
the private limb decode **and** adding their own `blake3` dependency, since `rsos` re-exports neither
`blake3` nor a byte conversion, so the public `impl Sink for blake3::Hasher` silently depends on
cargo unifying to a semver-compatible `blake3`.

**Fix:** `pub const fn from_le_bytes(&[u8; 32]) -> Self` + `to_le_bytes(self) -> [u8; 32]`,
`pub use blake3;`, and make `encoding::encode_to_vec` (`encoding.rs:153`) public — it is privately
useless anyway, since `impl Sink for Vec<u8>` is public and any consumer can reproduce it in three
lines.

### F-S9 · Iterators are missing the whole trait stack — P1

`Iter`, `Keys`, `Values`, `IntoIter`, `IntoKeys`, `IntoValues` implement `Iterator` and nothing else:
no `size_hint`, `ExactSizeIterator`, `FusedIterator`, `DoubleEndedIterator`, `Clone`, `Debug`.
Verified absent on `Iter`/`Keys`: no `.len()`, no `.rev()`, no `.clone()`, no `{:?}`.

Note **`ItemRange` is not covered by [#92](https://github.com/Akvize/reconcile-rs/issues/92)**, which
`PROGRESS.md:145` scopes to lazy/double-ended traversal — it has the same gap and no tracking issue.
`ItemRange` is also absent from `rsos`'s root re-exports and from `reconcile`'s, so through the
facade the return type of `range` is unnameable (it *is* reachable as
`rsos::fingerprint_tree_map::ItemRange`, the module being public).

### F-S10 · Smaller items — P2

- `select(index) -> &K` (`:779`) panics with std's raw index message, leaking internals: `select(99)`
  on a 1-element map reports *"len is 1 but the index is 99"* where "1" is the leaf's arity, not
  `len()`. Add a checked variant and a meaningful panic message.
- `check_invariants()` (`:596`) is `pub` and panicking, and `rsos/Cargo.toml` has **no `[features]`
  table at all**, so unlike `rbsr`/`reconcile` there is no `internal-testing` seam to put it behind.
- `lift`/`digest` (`fingerprint.rs:235, 246`) can panic via `absorb`'s `.expect` (`:207`) and carry no
  `# Panics` section; same for `range` and `last_key_value`.
- Missing cheap companions: `Extend<(K, V)>`, `From<[(K, V); N]>`, `Index<&K>`,
  `IntoIterator for &mut Self`, `Sum` on `Aggregate`/`Fingerprint` (both documented monoids, so
  `.sum()` over a partition of ranges ought to compile — it does not).
- `Serialize`/`Deserialize` on the map itself: absent, so every consumer round-trips through
  `Vec<(K, V)>` and pays a full rebuild. `BTreeMap` implements both.
- An inverted range (`15..5`) silently aggregates to `(0, 0)` where `BTreeMap::range` panics. The
  choice is right given `rbsr`'s hardening, but it is undocumented on both methods.
- 15 `must_use_candidate` clippy warnings; `#[must_use]` appears only on
  `Fingerprint::combine`/`remove`.
- Every module is `pub` **and** every item re-exported at the root, so each type has two permanently
  stable paths (`rsos::Fingerprint` and `rsos::fingerprint::Fingerprint`); moving an item later
  breaks both. std's pattern is private modules plus root `pub use`.
- `rsos/README.md` — the crates.io front page — attributes Def. 3.9 to "arXiv:2212.13567, Aljoscha
  Meyer, 2023", whereas `lib.rs:14-15` correctly attributes it to Amparore arXiv:2603.19820. It also
  links `crates.io/crates/rbsr`, which per AGENTS.md §11 has never been published.

`cargo rustdoc -p rsos -- -D missing_docs` **passes clean today**, so X1 costs `rsos` zero doc debt.

### 3.1 `BTreeMap` parity

| method / impl | present? | rating | note |
|---|---|---|---|
| `new` / `Default` / `Clone` / `Debug` | ✓ | — | `new` over-bounded vs `Default` (F-S4) |
| `len` / `is_empty` / `clear` | ✓ | Must | over-bounded with `Serialize` (F-S4) |
| `get` / `contains_key` / `remove` | ✓ | Must | no `Borrow<Q>` (F-S5) |
| `get_key_value` | ✗ | Should | needed whenever `K` is a lookup-normalized type; trivial over the existing descent |
| `remove_entry` | ✗ | Should | same descent as `remove`; free once it returns the popped key |
| `get_mut` | ✗ | Should | deliberately refused for `with_mut`; refusal sound **for a raw `&mut V`** |
| `entry` | ✗ | **Must** | refusal does not hold for `rsos`-only dependents (F-S7) |
| `pop_first` / `pop_last` | ✗ | Should | `first_key_value` + `remove` is 2×O(log n) and needs `K: Clone` |
| `first_entry` / `last_entry` | ✗ | Could | subsumed by `entry` + `first_key_value` |
| `append` | ✗ | Could | expressible as a loop; constant-factor loss only |
| `split_off` | ✗ | Should | the natural "shard a keyspace" op for a *range*-summarizable store; no cheap user-side equivalent that preserves aggregates |
| `range` | ✓ | Must | E0716 on runtime bounds (F-S2); no `RangeBounds<Q>` |
| `range_mut` | ✗ | Could | blocked by the re-lift problem; `for_each_mut(range, f)` is the honest form |
| `iter` / `keys` / `values` | ✓ | Must | missing the whole iterator trait stack (F-S9) |
| `iter_mut` / `values_mut` | ✗ | Could | `#[cfg(test)]`-only, correctly withheld |
| `into_iter` / `into_keys` / `into_values` | ✓ | — | full coverage; `IntoIterator for &mut Self` absent |
| cursors (`lower_bound`/`upper_bound`) | ✗ | Could | `rank`/`select`/`range` cover most uses |
| `retain` | ✓ | — | `&V` not `&mut V`, `K: Clone`, O(n log n) — justified, but undocumented as a deviation |
| `Extend` / `From<[(K,V); N]>` / `Index` | ✗ | Should | `FromIterator` exists; the three cheap companions do not |
| `PartialEq` / `Eq` | ✓ | **Must** | present but semantically wrong and unbounded (F-S1) |
| `PartialOrd` / `Ord` / `Hash` | ✗ | Could | blocked on fixing `PartialEq` first |
| `Serialize` / `Deserialize` | ✗ | **Must** | `BTreeMap` has both |
| iterator `ExactSize`/`Fused`/`DoubleEnded`/`Clone`/`Debug` | ✗ | **Must** | F-S9 |

---

## 4. `rbsr` — the algorithm crate (persona P3)

**Verdict: the loop closes, the contract does not.** A standalone consumer *can* drive a full
reconciliation today — this was verified, not argued: an external crate depending only on
`rbsr` + `rsos` (no `reconcile`, no `internal-testing`) converged two divergent 200/250-key stores
to 400 identical keys in 8 rounds over a byte-level transport, using public items throughout.
Nothing is unnameable and no private item blocks the loop.

What is missing is everything *around* the loop: the trait contract that makes a third-party backend
safe, session/termination help, output bounds, and a readable wire contract.

### F-B1 ⚠ · `RsosView` states no inter-method laws — P0

`RsosView` (`rbsr/src/rsos_view.rs:28-44`) documents its four methods individually and imposes **no
contract between them**. `protocol_round` nonetheless assumes all four:

- `size() == aggregate(&..).size()`
- `rank(select(r)) == r`
- `aggregate(l..u).size() == rank(u) − rank(l)`
- and that all four observe **one consistent snapshot** for the duration of the call — which
  `reconcile` supplies by holding `self.map.read()` across the call (`src/replica.rs:1127-1133`),
  and which the trait never mentions.

Violating this is not an error, it is a **panic driven by a remote key**: a plausible backend with an
unclamped `rank` panicked at `protocol.rs:389` (`local.select(next_index)`, index out of bounds) when
a segment named a key beyond the local store. A remote, lazy or cached RSOS backend — precisely the
backend the crate root advertises support for — is therefore a remote-DoS surface for its author.

**Fix:** state the four laws and the snapshot requirement on the trait; add `debug_assert!`s at
`protocol.rs:316`/`374`; and clamp defensively —
`let next_index = (cur_index + step).min(local.size());` before `select`.

### F-B2 · `RangeAggregate` encapsulation is theatre — P1

Fields are private with no accessors and the only constructor is `for_testing`, gated behind
`internal-testing` (`protocol.rs:120-148`). But the derived `Serialize`/`Deserialize` are a *de facto*
public constructor **and** accessor: a consumer can round-trip the type through a mirror struct to
read its bounds out and to build arbitrary ones — verified, including driving `protocol_round` on a
hand-built `Included(10)..Excluded(50)` segment that emitted 7 children. `Debug` also prints the
bounds verbatim, naming the otherwise-unnameable `KeyRange`.

So the encapsulation stops the honest consumer and not the determined one, while quietly making the
private layout a de-facto public API that a future "tidy-up" would break.

This has a consequence beyond hygiene. Because there is no supported way to build a bounded starting
family, **partial (prefix/subspace) reconciliation is unreachable from the supported API** — even
though `initial_ranges`' own doc (`protocol.rs:217-219`) says a caller "would only need a different
starting family". Ordered-range reconciliation is exactly the asset `SOTA.md` §2.2 credits RBSR with
over sketch families ("Sketches reconcile an *opaque* set"), and it is the one thing the surface
withholds.

**Fix:** make it deliberate — promote `for_testing` to
`pub fn new(start: Option<K>, end: Option<K>, aggregate: Aggregate) -> Self`, add
`start_bound()`/`end_bound()`/`aggregate()`, and document the byte layout on the type.

### F-B3 · The wire type is undocumented as a wire contract — P1

`StartBound`/`EndBound`/`KeyRange` are `pub(crate)`, so **rustdoc renders nothing about the wire
shape**, and the type-level "Wire compatibility" note (`protocol.rs:111-119`) explains *why* field
order is load-bearing without ever stating what the layout *is*. The golden vector that pins it lives
in another crate behind a feature flag. A non-Rust peer cannot be written from the published docs.

### F-B4 · No session, no termination help — P1

`protocol_round` is stateless and returns nothing (`protocol.rs:278-285`): no round counter, no
`is_done`, no progress signal. A peer that replays one segment with a bogus aggregate keeps the
responder answering forever; the verification driver needed a hand-rolled `rounds > 200` guard.
Malformed segments are dropped with a bare `debug!` (`protocol.rs:307`) — a `tracing` event, so a
consumer on `log` or with no subscriber installed sees literally nothing.

**Fix:** return a summary rather than nothing —
`pub struct RoundOutcome { pub split: usize, pub idlist: usize, pub skipped: usize, pub dropped_malformed: usize }`.

### F-B5 · Unbounded output — P1

One segment over `n` keys emits `⌊√n⌋` children into a caller `Vec` with no cap (`protocol.rs:374`);
at `n = 10⁶` that is 1000 children per segment per round. `reconcile` bounds the *input* side
(`decode_stream(max_items)`, `src/replica.rs:46`); `rbsr` offers nothing on the output side, against
`SOTA.md` §2.4 P3-9's explicit "allocation bounds, bounded fan-out".

**Fix:** a `RoundLimits { max_children, max_fanout }` parameter, counting what it stopped in
`RoundOutcome` rather than truncating silently.

### F-B6 · The blanket impl is a one-way door, documented backwards — P1

`impl<K, T: Rsos<K>> RsosView<K> for T` does **not** prevent a third party implementing `RsosView`
by hand for a non-`Rsos` type — coherence discharges the overlap, and it compiles. But adding
`impl Rsos<K>` for that same type *later* fails with `E0119` (reproduced). The choice is permanent
and undocumented, and `rsos_view.rs:24-27` steers readers the wrong way — "There is no need to
implement this by hand" — without warning that doing so forecloses `Rsos` forever. Downstream
*blanket* impls are blocked outright.

### F-B7 · Nothing in-repo exercises the public API — P1

There are **zero doc examples in all of `rbsr/src`** and **no `rbsr/tests/`**. Every unit test in
`protocol.rs:401-542` constructs `RangeAggregate` through private fields — i.e. tests a surface no
consumer has. AGENTS.md §7 asks for `tests/*.rs` on public-API-crossing changes; the crate whose
entire stated purpose is standalone reuse has no test that reuses it standalone.

### F-B8 · Smaller items — P2

- `local_size` is computed then discarded on the IDLIST path (`protocol.rs:316, 338`), forcing the
  caller into a second `aggregate()` call to size or rate-limit a dump. Emit
  `(EnumerationRange<K>, usize)`.
- "Reading a range's fate off the outputs is exhaustive" (`protocol.rs:248-252`) holds only for a
  single-element batch: flat output `Vec`s lose the input correspondence, and degenerate SPLITs
  re-emit the parent range verbatim.
- `rsos::Aggregate` appears in `RsosView::aggregate`'s public signature but `rsos` is not
  re-exported, so a consumer must add it as a second dependency and keep versions in lockstep or hit
  two-distinct-`Aggregate` errors. `pub use rsos;` at the root.
- Out-params are appended to, never cleared, and nothing says so; reusing a buffer double-processes.
  `active_ranges` taken by value needlessly rejects slices and iterators.

### 4.1 API shape implications for the tracked SOTA directions

- **Configurable `b` and `t`** ([#257](https://github.com/Akvize/reconcile-rs/issues/257)) — both are
  per-round *policy*, not crate constants:
  `RoundConfig { fan_out: FanOut, enumeration_threshold: usize }` with
  `enum FanOut { Fixed(NonZeroUsize), Sqrt }`, threaded as a `protocol_round` parameter and
  defaulting to today's behaviour, so it is purely additive. The two peers' choices need no
  agreement — correctness only needs disjoint children whose union is the parent
  (`protocol.rs:369-372`) — so this stays **off the wire type**. It also unblocks #257 (`b` becomes
  benchable without a source edit) and makes the paper's `T_loc = O(hL + bhI + K)` bound quotable
  again under `FanOut::Fixed`.
- **Generic monoid summary** (`BYOLiftingMonoid`, `SOTA.md` §2.4 P1-4) — the invasive one, and an
  `rsos` change first: `Aggregate` becomes `Aggregate<M>`, `RsosView::aggregate` returns
  `Aggregate<Self::Summary>`, and `RangeAggregate<K>` becomes the **generic wire type**
  `RangeAggregate<K, M>` — so the byte layout becomes `M`-dependent. This is the one change here
  that cannot be made additively, so it belongs before 1.0. It also retroactively justifies F-B2's
  accessors.
- **Hybrid RBSR + Rateless IBLT** ([#185](https://github.com/Akvize/reconcile-rs/issues/185)) — needs
  a fourth outcome beside SKIP/IDLIST/SPLIT ("this leaf is small enough, drain it with a sketch"):
  `RoundOutcome` gains `sketch_ranges`, and `RoundConfig` gains the cutover threshold — which *is*
  `t` from the first bullet, so ship that first as the cheap prerequisite. The sketch symbols stay
  outside `rbsr` (a second wire message the consumer owns), keeping the crate codec-free.

## 5. `lww-register` and `gossip` — the ports (persona P4)

These two crates disclaim stability, so they are audited through the surface that *is* offered as an
extension point: the four ports (`Transport`/`Discovery` from `gossip`, `Clock`/`Persistence` from
`lww-register`), plus the `auth`/`replay`/`entry` items a dependent still meets. Each port was
audited by writing an external implementation against it, out of workspace. Summary first, detail
after.

| Port | Implementable today? | Blocker |
|---|---|---|
| `Transport` | Yes, but only after adding your own `async-trait` | X2 (macro not re-exported) |
| `Discovery` | Yes, cleanly, zero extra deps | Behavioural traps, not compilation |
| `Clock` | Yes — but **not injectable**, so pointless | F-P4 |
| `Persistence` | Yes | Error type, blocking, panicking caller |

### F-P1 · `Transport::Addr` is dead freedom — P1

`type Addr: Clone + Eq + Hash + Send + Sync` (`gossip/src/transport.rs:39`) suggests the transport is
generic over its address type. It is not: every consumer site is hard-wired to
`dyn Transport<Addr = SocketAddr>` (`src/replicated_map.rs:252`, `src/read_replica_map.rs:105`), so an
impl with any other `Addr` compiles and can never be installed. The bound also omits `'static` and
`Debug`.

Separately, `recv_from(&self, …)` forces interior mutability into every implementation
(`InMemoryTransport` needs an `AsyncMutex`, `transport.rs:196`) — worth documenting as an expectation.

**Fix:** delete the associated type, use `SocketAddr` inline, and `pub use async_trait::async_trait;`
from `gossip`'s root so the documented BYOTransport extension point is actually reachable.

### F-P2 ⚠ · `Discovery::kind()` defaults to the destructive variant — P1

`kind()` defaults to `Authoritative` (`gossip/src/discovery.rs:86-88`). An implementor who simply
forgets it gets the semantics where **an absence decommissions a real member** — releasing the
causal-stability gate that `ARCHITECTURE.md` §5 invariant 6 rests on. The fail-safe default would be
`Speculative`.

The guard on the consuming side makes it worse rather than better: `with_discovery` checks the
precondition with `debug_assert!` (`src/replicated_map.rs:429`), which is a **no-op in release**. So a
release build silently accepts a speculative source and lets its absences decommission live members.

**Fix:** make `kind()` a required method with no default body; and turn the `with_discovery` check
into a real one — `-> Result<Self, ConfigError>`, or a newtype only `Authoritative` sources can enter.

### F-P3 · `Discovery` cannot express *why* it failed, and cannot time out — P1

`io::Result<Vec<IpAddr>>` (X6) cannot distinguish "resolver down, skip this round" from "the name is
gone" — a distinction the decommissioning path turns on. `DnsDiscovery::discover` (`:147-153`) calls
`tokio::net::lookup_host` with no timeout, no TTL control and no cancellation.

### F-P4 · `Clock` is a published port with no public implementor and no injection seam — P1

`Clock` is re-exported as public API from `reconcile` (`src/lib.rs:92`) and listed as one of the four
ports in `ARCHITECTURE.md` §3.2. Yet:

- `new_with_clock` is `#[cfg(test)] pub(crate)` (`src/replicated_map.rs:261`), so nobody can inject one;
- the only adapter, `HlcClock`, is itself `pub(crate)` (`src/clock.rs:147`), so the port has **zero**
  public implementors.

The stated rationale — "a non-monotonic clock silently breaks the causal ordering that tombstone
collection depends on" (`src/replicated_map.rs:236-238`) — is sound, and the risk is real: the obvious
naive implementation (read the wall clock, `logical = 0`) compiles and breaks the LWW total order,
because two same-millisecond writes get equal `(physical, logical, node_id)`, `Entry::merge`'s strict
`>` (`entry.rs:168`) keeps each side's own value, and — the stamp feeding the fingerprint — the
replicas re-exchange forever.

But the asymmetry with `Transport` is not defensible as it stands: `InMemoryTransport` is public
*specifically* so downstream crates can test their own application deterministically
(`transport.rs:150-153`), and the same argument applies verbatim to a clock. **A re-exported extension
point nobody can plug in is worse than a private one.** Pick one: open injection
(`ReplicatedMap::with_clock`), shipping a conformance harness alongside it
(`lww_register::clock::assert_conformance(&impl Clock)` checking monotonicity-under-burst and
observe-then-now ordering); or remove `Clock` from `reconcile`'s re-exports and from the §3.2 port
table.

`observe_trusted`'s default body (`clock.rs:582-584`) is a second trap: delegating to `observe` is
sound only for a clamp-free implementation. Any implementor that clamps in `observe` and takes the
default silently reintroduces own-write shadowing. Make it required.

### F-P5 · `Persistence` is sync, snapshot-only, and its caller panics — P1

- **Blocking on the runtime.** `save`/`load` are sync (`persistence.rs:88-90`) and called from the
  async `snapshot_periodically` (`src/replicated_map.rs:380-386`) with no `spawn_blocking`. A 200 ms
  S3 PUT stalls a tokio worker.
- **Whole-map clone per snapshot.** The entire map is cloned into a `Vec` before every save
  (`src/replicated_map.rs:368-374`): ≈2× peak memory, no delta path.
- **`load()` failure is a panic.** `.expect("failed to load persisted state")`
  (`src/replicated_map.rs:334`) turns any third-party backend error — an S3 5xx, a Postgres connect
  timeout, EACCES on the data dir — into a startup abort with no recovery hook. This is
  [#202](https://github.com/Akvize/reconcile-rs/issues/202)'s "crash-loop on transient load failure",
  and the API is where it is decided.
- **`save()` failure is `warn!`-only** (`:380`): durability can be broken for the whole process
  lifetime while the node reports healthy.
- **Undeclared bound.** `PersistedState`'s `Deserialize` requires `K: Eq + Hash`
  (`persistence.rs:70`), stated nowhere on `Persistence<K, V>`, so a third-party backend meets an
  unexplained bound error inside its own `load()` body.

**Fix:** `type Error`; boxed-future `load`/`save` (reusing `DiscoverFuture`'s shape — no new
dependency); `flush`; optional `save_delta`; `try_with_persistence(…) -> Result<Self, _>` keeping the
panicking one as a delegating convenience. Note `#[non_exhaustive]` on `PersistedState` (X5) must land
*with* a constructor, since `load()` has to build one.

### F-P6 · `Mac` cannot be implemented outside `gossip` — P0 for anyone trying

`Mac` (`gossip/src/auth.rs:213`) is `pub`, but `Tag`'s tuple field is private (E0423) and
`ClusterKey::as_bytes` is private (E0624) — verified by compiling an external impl, whose only
satisfying body is `todo!()`. The trait is public in appearance only.

Combined with `Authenticator` being a closed enum (`auth.rs:283-292`), this has one concrete
consequence worth calling out: **key rotation is impossible**. With a single shared secret and no
per-peer identity (AGENTS.md §8), accepting the old and the new key during a rollout is the only
migration path there is, and the type system forbids expressing it.

**Fix:** `Tag::new`/`Tag::as_bytes`, `ClusterKey::expose_secret`, and
`Enabled { primary: ClusterKey, also_accept: Vec<ClusterKey> }` on the verify path (seal always uses
`primary`).

### F-P7 · Zeroize protects one copy in five — P1

Traced end to end, with and without the feature:

1. **With `zeroize`:** only `ClusterKey` gets a `Drop` (`auth.rs:90`), so the copy inside
   `Authenticator::{Enabled, Encrypted}` and its clones is wiped. That is the **only** wiped copy.
2. **Never wiped, either way:** the caller's `[u8; 32]` literal, and `Config.cluster_key`
   (`src/replicated_map.rs:1281`) — `Config` is `#[derive(Clone, Copy)]` (`:1247`), so every
   `with_*(mut self) -> Self` step leaves an unreachable stack copy, and a `Copy` type **cannot** have
   a `Drop` at all.
3. `Authenticator::new` takes the array **by value** (`auth.rs:304`): the parameter plus the
   `ClusterKey::new(bytes)` argument are two further untracked plaintext copies. `Config` is likewise
   passed by value into `Replica::build` and `ReadReplicaMap::build`.
4. **Without the feature:** nothing anywhere is zeroized. In both configurations the decrypted AEAD
   plaintext (`Cow::Owned`, `auth.rs:407`) is never wiped.

The root cause is X2/§4's raw-`[u8; 32]` boundary. **Fix:** make `ClusterKey` the boundary type
(`Config::with_cluster_key(ClusterKey)`, `cluster_key: Option<ClusterKey>`,
`Authenticator::new(Option<ClusterKey>, bool)`), which forces dropping `Config: Copy` — and that is
precisely what makes the feature meaningful rather than decorative.

### F-P8 · Smaller items — P2

- `RandomProbe::new` (`discovery.rs:104`) leaks `parking_lot::RwLock` + `rand::rngs::StdRng` (X2)
  **and** has no external injection point at all (`Replica.probe` is private, `src/replica.rs:424`),
  so a `rand` 0.8→0.9 bump is a breaking change to an API nobody can use. It is also the single
  missing-doc item in both crates — `-W missing_docs` across `gossip` + `lww-register` yields exactly
  one warning, so X1 is one doc line away here.
- `replay::{Seq, Stamp}` (`replay.rs:76-147`) are shaped like API but have no value accessor at all
  (only `to_le_bytes`), no serde, and `impl Sub for Seq` (`:111`) panics on underflow behind a
  doc-only precondition. `#[allow(dead_code)]` on `pub` items (`:87`, `:134`) is vestigial.
- `ReplayFilter::len` (`:480`) compiles only because `#[allow(clippy::len_without_is_empty)]`
  suppresses the lint. `evict` (`:489`) is `#[cfg(test)]`, so it does **not** exist in any published
  build despite its doc advertising it as an escape hatch — AGENTS.md §8 is structurally safe here;
  the documentation is what is wrong.
- ~10 public types have no `Debug` (`ClusterKey`, `Tag`, `Payload`, `Authenticator`, `SenderCounter`,
  `ReplayFilter`, `RandomProbe`, `DnsDiscovery`, `InMemoryNetwork`, `InMemoryTransport`,
  `InMemoryPersistence`), against Rust API Guidelines C-DEBUG: a consumer cannot `#[derive(Debug)]` on
  any struct holding one. Hand-write `ClusterKey`/`Authenticator` to print `[redacted]`.
- No `Display`/`FromStr`/`TryFrom` on `NodeId`, `Timestamp`, `PhysicalTime`, `ClockDrift`,
  `ClusterKey` — so every operator reading a node id or a hex key from env/config writes the parse
  themselves, which AGENTS.md §4 says belongs on the type.
- `Entry`'s impl block is `impl<T: Ord + Copy, V: Clone>` (`entry.rs:151`), over-constraining:
  `project` needs only `V: Clone`, `merge` only `T: Ord + Clone`. Missing `Entry::map`/`as_ref`,
  `State::map`, and `Ord` on `Entry` despite `merge` being exactly `max`.

---

## 6. `reconcile` — the facade (persona P1)

**Verdict: the read/write map surface is in good shape; the *lifecycle* surface is not.** The
collection API closed by [#179](https://github.com/Akvize/reconcile-rs/issues/179)/[#180](https://github.com/Akvize/reconcile-rs/issues/180)
is coherent and symmetric across both map types. What an application developer cannot do is run the
thing in production: no shutdown, no readiness, no peer introspection, no forced flush, and several
silent-failure paths.

### F-R1 ⚠ · Every write panics outside a Tokio runtime — P0

`insert`, `remove`, all `*_bulk`, `clear`, `retain`, `delete_range`, `update`, `upsert`,
`get_or_insert_with` and `get_mut` are **sync `&self` methods** that reach `Replica::broadcast`
(`src/replica.rs:639`), which calls `tokio::spawn`. Outside a runtime that is a panic — "there is no
reactor running" — and nothing in the signature or the docs hints at it. A sync method that panics
depending on ambient async context is a trap; a config-time `Handle` (`Config::with_runtime_handle`,
falling back to `Handle::try_current()` with a hard error) removes the ambient dependency, and a
`# Panics` section documents it in the meantime.

### F-R2 ⚠ · Three different re-entrancy contracts, one of them a self-deadlock — P0

`if let Some(v) = map.get(&k) { map.insert(k, …) }` self-deadlocks: a live `parking_lot` read guard
plus `map.write()` on one thread, with no timeout and no panic. Meanwhile callbacks run under
*different* locks depending on the method — `for_each`/`for_each_in_range`/`retain` under the **read**
lock, `get_mut`/`update`/`upsert` under the **write** lock — and only `for_each` carries any warning
(`:639`).

The guard is `!Send`, so holding it across an `.await` in a spawned task is a compile error rather
than a runtime bug; same-thread re-entrancy is the real hazard.

**Fix:** a `# Deadlock` section on all seven, plus `pub fn get_cloned(&self, k: &K) -> Option<V>`
promoted as the documented default read (which also side-steps X2's guard leak).

### F-R3 · No readiness, no peers, no local address — P0 for operability

`members_snapshot`/`peers_map_len` exist but are `internal-testing`-gated (`:903-938`). There is no
public way to answer "is this node synced", "who are my peers", or even "what port did I bind". The
Kubernetes example points its probes at `/metrics`, which only proves the process is alive — so a
**cold replica serves empty reads while reporting Ready**.

**Fix:** `peers()`, `members()`, `local_addr()`, and a
`SyncState { rounds, last_round_at, peers, last_snapshot_at }` with `sync_state()`.

### F-R4 · `Config::default()` cannot converge — P1

`port: 0` (`:1378`) is used as **both** the bind port and the destination port of every outbound
datagram (`replica.rs:430, 647`, `read_replica_map.rs:441`). A node built from `Config::default()`
therefore binds ephemeral and sends to port 0 — it can never converge, silently. README:46, 168 and
256 all use `Config::default()`.

**Fix:** `Config::new(port: u16)`, and reject `port == 0` in both constructors with
`io::ErrorKind::InvalidInput`.

### F-R5 · `Config` has two construction paths with different guarantees — P1

All 16 fields are `pub` *and* there is a builder. So `with_encryption` can be feature-gated
(`:1554`) while `config.encrypt = true` (`:1296`) is always available; `with_net` panics past
`MAX_NETS` (`:1420`) while a struct literal does not check at all. The crate's own tests build
`Config { … }` literally (`:1578`). Add `#[non_exhaustive]`, keep the builder as the only path, and
give the cap one behaviour instead of the current three (`with_net` panics, `add_net` returns
`false` + warns, `set_nets` does **no check at all** — `replica.rs:535`).

### F-R6 · Hooks: `add_*` is a setter, and it under-informs — P1

`add_pre_insert` (`:559`) and `add_on_update` (`read_replica_map.rs:245`) are both
`*slot.write() = Box::new(f)` — a **setter named `add_`**. A second call silently discards the first,
and the doc at `:489` says the opposite. There is no removal and no composition.

The payload is also too thin for the obvious use cases: `Fn(&K, &Entry<Timestamp, V>)` sees only the
*new* entry — no previous value, and no way to distinguish a local write (`replica.rs:614`) from a
remote merge (`:1217`) from a persistence-restore replay (`src/replicated_map.rs:360`). Cache
invalidation and secondary indexes need all three. And because the hook runs on the receive path
(`replica.rs:1217`), a slow hook stalls reception for **every** peer — documented for deadlock, never
for latency.

**Fix:** rename to `set_pre_insert` (or make it a real multi-hook registry returning a `HookId`);
widen to `Fn(&K, Option<&Entry<…>>, &Entry<…>, ChangeOrigin)`; and add
`subscribe() -> broadcast::Receiver<Change<K, V>>` — the Hazelcast `EntryListener` / Pekko
`Subscribe` equivalent — so consumers are decoupled from the receive loop.

### F-R7 · Durability knobs are all private — P1

`SNAPSHOT_INTERVAL` is a 5 s private const (`:49`) with no knob and no manual flush, so a durable
deployment loses up to 5 s of writes on every restart regardless of backend. Aborting the `run`
handle drops mid-round with no final `snapshot()`. `MAX_CLOCK_DRIFT`/`HlcClock::with_max_clock_drift`
(`clock.rs:178`, carrying `#[allow(dead_code)]`) — the budget governing both clock clamping *and*
tombstone expiry — is likewise unreachable; the code flags its own gap at `src/replicated_map.rs:43-45`.

**Fix:** `Config::with_snapshot_interval`, `Config::with_max_clock_drift`, `snapshot_now()`, and
`run(self, shutdown: CancellationToken) -> RunOutcome` flushing on the way out.

### F-R8 · README contradicts the crate docs on replay protection — P1

README:136-141 states the keyed modes provide "no replay protection (replaying a captured datagram is
benign)". `src/lib.rs:51-57` and `gossip/src/replay.rs` say a cluster key enables per-sender replay
protection plus a freshness window. One of the two is wrong, on the security section — and the code
agrees with `lib.rs`. Rewrite README:136-141; the "no per-peer identity / no forward secrecy" caveats
next to it remain true.

Relatedly, the README's 9 `rust` fences are never compiled and several no longer do (README:45-49
lacks an async context and types; README:105 has a `/* … */` placeholder; README:256 references an
undefined `dated_addr`). `#![doc = include_str!("../README.md")]` makes them CI-checked.

### F-R9 · `ReadReplicaMap` is a second-class citizen — P1

| Capability | `ReplicatedMap` | `ReadReplicaMap` | Verdict |
|---|---|---|---|
| `new` / `new_with_transport` | ✓ `:219`/`:250` | ✓ `:155`/`:182` | symmetric |
| Read API (12 methods) | ✓ | ✓ | symmetric — the one well-aligned surface |
| Write API | ✓ | ✗ | by design (sink) |
| `node_id()` / `local_addr()` | ✓ `:295` / ✗ | ✗ / ✗ | gap |
| Persistence | ✓ `:315` | ✗ **none** | gap — a read replica always cold-starts empty |
| Discovery (5 builders) | ✓ `:428-474` | ✗ **none** | **major gap** — cannot be deployed on k8s at all |
| `seed_peer` (runtime) | ✓ `:409` | ✗ (`with_seed` only) | gap |
| Nets | 5 methods `:949-974` | `set_net`/`net`, single net | gap + naming |
| `set_reconcile_interval` | ✓ `:997` | ✗ — private `ACTIVITY_TIMEOUT` (`:75`) | gap |
| Change hook | `Fn(&K, &Entry<Timestamp,V>)` `:495` | `Fn(&K, &State<V>)` `:244` | name, payload **and** timing all differ |
| `fingerprint` | dated `:562` | **value-only** `:286` | **same name, different semantics** |
| `start_reconciliation` | `(&self)` `:881` | `(&self, &mut Vec<u8>)` `:415` | signature asymmetry; leaks a scratch buffer |
| `Config` fields honoured | 16 of 16 | **7 of 16**, rest silently ignored | gap |
| `Debug` | ✗ | ✗ | symmetric gap |

Two of these deserve fixing on their own: `ReadReplicaMap::fingerprint` should be renamed
`value_fingerprint` (today `dated.fingerprint(r) == replica.fingerprint(r)` is *always* false and
looks like divergence), and the silently-ignored `Config` fields should either warn or move to a
separate `ReadReplicaConfig`.

### F-R10 · Silent-failure inventory — P1

The paths where something goes wrong and nobody finds out:

| Path | Current behaviour | What a dependent needs |
|---|---|---|
| Persistence `save` fails | `warn!` only (`:380`) | counter + `on_persistence_error` callback + `last_snapshot_at` |
| Value > `65507 −` auth overhead | `warn!` on send; **the key never converges on any peer** (documented `:721-731`) | `reconcile_values_oversized_total` + `Config::with_max_value_size` rejecting at *write* time |
| Discovery round fails | `debug!`, round skipped (`:1067`) | `reconcile_discovery_failures_total` + `last_successful_discovery_at` |
| `set_nets` past `MAX_NETS` | no check at all (`replica.rs:535`) | validate, return `Result` |
| Write-path backpressure | none: one detached task per write, message vec cloned per peer, unbounded in flight | bounded egress queue + depth gauge + `try_insert(..) -> Result<_, Backpressure>` |
| `ReadReplicaMap::start_reconciliation` encode | bare `.unwrap()` (`:423`) | `.expect(...)`, matching `replica.rs:906` |

The datagram-drop path (bad MAC / replay / peer-cap / malformed → `reconcile_datagrams_dropped_total`
with a `reason` label) is the best-instrumented code in the crate and is the model the rows above
should follow.

### F-R11 · Metric names are private; the useful gauges do not exist — P2

Every metric name is a `pub(crate) const` (`observability.rs:42-56`), so dashboards and alert rules
have no stable, discoverable name set. All 14 metrics are counters or histograms: there is no gauge
for peers, members, live entries, tombstones, in-flight bulk dumps, or persistence failures — i.e.
none of the quantities an operator actually alerts on.

---

## 7. Prioritised backlog

Ordered by dependent cost, not by implementation cost. "⚠" marks findings with correctness or
security consequences beyond ergonomics.

### P0 — fix before the first split-aware release ([#204](https://github.com/Akvize/reconcile-rs/issues/204))

| # | Finding | Crate | One-line fix |
|---|---|---|---|
| 1 | ⚠ F-S1 `PartialEq` on fingerprint alone, unbounded `Eq` | `rsos` | `self.root.subtree == other.root.subtree`; bound `Eq` |
| 2 | F-S2 `range(&R)` E0716 on runtime bounds | `rsos` | take `R` by value |
| 3 | ⚠ F-S3 `with_mut` leaves stale aggregates on panic | `rsos` | `Drop`-guard the propagation |
| 4 | ⚠ F-B1 `RsosView` states no inter-method laws → peer-driven panic | `rbsr` | document the four laws; clamp before `select` |
| 5 | ⚠ F-R1 writes panic outside a Tokio runtime | `reconcile` | `Config::with_runtime_handle`; `# Panics` now |
| 6 | ⚠ F-R2 `get`-then-`insert` self-deadlocks | `reconcile` | `get_cloned` + `# Deadlock` on all seven |
| 7 | F-R3 no readiness / peers / local address | `reconcile` | `sync_state()`, `peers()`, `members()`, `local_addr()` |
| 8 | ⚠ F-P2 `with_discovery`'s precondition is a `debug_assert!` | `reconcile` | real check returning `Result` |
| 9 | F-P6 `Mac` unimplementable ⇒ key rotation impossible | `gossip` | `Tag::new`/`as_bytes`, `expose_secret`, `also_accept` |

### P1 — before 1.0

`rsos` F-S4 (Serialize over-bounding) · F-S5 (`Borrow<Q>` lookup) · F-S6 (summary law on `Rsos`) ·
F-S7 (closure-shaped `entry`) · F-S8 (`Fingerprint` byte API) · F-S9 (iterator trait stack, incl.
`ItemRange`, which #92 does not cover).
`rbsr` F-B2 (`RangeAggregate` accessors + constructor ⇒ unblocks subspace reconciliation) · F-B3 (wire
contract in rustdoc) · F-B4 (`RoundOutcome`) · F-B5 (`RoundLimits`) · F-B6 (document the one-way
door) · F-B7 (a standalone test + doc example).
ports F-P1 (`Addr`, `async-trait`) · F-P3 (`Discovery` error/timeout) · F-P4 (decide `Clock`:
inject or unpublish) · F-P5 (`Persistence`: `type Error`, async, `flush`, non-panicking load) ·
F-P7 (`ClusterKey` at the boundary ⇒ zeroize becomes real).
`reconcile` F-R4 (`Config::new(port)`) · F-R5 (`#[non_exhaustive]`, one construction path) ·
F-R6 (hooks) · F-R7 (`with_snapshot_interval`, `snapshot_now`, graceful `run`) · F-R8 (README
security contradiction + compiled examples) · F-R9 (read-replica parity) · F-R10 (silent failures).
cross-cutting X1 · X2 · X5 · X6.

### P2 — hygiene

X3 (MSRV, docs.rs) · X4 (keywords/categories) · F-S10 · F-B8 · F-P8 · F-R11, plus `#[must_use]` on
every builder and bool-returning mutator (the facade has **zero** today; the workspace has six, all in
`rsos`/`lww-register`).

### Suggested sequencing

1. **One mechanical PR:** X1 (`deny(missing_docs)` — clean on `rsos` today, one warning across
   `gossip`+`lww-register`), X3, X4, `#[must_use]`, `#[non_exhaustive]`. No behaviour change, and it
   stops the class from regrowing, per AGENTS.md §10.
2. **`rsos` correctness PR:** P0 items 1–3 + F-S4. All are small, and F-S4 unblocks the rest.
3. **Port contracts PR:** items 4, 8, 9 + F-P1/F-P3/F-P5/F-P7. This is the pre-publication one — these
   are the signatures that become frozen at 1.0.
4. **Facade operability PR:** items 5–7 + F-R7. Largest, and the one that decides whether `reconcile`
   is deployable by someone who did not write it.

---

## 8. Dispatch — where every finding now lives

Filed 2026-08-10. Every finding above is routed; nothing is left only in this document. This table is
the index — the issues carry the live state, this file carries the evidence.

| Findings | Destination | Kind |
|---|---|---|
| F-S1, F-S2, F-S3 | [#282](https://github.com/Akvize/reconcile-rs/issues/282) `rsos`: three P0 defects | new |
| F-P2, F-R1, F-R2 | [#283](https://github.com/Akvize/reconcile-rs/issues/283) `reconcile`: three P0 defects | new |
| F-B1, F-B6, F-S6 | [#284](https://github.com/Akvize/reconcile-rs/issues/284) contract laws | new |
| F-P6 | [#285](https://github.com/Akvize/reconcile-rs/issues/285) `gossip`: auth extension surface | new, child of #137 |
| F-P7 | [#286](https://github.com/Akvize/reconcile-rs/issues/286) `ClusterKey` at the boundary | new |
| F-P1, F-P3, F-P8, X6 | [#287](https://github.com/Akvize/reconcile-rs/issues/287) `Transport`/`Discovery` contracts | new |
| F-P4 | [#288](https://github.com/Akvize/reconcile-rs/issues/288) `Clock`: inject or unpublish | new — **a decision** |
| F-B2, F-B3, F-B7, F-B8 | [#289](https://github.com/Akvize/reconcile-rs/issues/289) `rbsr`: public wire contract | new |
| F-S4, F-S5, F-S7, F-S8, F-S10 | [#290](https://github.com/Akvize/reconcile-rs/issues/290) `rsos`: std parity | new |
| F-S9 (`ItemRange` half) | [#291](https://github.com/Akvize/reconcile-rs/issues/291) | new, child of #92 |
| F-R3, F-R7 | [#292](https://github.com/Akvize/reconcile-rs/issues/292) lifecycle & introspection | new |
| F-R4, F-R5 | [#293](https://github.com/Akvize/reconcile-rs/issues/293) `Config` ergonomics | new |
| F-R9 | [#294](https://github.com/Akvize/reconcile-rs/issues/294) `ReadReplicaMap` parity | new |
| F-R10, F-R11 | [#295](https://github.com/Akvize/reconcile-rs/issues/295) silent failures & gauges | new |
| F-R6 | [#296](https://github.com/Akvize/reconcile-rs/issues/296) hooks are setters | new, child of #79 |
| X2 | [#297](https://github.com/Akvize/reconcile-rs/issues/297) foreign types in public signatures | new |
| §4.1 (b) | [#298](https://github.com/Akvize/reconcile-rs/issues/298) generic monoid `Aggregate<M>` | new — **non-additive** |
| conditional writes | [#299](https://github.com/Akvize/reconcile-rs/issues/299) decide, do not implement | new, split from #180 |
| X1 | [#72](https://github.com/Akvize/reconcile-rs/issues/72) widened to the lint on all five roots | extended |
| F-S9 (six iterators) | [#92](https://github.com/Akvize/reconcile-rs/issues/92) widened to the current set | extended |
| X3, X4 | [#189](https://github.com/Akvize/reconcile-rs/issues/189) widened to workspace release metadata | extended |
| F-P5 | [#202](https://github.com/Akvize/reconcile-rs/issues/202) widened to the port contract | extended |
| X5 | [#205](https://github.com/Akvize/reconcile-rs/issues/205) `#[non_exhaustive]`, `#[must_use]` | extended |
| F-B4, F-B5, §4.1 (a) | [#257](https://github.com/Akvize/reconcile-rs/issues/257) `RoundOutcome`, `RoundLimits` | extended |
| §4.1 (c) | [#185](https://github.com/Akvize/reconcile-rs/issues/185) #257 named as prerequisite | extended |
| F-R8 | README "Security model" | **fixed on this branch** |

Findings whose durable home is documentation, not a ticket (each is an acceptance criterion of the
issue beside it): the four `RsosView` laws and the `Rsos` summary law → `ARCHITECTURE.md` §5 and the
trait rustdoc (#284); the `Clock` decision → §3.2's port table (#288); the `RangeAggregate` byte
layout → the type's own rustdoc (#289).

Already tracked before this audit, and unchanged by it:
[#150](https://github.com/Akvize/reconcile-rs/issues/150) (peers cap),
[#170](https://github.com/Akvize/reconcile-rs/issues/170)–[#173](https://github.com/Akvize/reconcile-rs/issues/173)
(performance), [#230](https://github.com/Akvize/reconcile-rs/issues/230) (value-size ceiling — F-R10
row 2 restates it), [#270](https://github.com/Akvize/reconcile-rs/issues/270)/[#271](https://github.com/Akvize/reconcile-rs/issues/271)
(zero-copy iteration).

### 8.1 Housekeeping this audit surfaced

- **Done but not closed:** [#233](https://github.com/Akvize/reconcile-rs/issues/233)
  (`ReplicatedMap::node_id()` exists, `src/replicated_map.rs:295`) and
  [#234](https://github.com/Akvize/reconcile-rs/issues/234) (`Value` no longer carries `PartialEq`,
  `lww-register/src/bounds.rs`). [#138](https://github.com/Akvize/reconcile-rs/issues/138) is
  effectively complete (6/7 children closed, step 6 landed but unticked).
- **Stale type names in six open titles** — `ReconcileStore`/`HRTree` exist nowhere in the code
  (#72, #92, #179, #180, #193, #233).
- **#2 vs #230:** the June rescope collapsed #2 onto #230's guard rail, so both now describe the same
  work. Revert #2 to its original fragmentation scope or close it as superseded.
- **PRs #218–#221: never merged — investigated and resolved, 2026-08-10.** The audit initially read
  these as fixes that had landed and vanished, because their bodies are written in landed tense
  ("MSRV declared: `rust-version = "1.85"`", "`LoadError` re-exported at the crate root",
  "Closes #189"). They are proposals inside unmerged PRs. Git pickaxe over *all* history returns
  **zero** hits for every identifier they introduce (`rust-version`, `LoadError`, `/ready`,
  `reconcile_internal_testing`, `with_snapshot_change_threshold`), so nothing was reverted and the
  workspace split is exonerated. Root cause: a **7-deep stacked chain** in which every PR targeted
  the previous feature branch rather than `main`, left open ~2 months while `main` moved; closed
  unmerged in one reasoned sweep on 2026-08-07. #189/#202/#205 stayed open throughout and remain
  accurate. The branches are live on the remote — recoverable, and best re-landed fresh against
  current `main` (#217→#248 is the precedent), not rebased through the stack.
  **Not systematic:** 10 of 10 spot-checked merged PRs survive intact as ancestors of `origin/main`.
  Two things are worth carrying forward: a PR body written in landed tense is not evidence of a
  merge, and the GitHub API's `merged` boolean is unreliable here (it reads `false` for plainly
  landed PRs) — `merged_at` is the discriminator.

---

## 9. Deliberate non-goals, re-examined

Three absences are argued for in the codebase. Two hold as stated; one does not.

- **No public `Encoding` port** (`ARCHITECTURE.md` §7). **Holds.** One implementation, no
  test-driven need for a second, and reintroducing it later is additive. Nothing found here changes
  that.
- **No `entry()` on `FingerprintTreeMap`** (`fingerprint_tree_map.rs:358-362`). **Holds for a raw
  `&mut V`; does not hold as stated.** The rationale points at `ReplicatedMap::upsert`/
  `get_or_insert_with` as the replacement, which lives in a crate an `rsos`-only dependent does not
  have — see F-S7. A closure-shaped `entry` keeps the invariant the objection is protecting.
- **`Clock` not injectable** (`src/replicated_map.rs:236-238`). **The risk is real, the asymmetry is
  not defensible.** `InMemoryTransport` is public specifically so downstream crates can test
  deterministically; the same argument applies to a clock, and today the port is re-exported with
  zero public implementors. Decide either way — see F-P4.

---

*Audit performed by reading the full public surface of all five crates and by compiling external
probe crates against `rsos`, `rbsr`, `gossip` and `lww-register` out of workspace. Claims marked
"verified" were reproduced by compilation or execution, not inferred: the `Eq` unboundedness, the
E0716 on `range`, the stale aggregates after a panicking `with_mut`, the `select` panic from an
inconsistent `RsosView`, the `E0119` on `RsosView`+`Rsos`, the unimplementable `Mac`, and a
standalone `rbsr` session converging two 200/250-key stores in 8 rounds.*

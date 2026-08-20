# rsos

A **Range-Summarizable Order-Statistics Store**: an ordered key-value map that also maintains, for
every subtree, a *range fingerprint*, so the hash of any key interval is available in `O(log n)`.

- `FingerprintTreeMap<K, V>` — the B-tree realization. Ordered map operations, plus
  `aggregate(range) -> Aggregate`: the bundled size-and-fingerprint summary of a key interval,
  answered in one walk.
- `Aggregate` — that bundle as a type (Def. 3.5's `A(S) = (|S|, Σ(S))`): a commutative monoid under
  `+`, with `Aggregate::ZERO` as its identity.
- `Fingerprint` — a 256-bit summary (per-element BLAKE3, combined by addition modulo 2²⁵⁶), chosen
  over a 64-bit XOR for collision resistance and stability as a wire token.
- `encoding` — the injective, length-prefixed byte encoding those per-element hashes are computed
  from: a `serde::Serializer` writing straight into BLAKE3. It is what makes a fingerprint stable
  across Rust versions, platforms and endianness, and the reason the element bound is `Serialize`
  rather than `std::hash::Hash` (whose byte sequences Rust does not promise to keep stable, and
  which `HashMap`/`HashSet` do not implement at all).
- `Rsos<K>` — the trait stating the operations such a store must answer (Def. 3.9 of *Range-Based
  Set Reconciliation via Range-Summarizable Order-Statistics Stores*, Amparore,
  [arXiv:2603.19820](https://arxiv.org/abs/2603.19820)), with an associated `Value` type so a
  backend names its own value type.

This crate is a standalone leaf: no workspace dependencies, no async runtime, no sockets, no codec.
It is the data structure that makes range-based set reconciliation cheap, and is useful on its own
to anyone who needs cheap interval summaries over an ordered map.

Written for [`reconcile`](https://crates.io/crates/reconcile), which gossips a replicated map over
UDP; the reconciliation algorithm itself is in [`rbsr`](https://crates.io/crates/rbsr), written
against `Rsos` rather than against this type.

## License

Licensed under either of [Apache-2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT), at your option.

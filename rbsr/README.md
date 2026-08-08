# rbsr

**Range-Based Set Reconciliation** — the anti-entropy algorithm from
[arXiv:2212.13567](https://arxiv.org/abs/2212.13567) (Aljoscha Meyer, 2023), which lets two peers
find the difference between two large ordered sets in a small number of round-trips by comparing
fingerprints over shrinking key ranges and exchanging only the entries that actually differ.

- `initial_ranges` / `protocol_round` — the protocol driver and its `RangeAggregate` wire messages.
  `initial_ranges` produces the *outer range*; each `protocol_round` answers a batch of *active
  ranges* with the protocol's three outcomes — SKIP (resolved), IDLIST (an `EnumerationRange`, whose
  contents the caller sends), or SPLIT (*child ranges*, bounced back).
- `RsosView<K>` — the small read-only backend trait the driver is written against: the four
  range/order-statistics queries it actually needs. Blanket-implemented for every
  [`rsos::Rsos`](https://crates.io/crates/rsos) implementor, so it runs over any store that can
  answer them, not just one particular tree.

The crate-root docs carry a full correspondence table between the protocol vocabulary of
[arXiv:2603.19820](https://arxiv.org/abs/2603.19820) (Amparore, 2026 — RBSR over any
range-summarizable order-statistics store) and the items here, plus the two places this crate
deliberately instantiates Algorithm 1 differently (no enumeration threshold `t`; a `√n` rather than
fixed-`b` split fan-out).

Equality and emptiness are decided by interval **size**, not by hash, to stay collision-safe.

Its only dependency in this workspace is `rsos`. It knows nothing about how the segments travel
(no runtime, no sockets, no wire codec) and nothing about conflict resolution — those belong to the
caller. [`reconcile`](https://crates.io/crates/reconcile) is one such caller: it drives this walk
over UDP with last-write-wins merge.

## License

Licensed under either of [Apache-2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT), at your option.

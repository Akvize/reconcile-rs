# rbsr

**Range-Based Set Reconciliation** — the anti-entropy algorithm from
[arXiv:2212.13567](https://arxiv.org/abs/2212.13567) (Aljoscha Meyer, 2023), which lets two peers
find the difference between two large ordered sets in a small number of round-trips by comparing
fingerprints over shrinking key ranges and exchanging only the divergent entries.

- `start_diff` / `diff_round` — the protocol walk and its `RangeAggregate` wire messages.
- `RsosView<K>` — the small read-only backend trait the walk is written against: the four
  range/order-statistics queries it actually needs. Blanket-implemented for every
  [`rsos::Rsos`](https://crates.io/crates/rsos) implementor, so it runs over any store that can
  answer them, not just one particular tree.

Equality and emptiness are decided by interval **size**, not by hash, to stay collision-safe.

Its only dependency in this workspace is `rsos`. It knows nothing about how the segments travel
(no runtime, no sockets, no wire codec) and nothing about conflict resolution — those belong to the
caller. [`reconcile`](https://crates.io/crates/reconcile) is one such caller: it drives this walk
over UDP with last-write-wins merge.

## License

Licensed under either of [Apache-2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT), at your option.

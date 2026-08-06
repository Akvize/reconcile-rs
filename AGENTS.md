# Agent guidelines for reconcile-rs

Conventions for anyone (human or agent) making changes to this crate. These are in addition to
[`CONTRIBUTING.md`](./CONTRIBUTING.md) (workflow) and [`ARCHITECTURE.md`](./ARCHITECTURE.md)
(structure); this file is about how code in this crate should be shaped.

## Strong-type every wire/domain entity — no bare primitives for meaningful values

A value that means something specific in the protocol or the domain (a sequence number, a
timestamp, a key, a MAC tag, a cluster secret) must be its own newtype, never passed around as a
bare `u64`/`[u8; N]`/`String`. Two bare `u64` parameters of the same type are a bug waiting to
happen (nothing stops a caller from swapping `seq` and `stamp`); two distinct newtypes make that
swap a compile error.

**Precedent already in the crate**: [`Timestamp`](./src/clock.rs) (HLC timestamp: `wall_ms`,
`counter`, `node_id`), [`ClusterKey`](./src/auth.rs) and [`Tag`](./src/auth.rs) (MAC key/output),
[`Seq` and `Stamp`](./src/replay.rs) (replay-header sequence number and sender wall-clock stamp).
None of these are exposed as raw integers or byte arrays past the boundary where they are parsed
off the wire.

## Every entity owns its own validation

The type that represents a value is the *only* place that decides whether a given instance of that
value is well-formed, in-range, or acceptable — never the caller. Concretely:

- Parsing/encoding lives on the type (`Seq::from_le_bytes`/`to_le_bytes`, not free functions that
  take/return `u64`).
- A check that is conceptually "is this value acceptable" is a method on the type, not arithmetic
  redone at each call site. Example: [`Stamp::is_fresh`](./src/replay.rs) is the *only* place the
  freshness-window comparison is written; [`ReplayFilter`](./src/replay.rs) calls it rather than
  reimplementing `now.saturating_sub(stamp) > window_ms` itself. If you find the same validation
  arithmetic duplicated at two call sites, that arithmetic belongs on the type instead.
- Constructing an invalid instance should be structurally impossible, or at least funneled through
  one obviously-fallible constructor — not something every caller has to remember to check.

This is the same "parse, don't validate" principle already called out for [`Payload`](./src/auth.rs)
(a `Payload` can only be obtained from `Authenticator::open`, so unauthenticated bytes can never
reach message handling) and for [`Entry`](./ARCHITECTURE.md#36-domain-types-and-conflict-policy)
(merge semantics live on the type, not scattered across call sites). Apply it to every entity, not
just the ones that happen to be security-critical.

## When adding a new wire field or protocol value

1. Give it a newtype in the module that owns its semantics (usually the module that already parses
   or generates it), not in the module that happens to consume it first.
2. Put its encode/decode and any acceptance/validation logic on that type.
3. Only reach for a bare primitive at the actual wire boundary (the byte array a `to_le_bytes`
   writes into) — everywhere else in the call chain, pass the typed value.

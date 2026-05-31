# reconcile-rs

[![Crates.io][crates-badge]][crates-url]
[![MIT licensed][mit-badge]][mit-url]
[![Apache licensed][apache-badge]][apache-url]
[![Build Status][actions-badge]][actions-url]
[![Docs Status][docs-badge]][docs-url]

[crates-badge]: https://img.shields.io/crates/v/reconcile.svg
[crates-url]: https://crates.io/crates/reconcile
[mit-badge]: https://img.shields.io/badge/license-MIT-blue.svg
[mit-url]: https://github.com/Akvize/reconcile-rs/blob/master/LICENSE-MIT
[apache-badge]: https://img.shields.io/badge/license-APACHE-blue.svg
[apache-url]: https://github.com/Akvize/reconcile-rs/blob/master/LICENSE-APACHE
[actions-badge]: https://github.com/Akvize/reconcile-rs/actions/workflows/master.yml/badge.svg
[actions-url]: https://github.com/Akvize/reconcile-rs/actions/workflows/master.yml
[actions-badge]: https://github.com/Akvize/reconcile-rs/actions/workflows/master.yml/badge.svg
[actions-url]: https://github.com/Akvize/reconcile-rs/actions/workflows/master.yml
[docs-badge]: https://docs.rs/reconcile/badge.svg
[docs-url]: https://docs.rs/reconcile/latest/reconcile/

This crate provides a key-data map structure `HRTree` that can be used together
with the reconciliation `ReconcileStore`. Different instances can talk together over
UDP to efficiently reconcile their differences.

All the data is available locally on all instances, and the user can be
notified of changes to the collection with an insertion hook.

The protocol allows finding a difference over millions of elements with a limited
number of round-trips. It should also work well to populate an instance from
scratch from other instances.

The intended use case is a scalable Web service with a non-persistent and
eventually consistent key-value store. The design enable high performance by
avoiding any latency related to using an external store such as Redis.

![Architecture diagram of a scalable Web service using reconcile-rs](img/illustration.png)

In code, this would look like this:

```rust
let mut store = ReconcileStore::new(Config::default()).await;
tokio::spawn(store.clone().run());
// use the reconciliation store as a key-value store in the API
```

## Security model

> **By default, the UDP reconciliation protocol is _unauthenticated_.** Any host that can send a
> UDP datagram to the port can forge an update — including one with a year-9999 timestamp that
> wins against every legitimate write forever, or a forged tombstone that deletes a key — and the
> poison propagates to the whole cluster through last-write-wins. UDP source addresses are
> spoofable, so this is anonymous.

To close this vector, provide a shared 32-byte cluster secret on **every** node:

```rust
let key: [u8; 32] = /* same secret on all nodes, e.g. loaded from your secret manager */;
let config = Config::default().with_cluster_key(key);
let store = ReconcileStore::new(config).await;
```

With a key set, every outgoing datagram is framed with a per-datagram keyed MAC over its payload,
and every incoming datagram is **verified before deserialization**; datagrams with a missing or
invalid tag are silently dropped. When no key is set, the store logs a loud warning at startup and
runs unauthenticated.

The MAC primitive is selected at build time via Cargo features: `mac-blake3` (default, keyed
BLAKE3) or `mac-hmac` (HMAC-SHA256). All nodes in a cluster must share the identical key **and** be
built with the same backend. The optional `zeroize` feature wipes the cluster key from memory when
it is dropped (defense in depth).

**Scope.** This provides message integrity and authenticity. It does **not** provide
confidentiality (the payload is not encrypted — see issue #96), replay protection (replaying a
captured datagram is benign for idempotent last-write-wins reconciliation), or a peer allow-list.
For confidentiality, run the protocol over a trusted/encrypted underlay.

## HRTree

The core of the protocol is made possible by the `HRTree` (Hash-Range Tree) data structure, which
allows `O(log(n))` access, insertion and removal, as well as `O(log(n))`
cumulated hash range-query. The latter property enables querying
the cumulated fingerprint of all key-value pairs between two keys. The fingerprint is a
256-bit BLAKE3-per-element hash combined by addition modulo 2²⁵⁶ — a stable, portable wire
token, chosen over a 64-bit XOR for collision resistance and cross-version interoperability
(see `src/fingerprint.rs`).

Although we did come we the idea independently, it exactly matches a paper
published on Arxiv in February 2023: [Range-Based Set
Reconciliation](https://arxiv.org/abs/2212.13567), by Aljoscha Meyer

Our implementation of this data structure is based on a B-Trees that we wrote
ourselves. Although we put a limited amount of effort in this
and have to maintain more invariants, we stay within a factor 2 of the
standard `BTreeMap` from the standard library:

![Graph of the time needed to insert N elements in an empty tree](img/perf-fill.png)

The graph above shows the amount of time **in milliseconds** (ordinate, left
axis) needed to **insert N elements** (abscissa, bottom axis) in a tree
(initially empty). Note that both axes use a logarithmic scale.

The performance of our `HRTree` implementation follows closely that of
`BTreeMap`. When looking at each value of N, we see that the average throughput
of the `HRTree` is between one third and one half that of `BTreeMap`.

![Graph of the time needed to insert and remove 1 element in a tree of size N](img/perf-insert.png)

The graph above shows the amount of time **in nanoseconds** (abscissa, bottom
axis) needed to **insert a single element** (and remove it) in a tree
containing N elements (ordinate, bottom axis). Note that both axes use a
logarithmic scale.

The most important thing to notice is that the average insertion/removal time
only grows from 80 ns to 700 s although the size of the tree changes from 10 to
1,000,000 elements.

![Graph of the time needed to remove and restore 1 element in a tree of size N](img/perf-remove.png)

The graph above shows the amount of time **in nanoseconds** (abscissa, bottom
axis) needed to **remove a single element** (and restore it) from a tree
containing N elements (ordinate, bottom axis). Note that both axes use a
logarithmic scale.

The most important thing to notice is that the average removal/insertion time
only grows from 100 ns to 800 s although the size of the tree changes from 10 to
1,000,000 elements.

![Graph of the time needed to compute 1 hash of a range of elements in a tree of size N](img/perf-hash.png)

The graph above shows the amount of time **in microseconds** (abscissa, bottom
axis) needed to compute **1 cumulated hash** over a random range of elements in a
tree of size N (ordinate, bottom axis). Note that both axes use a logarithmic
scale.

The average time per cumulated hash grows from 30 ns to 1,200 ns as the size of
the tree changes from 10 to 1,000,000 elements.

Although there is likely still a lot of room for improvement regarding the
performance of the `HRTree`, it is quite enough for our purposes, since we
expect network delays to be orders of magnitude longer.

## ReconcileStore

The ReconcileStore exploits the properties of `HRTree` to conduct a binary-search-like
search in the collections of the two instances. Once difference are found, the
corresponding key-value pairs are exchanged and conflicts are resolved.

![Graph of the time needed to send 1 insertion and 1 removal](img/perf-send.png)

The graph above shows the amount of time **in microseconds** (abscissa, bottom
axis) needed to send 1 insertion, then 1 removal** between two instances of
`ReconcileStore` that contain the same N elements (ordinate, left axis). Note that
both axes use a logarithmic scale.

The times are very consistent, hovering around 122 µs, showing that the
reconciliation time is entirely bounded by the local network transmission. This
is made possible by the immediate transmission of the element at
insertion/removal.

![Graph of the time to reconcile 1 difference between two instances](img/perf-reconcile.png)

The graph above shows the amount of time **in milliseconds** (abscissa, bottom
axis) needed to reconcile 1 insertion, then 1 removal** between two instances of
`ReconcileStore` that contain the same other N elements (ordinate, left axis). Note that
both axes use a logarithmic scale.

This time, the full reconciliation protocol must be run to identify the
difference. The times grow from 240 µs to 640 µs as the size of the collection
changes from 10 to 1,000,000 elements.

**Note:** These benchmarks are performed locally on the loop-back network
interface. On a real network, transmission delays will make the values larger.

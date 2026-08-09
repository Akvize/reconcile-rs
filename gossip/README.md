# reconcile-gossip

> ## ⚠ Implementation detail — no stability guarantee
>
> **Do not depend on this crate directly. Depend on
> [`reconcile`](https://crates.io/crates/reconcile)**, which re-exports the parts meant for
> consumers (`reconcile::UdpTransport`, `reconcile::Transport`, `reconcile::Discovery`, …).
>
> This crate publishes to crates.io for one reason: cargo has no vendoring, so `reconcile` cannot be
> published unless every crate it depends on is published too — the same reason `serde_derive`,
> `pin-project-internal` and `tracing-attributes` are on the registry. **Anything here may change
> or disappear in any release**, including a patch release, with no deprecation period and no
> mention in `reconcile`'s changelog. Several items are `pub` only so the facade can reach them
> across the crate boundary; they were private before the workspace split and are not supported API.
> Current publish status: [`PROGRESS.md`](https://github.com/Akvize/reconcile-rs/blob/main/PROGRESS.md).

The gossip adapter layer of [`reconcile-rs`](https://github.com/Akvize/reconcile-rs) — everything a
replica needs to *talk* to its peers, and nothing about what it says:

- the `Transport` port over datagrams, with `UdpTransport` for real sockets and
  `InMemoryTransport`/`InMemoryNetwork` for deterministic socket-free tests;
- the bincode wire-encoding functions, with a bounded message count per datagram;
- per-datagram MAC authentication and optional XChaCha20-Poly1305 encryption over a shared cluster
  key, plus per-sender replay protection (sequence window + freshness stamp);
- the `Discovery` port with its `RandomProbe` and `DnsDiscovery` adapters.

It deliberately does **not** depend on the domain crate: nothing here knows what an `Entry`, a
`Timestamp` or a `Key` is. A datagram is a byte slice and a peer is an address.

## Name

The package is `reconcile-gossip` because `gossip` is taken on crates.io. The crate is still
`gossip` in every dependent's source, via cargo's dependency renaming:

```toml
gossip = { package = "reconcile-gossip", version = "0.1.0" }
```

## License

Licensed under either of [Apache-2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT), at your option.

# lww-register

> ## ⚠ Implementation detail — no stability guarantee
>
> **Do not depend on this crate directly. Depend on
> [`reconcile`](https://crates.io/crates/reconcile)**, which re-exports everything here that is
> meant for consumers (`reconcile::Entry`, `reconcile::Timestamp`, `reconcile::Persistence`, …).
>
> This crate is on crates.io for one reason: cargo has no vendoring, so `reconcile` cannot be
> published unless every crate it depends on is published too — the same reason `serde_derive`,
> `pin-project-internal` and `tracing-attributes` are on the registry. **Anything here may change
> or disappear in any release**, including a patch release, with no deprecation period and no
> mention in `reconcile`'s changelog. It is versioned against `reconcile`'s API, not its own, and
> is shaped entirely by what `reconcile` needs — it is not offered as a general-purpose
> LWW-Register.

The state-based last-write-wins register domain of
[`reconcile-rs`](https://github.com/Akvize/reconcile-rs): `Entry`/`State` (the register cell, its
tombstone-aware state and the LWW merge rule), `Timestamp` plus the `Clock` port and the hybrid
logical clock ordering arithmetic, the `Persistence` port with `PersistedState` and
`InMemoryPersistence`, and the `Key`/`Value` bound bundles.

It is **infrastructure-free by construction**: its manifest declares exactly one dependency,
`serde`'s derive, so no async runtime, socket, wire codec or wall clock can be imported here — the
build fails rather than the boundary rotting. The physical-time read and the file-backed
persistence adapter both live outside it, behind the `Clock` and `Persistence` ports.

## License

Licensed under either of [Apache-2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT), at your option.

## Examples of how to use `reconcile`

This directory showcases various capabilities of the `reconcile` crate.

The [`demo`](demo.rs) example can be executed with:

```
cargo run --release --example demo 8080 127.0.0.1 127.0.0.0/30 100000
```

The [`k8s`](k8s/) directory is a complete, turnkey Kubernetes example — an env-driven node
([`k8s/main.rs`](k8s/main.rs), run with `cargo run --example k8s`), manifests, a `Dockerfile`, and
a local [kind](https://kind.sigs.k8s.io/) playground. See [`k8s/README.md`](k8s/README.md).

If you've got an example you'd like to see here, please feel free to open an
issue. Otherwise if you've got an example you'd like to add, please feel free
to make a PR!

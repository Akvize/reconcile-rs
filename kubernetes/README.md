# Kubernetes example for reconcile-rs

This directory is a **complete, turnkey example** of running reconcile-rs on Kubernetes. It is
example and deployment scaffolding — *not* part of the library — and is excluded from the published
crate. The library itself lives in [`../src/`](../src/); the runnable node binaries it builds on
live in [`../examples/`](../examples/).

## Layout

| Path | What it is |
| --- | --- |
| [`Dockerfile`](Dockerfile) | Multi-stage build of a node image from an `examples/*.rs` binary (selectable via the `EXAMPLE` build arg). Build context is the repo root: `docker build -f kubernetes/Dockerfile .` |
| [`base/`](base/) | The production manifests: a headless `Service`, a `ConfigMap`, a `StatefulSet`, and an example `Secret`. Point these at your own registry image and apply them to a real cluster. |
| [`kind/`](kind/) | A local [kind](https://kind.sigs.k8s.io/) playground: a thin kustomize overlay on `base/` plus `up.sh`/`down.sh`, for spinning the whole thing up on your laptop. **Start here to try it out** — see [`kind/README.md`](kind/README.md). |

## The node binaries

Both are env-driven and discover their peers by resolving the headless `Service` over DNS (no
Kubernetes API access, no RBAC):

- [`../examples/k8s_node.rs`](../examples/k8s_node.rs) — the production node. Runs the store and
  exposes `/metrics` for the kubelet probes; the `Dockerfile`'s default `EXAMPLE`.
- [`../examples/k8s_heartbeat.rs`](../examples/k8s_heartbeat.rs) — the same node plus a periodic
  per-pod heartbeat write and a hook that logs reconciled keys, so convergence between pods is
  visible directly in `kubectl logs`. The `kind/` playground builds this one.

## Quick start (local)

```sh
./kubernetes/kind/up.sh     # build + load the image, create a kind cluster, deploy 5 pods
./kubernetes/kind/down.sh   # tear it all down
```

See [`kind/README.md`](kind/README.md) for the full walkthrough and the concepts behind each step.

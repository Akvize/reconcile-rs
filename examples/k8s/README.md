# Kubernetes example for reconcile-rs

This directory is a **complete, turnkey example** of running reconcile-rs on Kubernetes. It is
example and deployment scaffolding — *not* part of the library — and is excluded from the published
crate. The library itself lives in [`../../src/`](../../src/).

## Layout

| Path | What it is |
| --- | --- |
| [`main.rs`](main.rs) | The example node itself. As a multi-file example its cargo target is the directory name: `cargo run --example k8s`. |
| [`Dockerfile`](Dockerfile) | Multi-stage build of the node image. Build context is the repo root: `docker build -f examples/k8s/Dockerfile .` |
| [`base/`](base/) | The production manifests: a headless `Service`, a `ConfigMap`, a `StatefulSet`, and an example `Secret`. Point these at your own registry image and apply them to a real cluster. |
| [`kind/`](kind/) | A local [kind](https://kind.sigs.k8s.io/) playground: a thin kustomize overlay on `base/` plus `up.sh`/`down.sh`, for spinning the whole thing up on your laptop. **Start here to try it out** — see [`kind/README.md`](kind/README.md). |

## The node

[`main.rs`](main.rs) is env-driven and discovers its peers by resolving the headless `Service` over
DNS (no Kubernetes API access, no RBAC). It runs the store and exposes `/metrics` for the kubelet
probes. To make reconciliation observable it also runs a small **demo** behaviour — a periodic
per-pod heartbeat write plus a hook that logs reconciled keys, so you watch convergence directly in
`kubectl logs`. Those two blocks are fenced with `--- demo ---` markers; delete them for a bare
production node.

## Quick start (local)

```sh
./examples/k8s/kind/up.sh     # build + load the image, create a kind cluster, deploy 5 pods
./examples/k8s/kind/down.sh   # tear it all down
```

See [`kind/README.md`](kind/README.md) for the full walkthrough and the concepts behind each step.

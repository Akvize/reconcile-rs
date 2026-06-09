# Running reconcile-rs in a local kind cluster

This directory spins up a small, throwaway Kubernetes cluster on your own machine and runs
**5 `ReconcileStore` pods** in it. It's meant as a hands-on playground for learning Kubernetes,
using reconcile-rs as a realistic, stateful, peer-to-peer workload.

It does **not** introduce a separate deployment: it's a thin
[kustomize](https://kustomize.io/) overlay on top of the production manifests in
[`../k8s/`](../k8s/). The overlay only patches what differs locally (the image and the replica
count); everything else — the StatefulSet, the headless Service, the ConfigMap — is reused as-is.
That's the idiomatic Kubernetes way to keep one set of manifests for both prod and local.

The image built here is the **`k8s_heartbeat`** example (`examples/k8s_heartbeat.rs`): the same node
as production, plus two tiny additions that make reconciliation visible in the logs — each pod
periodically writes a `heartbeat/<pod-name>` key, and a hook logs every key the first time it
appears locally. Since the hook fires for peer-originated updates too, you watch each pod learn the
*other* pods' heartbeats as gossip converges. The production manifests in `../k8s/` still build the
plain `k8s_node` (no heartbeat, no demo behaviour).

## What you'll need

Install these on your machine (not provided by this repo):

| Tool | What it is | Install |
| --- | --- | --- |
| [Docker](https://docs.docker.com/get-docker/) | Builds the image; kind runs its nodes as Docker containers. | — |
| [kind](https://kind.sigs.k8s.io/docs/user/quick-start/#installation) | "Kubernetes IN Docker" — a full cluster made of containers. | `go install` / `brew` / binary |
| [kubectl](https://kubernetes.io/docs/tasks/tools/) | The Kubernetes CLI. | — |
| `openssl` | Generates the random cluster key. | usually already present |

## Quick start

From the repository root:

```sh
./deploy/kind/up.sh      # build + load image, create cluster, deploy 5 pods, wait for readiness
# ... experiment ...
./deploy/kind/down.sh    # delete the whole cluster
```

`up.sh` is idempotent — re-run it after changing the code to rebuild and roll out.

## What `up.sh` actually does (and why)

Each step maps to a Kubernetes concept worth understanding:

1. **`kind create cluster`** — creates a 3-node cluster (1 control-plane + 2 workers) from
   [`kind-config.yaml`](kind-config.yaml). Each node is just a Docker container on your laptop.
2. **`docker build --build-arg EXAMPLE=k8s_heartbeat`** — compiles `examples/k8s_heartbeat.rs` into
   the image, using the repo [`Dockerfile`](../../Dockerfile). (The `EXAMPLE` arg defaults to
   `k8s_node`; the playground overrides it to get the heartbeat/logging behaviour.)
3. **`kind load docker-image`** — copies the image *into* the cluster's nodes. This is the classic
   kind gotcha: the cluster nodes can't see your local Docker daemon, so an image you just built is
   invisible to them until you load it. (The overlay sets `imagePullPolicy: Never` so a forgotten
   load fails loudly with `ErrImageNeverPull` instead of silently trying a registry.)
4. **`kubectl create secret`** — generates a random 32-byte **cluster key** and stores it in a
   Kubernetes [Secret](https://kubernetes.io/docs/concepts/configuration/secret/). Every pod reads
   the same key (via `secretKeyRef`) and uses it to authenticate gossip datagrams. A real key never
   touches a committed file.
5. **`kubectl apply -k deploy/kind`** — applies the overlay: the headless Service, the ConfigMap,
   and the StatefulSet scaled to 5 replicas.
6. **`kubectl rollout status`** — waits until all pods pass their readiness probe (`GET /metrics`).

## How the pieces fit together

- The **StatefulSet** gives each pod a stable name (`reconcile-0` … `reconcile-4`) and stable DNS.
  `k8s_node` hashes the pod name into a stable node id, used for the clock tie-break — so a
  restarted pod keeps its identity.
- The **headless Service** (`clusterIP: None`) makes
  `reconcile-headless.default.svc.cluster.local` resolve to one IP **per ready pod**. The node's
  `DnsDiscovery` re-resolves that name every few seconds to find its peers — no Kubernetes API
  access, no RBAC.
- Pods then **gossip over UDP** (port 8080) and reconcile their `ReconcileStore<String, String>`
  contents. Port 9000 serves `/metrics`, which doubles as the readiness/liveness probe.

## See reconciliation happen

This is the payoff: each pod only ever writes *its own* heartbeat key, so any **other** pod's
heartbeat that shows up in a node's log can only have arrived through gossip reconciliation.

```sh
# Follow one pod's log. As the cluster converges you'll see it announce keys it didn't write
# itself — heartbeat/reconcile-1, heartbeat/reconcile-2, … — i.e. peers' data reconciled in.
kubectl logs reconcile-0 -f | grep "store now holds"
```

Expected, after a few seconds (order varies):

```
INFO ... key=heartbeat/reconcile-0  store now holds this key (local write or reconciled from a peer)
INFO ... key=heartbeat/reconcile-3  store now holds this key (local write or reconciled from a peer)
INFO ... key=heartbeat/reconcile-1  store now holds this key (local write or reconciled from a peer)
...
```

Every replica converges to the same five `heartbeat/*` keys. The data crosses pods purely over the
gossip protocol, which is authenticated by the shared cluster key.

## Other things to try

```sh
# See the 5 pods and which node each landed on.
kubectl get pods -o wide

# Watch a node discover its peers over DNS and reconcile their heartbeats.
kubectl logs reconcile-0 -f

# Scale up and watch the new pods get discovered; scale down and watch them drop out.
kubectl scale statefulset/reconcile --replicas=7
kubectl scale statefulset/reconcile --replicas=3

# Delete a pod and watch the StatefulSet recreate it with the SAME name and identity
# (and re-sync its state from peers on restart).
kubectl delete pod reconcile-2

# Inspect the gossip/reconciliation counters a node exposes.
kubectl port-forward pod/reconcile-0 9000:9000 &
curl -s localhost:9000/metrics | grep '^reconcile_'   # messages_sent/received, rounds, etc.
```

## Cleaning up

```sh
./deploy/kind/down.sh
```

This deletes the kind cluster entirely; nothing persists outside it.

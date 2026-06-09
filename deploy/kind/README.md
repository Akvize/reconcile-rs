# Running reconcile-rs in a local kind cluster

This directory spins up a small, throwaway Kubernetes cluster on your own machine and runs
**5 `ReconcileStore` pods** in it. It's meant as a hands-on playground for learning Kubernetes,
using reconcile-rs as a realistic, stateful, peer-to-peer workload.

It does **not** introduce a separate deployment: it's a thin
[kustomize](https://kustomize.io/) overlay on top of the production manifests in
[`../k8s/`](../k8s/). The overlay only patches what differs locally (the image, the replica count,
and the demo HTTP port); everything else — the StatefulSet, the headless Service, the ConfigMap —
is reused as-is. That's the idiomatic Kubernetes way to keep one set of manifests for both prod and
local.

The image built here is the **`k8s_kv`** example (`examples/k8s_kv.rs`): the same node as
production, plus a tiny **HTTP key/value API on port 8081**, so you can write to one pod and watch
the value reconcile to the others. The production manifests in `../k8s/` still build the plain
`k8s_node` (no write API).

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
2. **`docker build --build-arg EXAMPLE=k8s_kv`** — compiles `examples/k8s_kv.rs` into the image,
   using the repo [`Dockerfile`](../../Dockerfile). (The `EXAMPLE` arg defaults to `k8s_node`; the
   playground overrides it to get the demo HTTP API.)
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
  contents. Port 9000 serves `/metrics` (the readiness/liveness probe), and port 8081 serves the
  demo HTTP key/value API.

## See reconciliation happen

This is the payoff: write a key to **one** pod and read it back from **another**.

```sh
# Forward the HTTP API of two different pods to two local ports.
kubectl port-forward pod/reconcile-0 8081:8081 &
kubectl port-forward pod/reconcile-4 8082:8081 &

# Write a key to pod 0.
curl -X PUT -d 'bonjour' localhost:8081/kv/greeting

# Read it back from pod 4 — it arrived there purely via gossip reconciliation.
sleep 2
curl localhost:8082/kv/greeting        # -> bonjour
curl localhost:8082/kv                 # list every key this replica knows (incl. reconciled ones)

# Deletes propagate too (as tombstones).
curl -X DELETE localhost:8081/kv/greeting
sleep 2
curl -i localhost:8082/kv/greeting     # -> 404
```

The HTTP API (`PUT`/`GET`/`DELETE /kv/<key>`, `GET /kv`) is an unauthenticated demo surface — the
gossip protocol between pods is still authenticated by the shared cluster key.

## Other things to try

```sh
# See the 5 pods and which node each landed on.
kubectl get pods -o wide

# Watch a node discover its peers over DNS as the cluster forms.
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

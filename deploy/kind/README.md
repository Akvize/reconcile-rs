# Running reconcile-rs in a local kind cluster

This directory spins up a small, throwaway Kubernetes cluster on your own machine and runs
**5 `ReconcileStore` pods** in it. It's meant as a hands-on playground for learning Kubernetes,
using reconcile-rs as a realistic, stateful, peer-to-peer workload.

It does **not** introduce a separate deployment: it's a thin
[kustomize](https://kustomize.io/) overlay on top of the production manifests in
[`../k8s/`](../k8s/). The overlay only patches the two things that differ locally (the image and
the replica count); everything else — the StatefulSet, the headless Service, the ConfigMap — is
reused as-is. That's the idiomatic Kubernetes way to keep one set of manifests for both prod and
local.

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
2. **`docker build`** — compiles `examples/k8s_node.rs` into the image, using the repo
   [`Dockerfile`](../../Dockerfile).
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

## Things to try

```sh
# See the 5 pods and which node each landed on.
kubectl get pods -o wide

# Watch a node discover its peers over DNS as the cluster forms.
kubectl logs reconcile-0 -f

# Scale up and watch the new pods get discovered; scale down and watch them drop out.
kubectl scale statefulset/reconcile --replicas=7
kubectl scale statefulset/reconcile --replicas=3

# Delete a pod and watch the StatefulSet recreate it with the SAME name and identity.
kubectl delete pod reconcile-2

# Inspect the gossip/reconciliation counters a node exposes.
kubectl exec reconcile-0 -- /usr/local/bin/k8s_node --help 2>/dev/null || true
kubectl port-forward pod/reconcile-0 9000:9000 &
curl -s localhost:9000/metrics | grep '^reconcile_'   # messages_sent/received, rounds, etc.
```

> Note: `k8s_node` runs an empty `String -> String` store with no external write API, so the data
> all five nodes converge on is (correctly) empty. What you can observe here is the **cluster
> behaviour** — discovery, gossip traffic, stable identity, scaling, probe-driven readiness. To see
> keys actually replicate between nodes, look at the `demo` example (`examples/demo.rs`) or extend
> `k8s_node` with an HTTP endpoint that calls `store.insert(...)`.

## Cleaning up

```sh
./deploy/kind/down.sh
```

This deletes the kind cluster entirely; nothing persists outside it.

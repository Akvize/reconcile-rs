#!/usr/bin/env bash
# Bring up a local kind cluster running 5 ReconcileStore pods.
#
# Idempotent: safe to re-run. It will (re)build the image, (re)load it into kind, ensure the
# cluster key Secret exists, and (re)apply the manifests.
#
# Requirements: docker, kind, kubectl, openssl.
set -euo pipefail

CLUSTER=reconcile
IMAGE=reconcile:kind
# Resolve paths relative to this script so it works from any working directory.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' is required but not installed" >&2; exit 1; }; }
need docker; need kind; need kubectl; need openssl

echo "==> Ensuring kind cluster '$CLUSTER' exists"
if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  echo "    cluster already exists"
else
  kind create cluster --config "$SCRIPT_DIR/kind-config.yaml"
fi
# Point kubectl at this cluster.
kubectl config use-context "kind-$CLUSTER" >/dev/null

echo "==> Building the node image ($IMAGE) from the repo Dockerfile"
echo "    (first build compiles the Rust release binary — this can take a few minutes)"
# Build the k8s_kv example: same node as production, plus a demo HTTP key/value API on port 8081
# so you can write to one pod and watch the value reconcile to the others.
docker build --build-arg EXAMPLE=k8s_kv -t "$IMAGE" "$REPO_ROOT"

echo "==> Loading the image into kind (its nodes can't see your local Docker daemon)"
kind load docker-image "$IMAGE" --name "$CLUSTER"

echo "==> Ensuring the cluster-key Secret exists"
if kubectl get secret reconcile-secret >/dev/null 2>&1; then
  echo "    secret 'reconcile-secret' already exists (leaving it as is)"
else
  kubectl create secret generic reconcile-secret \
    --from-literal=cluster-key="$(openssl rand -hex 32)"
  echo "    created 'reconcile-secret' with a fresh random 32-byte key"
fi

echo "==> Applying the kind overlay (5 replicas)"
kubectl apply -k "$SCRIPT_DIR"

echo "==> Waiting for pods to become ready"
kubectl rollout status statefulset/reconcile --timeout=180s

echo
echo "Done. Try:"
echo "  kubectl get pods -o wide                     # the 5 pods, spread across nodes"
echo "  kubectl logs reconcile-0 -f                  # watch a node discover its peers over DNS"
echo "  kubectl port-forward pod/reconcile-0 8081 &  # then: curl -X PUT -d hi localhost:8081/kv/x"
echo "  kubectl port-forward pod/reconcile-4 8082:8081 &  # then: curl localhost:8082/kv/x  (replicated!)"
echo "  ./deploy/kind/down.sh                        # tear the whole cluster down"

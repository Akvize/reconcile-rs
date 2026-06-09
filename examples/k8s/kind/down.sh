#!/usr/bin/env bash
# Delete the local kind cluster created by ./up.sh. This removes everything (pods, secret, the
# cluster itself); nothing persists outside the cluster.
set -euo pipefail

CLUSTER=reconcile

if command -v kind >/dev/null 2>&1 && kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  echo "==> Deleting kind cluster '$CLUSTER'"
  kind delete cluster --name "$CLUSTER"
else
  echo "kind cluster '$CLUSTER' not found; nothing to do"
fi

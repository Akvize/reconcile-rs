#!/usr/bin/env bash
# Enforces the hexagonal-architecture invariant documented in ARCHITECTURE.md §2.2:
# the domain mechanism (data structure + protocol algorithm) imports no infrastructure
# crate — no async runtime, no socket, no wire codec, no wall clock. This is a real
# invariant today (not aspirational), so we gate on it rather than let it silently rot
# as the ports/adapters migration (#138) proceeds.
#
# Add a module to DOMAIN_FILES only once ARCHITECTURE.md documents it as infra-free.
set -Eeuo pipefail

# Resolve the repo root from the script's own location (not `git rev-parse`, since the
# pre-commit hook runs this against a bare `git checkout-index` copy with no `.git`).
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

DOMAIN_FILES=(
    src/hrtree.rs
    src/hrtree_iter.rs
    src/fingerprint.rs
    src/reconcilable.rs
    src/bounds.rs
    src/proto.rs
)

# Infrastructure crates/modules the domain must never import directly.
FORBIDDEN='^\s*use\s+(tokio|bincode|chrono|ipnet|mio|reqwest|hyper|std::net)\b'

status=0
for f in "${DOMAIN_FILES[@]}"; do
    if [ ! -f "$f" ]; then
        echo "check-domain-purity: $f listed but missing — update the script" >&2
        status=1
        continue
    fi
    if hits=$(grep -nE "$FORBIDDEN" "$f"); then
        echo "check-domain-purity: infrastructure import(s) in domain module $f:" >&2
        echo "$hits" >&2
        status=1
    fi
done

if [ "$status" -ne 0 ]; then
    echo >&2
    echo "Domain modules must stay infrastructure-free (ARCHITECTURE.md §2.2/§3.3)." >&2
    echo "Route the dependency through a port/adapter instead, or move the code out of the domain." >&2
fi

exit "$status"

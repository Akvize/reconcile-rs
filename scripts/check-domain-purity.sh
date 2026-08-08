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
    src/entry.rs
    src/bounds.rs
    src/proto.rs
)
# `src/hrtree.rs`, `src/hrtree_iter.rs` (now `rsos/src/fingerprint_tree_map{,_iter}.rs`) and
# `src/fingerprint.rs` moved to the standalone `rsos`
# crate (workspace split step A) — dropped from this list the same way `reconcilable.rs` was
# dropped when `entry.rs` took its place. `rsos` has its own, stronger invariant (zero dependency
# on anything reconciliation-domain-specific, enforced today by its Cargo.toml's dependency list
# alone); a grep-based analog for `rsos`/the future `rbsr` crate is tracked as a later step, not
# added here.

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

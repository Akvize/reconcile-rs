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
    lww-register/src/lib.rs
    lww-register/src/bounds.rs
    lww-register/src/clock.rs
    lww-register/src/entry.rs
    lww-register/src/persistence.rs
)
# The whole domain now lives in one crate, `lww-register` (workspace split step C): `entry.rs` and
# `bounds.rs` moved there wholesale, `clock.rs` contributed `Timestamp`/`Clock`/the HLC ordering
# arithmetic (the chrono-reading `HlcClock` adapter stayed behind in `src/clock.rs`), and
# `persistence.rs` contributed the `Persistence` port, `PersistedState` and `InMemoryPersistence`
# (the `std::fs`-touching `FileSnapshot` went to the `snapshot` crate).
#
# `lww-register/Cargo.toml` already blocks the crate-level edge — it names no infrastructure
# dependency, so `use tokio::…` there would not compile. This script covers what that manifest
# cannot: an infrastructure type reached through a *re-export* of some allowed dependency, which
# compiles fine and silently breaches the boundary. That intra-crate sub-boundary is the reason to
# keep the check after the split, and the reason it lists every file in the crate rather than a
# hand-picked subset — a new module in `lww-register` is domain by construction.
#
# `src/hrtree.rs`, `src/hrtree_iter.rs` and `src/fingerprint.rs` moved to the standalone `rsos`
# crate (step A, where the first two are now `rsos/src/fingerprint_tree_map{,_iter}.rs`, joined by
# `rsos/src/aggregate.rs`), and `src/proto.rs` to the standalone `rbsr` crate (step B, now
# `rbsr/src/diff.rs` + `rbsr/src/rsos_view.rs`). Both carry their own, stronger invariant (zero
# dependency on anything reconciliation-domain-specific, enforced today by their Cargo.toml
# dependency lists alone); a grep-based analog for `rsos`/`rbsr` is tracked as a later step, not
# added here.

# Infrastructure crates/modules the domain must never import directly.
FORBIDDEN='^\s*use\s+(tokio|bincode|chrono|ipnet|mio|reqwest|hyper|std::net)\b'

# The one carve-out from `std::net`: its plain address *value* types. `IpAddr` and friends are data
# — `PersistedState`'s causal-stability membership set is literally a set of peer identities, and a
# peer's identity is its address — whereas `UdpSocket`/`TcpStream`/`ToSocketAddrs` are the actual
# I/O machinery this check exists to keep out. An import qualifies only if *every* name it brings in
# is on this list, so `use std::net::{IpAddr, UdpSocket};` is still rejected.
# (Making peer identity an opaque domain newtype instead of an address would remove the need for
# this carve-out; that is a separate change, not part of the workspace split.)
NET_VALUE_TYPES='(IpAddr|Ipv4Addr|Ipv6Addr|SocketAddr|SocketAddrV4|SocketAddrV6|AddrParseError)'
# Note the leading `[0-9]+:` — this is matched against `grep -n` output, which carries a line-number
# prefix.
ALLOWED="^[0-9]+:[[:space:]]*use[[:space:]]+std::net::(\{[[:space:]]*)?(${NET_VALUE_TYPES}([[:space:]]*,[[:space:]]*)?)+[[:space:]]*\}?[[:space:]]*;[[:space:]]*$"

status=0
for f in "${DOMAIN_FILES[@]}"; do
    if [ ! -f "$f" ]; then
        echo "check-domain-purity: $f listed but missing — update the script" >&2
        status=1
        continue
    fi
    if hits=$(grep -nE "$FORBIDDEN" "$f" | grep -vE "$ALLOWED"); then
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

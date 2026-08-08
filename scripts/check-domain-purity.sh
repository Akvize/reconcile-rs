#!/usr/bin/env bash
# Enforces the hexagonal-architecture invariant documented in ARCHITECTURE.md §2.2:
# the domain mechanism (data structure + protocol algorithm) imports no infrastructure
# crate — no async runtime, no socket, no wire codec, no wall clock. This is a real
# invariant today (not aspirational), so we gate on it rather than let it silently rot
# as the ports/adapters migration (#138) proceeds.
#
#
# Two parts, two different levels of the same invariant:
#   1. a source grep over `lww-register/src/*.rs` — is an *allowed* dependency being used to
#      reach something forbidden, through a re-export? (a manifest cannot see this)
#   2. a manifest gate over `rsos`/`rbsr`/`lww-register`'s own `Cargo.toml`s — can the forbidden
#      thing be reached at all? (a source grep there could never fire: with the dependency
#      undeclared, the import would not compile in the first place)
#
# Add a module to DOMAIN_FILES / a manifest to STANDALONE_MANIFESTS only once ARCHITECTURE.md
# documents it as infra-free.
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
# (the `std::fs`-touching `FileSnapshot` stayed on the adapter side, in `reconcile`'s
# `src/snapshot.rs`).
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
    echo >&2
fi

manifest_status=0

# ---------------------------------------------------------------------------
# Part 2: the manifest gate for the standalone crates.
#
# The source grep above answers "is an *allowed* dependency being used to reach something
# forbidden, through a re-export?". This part answers the prior question: "can the forbidden
# thing be reached at all?" — i.e. is the dependency even declared. That is where the invariant
# is actually breakable for `rsos`/`rbsr`/`lww-register`: none of them declares an
# infrastructure crate today, so `use tokio::…` there does not compile, and a source-level grep
# for such imports could never fire. The step that *would* make it compile is someone adding the
# dependency to the manifest — so that is what is gated here.
#
# Deliberately scoped to these three manifests only. The workspace root (`reconcile`) and
# `gossip` legitimately carry infrastructure dependencies; they are adapters, that is
# their job.
STANDALONE_MANIFESTS=(
    rsos/Cargo.toml
    rbsr/Cargo.toml
    lww-register/Cargo.toml
)

# Same infrastructure set as FORBIDDEN above, expressed as crate names rather than import paths,
# plus the two that only ever appear as dependencies (`socket2`, `async-trait`).
FORBIDDEN_DEPS='tokio|bincode|chrono|ipnet|mio|reqwest|hyper|socket2|async-trait'


# Section-scoped scan, no TOML parser: track the current `[section]`, and inside any
# `[…dependencies]` table flag a forbidden key (`tokio = …`, `tokio.workspace = true`) or a
# forbidden `package = "…"` rename target. `[dependencies.tokio]`-style tables are matched on the
# header itself. Comments are stripped first so the explanatory prose in these manifests — which
# names these very crates — does not trip the check.
for m in "${STANDALONE_MANIFESTS[@]}"; do
    if [ ! -f "$m" ]; then
        echo "check-domain-purity: $m listed but missing — update the script" >&2
        manifest_status=1
        continue
    fi
    crate_dir=$(dirname "$m")
    if ! awk -v forb="^(${FORBIDDEN_DEPS})\$" \
             -v crate="$crate_dir" '
        { line = $0; sub(/#.*$/, "", line) }
        line ~ /^[[:space:]]*\[/ {
            hdr = line
            sub(/^[[:space:]]*\[+/, "", hdr)
            sub(/\]+.*$/, "", hdr)
            gsub(/[[:space:]"'"'"']/, "", hdr)
            insec = (hdr ~ /(^|\.)(dependencies|dev-dependencies|build-dependencies)$/)
            if (hdr ~ /(^|\.)(dependencies|dev-dependencies|build-dependencies)\.[A-Za-z0-9_.-]+$/) {
                nm = hdr
                sub(/^.*dependencies\./, "", nm)
                if (nm ~ forb) {
                    printf "  line %d: [%s]\n", NR, hdr; bad = 1
                }
            }
            next
        }
        insec && line ~ /=/ {
            key = line
            sub(/=.*$/, "", key)
            gsub(/[[:space:]"'"'"']/, "", key)
            sub(/\..*$/, "", key)
            if (key ~ forb) {
                printf "  line %d: %s\n", NR, key; bad = 1
            }
            if (match(line, /package[[:space:]]*=[[:space:]]*"[^"]+"/)) {
                pkg = substr(line, RSTART, RLENGTH)
                sub(/^[^"]*"/, "", pkg)
                sub(/".*$/, "", pkg)
                if (pkg ~ forb) {
                    printf "  line %d: %s (renamed dependency)\n", NR, pkg; bad = 1
                }
            }
        }
        END { exit bad ? 1 : 0 }
    ' "$m"; then
        echo "check-domain-purity: forbidden infrastructure dependency in $m (see above)" >&2
        manifest_status=1
    fi
done

if [ "$manifest_status" -ne 0 ]; then
    echo >&2
    echo "rsos/rbsr/lww-register must stay standalone: no async runtime, socket, wire codec or" >&2
    echo "wall clock in their manifests (ARCHITECTURE.md §2.2, AGENTS.md §9.2). Put the adapter" >&2
    echo "in gossip or reconcile instead." >&2
    status=1
fi

exit "$status"

#!/usr/bin/env bash
# Mechanical public-API gate (#311). Two rules, both scoped to committed-file diffs — no published
# registry baseline needed, unlike `cargo-semver-checks` (#311's rule 1, sequenced separately: it
# needs a baseline and 4 of 5 crates have never published one).
#
#   rule 2: a crate's public API only changes when a human meant it to. `cargo public-api` renders
#           each crate's API as text; a committed snapshot under public-api/ is the "meant to"
#           record, and this script fails on any diff against it — the same shape as a stale
#           Cargo.lock. `--bless` regenerates the snapshots after a deliberate change.
#   rule 3: `rbsr` is deliberately held at 0.x (AGENTS.md §11, #308) — nothing in `reconcile`'s
#           public API may name an `rbsr` symbol, because that would couple `rbsr`'s semver to
#           `reconcile`'s major retroactively (#308's "The measurement is not self-enforcing").
#           Checked against the live `cargo public-api` output, not the snapshot, so a rule-3
#           break is reported on its own terms even when `--bless` is passed. Two sub-checks, since
#           `cargo public-api`'s rendered text hides where a re-exported item was *defined*:
#             a. a type used directly in a public signature still prints its defining path (e.g.
#                `-> rbsr::protocol::RangeAggregate`) — caught by grepping the rendered text.
#             b. `pub use rbsr::Something;` prints only the destination path (`reconcile::Something`)
#                once re-exported, hiding the origin — caught instead from rustdoc JSON's `index`,
#                whose `use` items carry the pre-rename `source` path verbatim.
#
# Snapshots use default features only: `cargo public-api` renders one feature selection per
# invocation, and default features is what `cargo add reconcile` resolves to — the shape most
# consumers see. A feature-gated symbol entering the *default* public API is what this gate exists
# to catch; `cargo doc --all-features` (AGENTS.md §3) already gates the all-features build.
#
# Needs a `nightly` toolchain (for rustdoc's unstable `--output-format json`) and `cargo-public-api`
# (`cargo install cargo-public-api`) — AGENTS.md §2. CI-only tier (AGENTS.md §3): a full workspace
# rustdoc build per crate is well over `pre-push`'s ~20 s budget.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

# crate directory -> (package name, snapshot file). Directory "." is the workspace root package,
# "reconcile" — kept out of the pair list below so the loop can still glob a real directory.
CRATES=(rsos rbsr lww-register gossip .)
declare -A PKG_NAME=(
    [rsos]=rsos
    [rbsr]=rbsr
    [lww-register]=lww-register
    [gossip]=reconcile-gossip
    [.]=reconcile
)

BLESS=0
if [ "${1:-}" = "--bless" ]; then
    BLESS=1
fi

if ! command -v cargo-public-api >/dev/null 2>&1; then
    echo "check-public-api: cargo-public-api not installed — 'cargo install cargo-public-api' (AGENTS.md §2)" >&2
    exit 1
fi

mkdir -p public-api

status=0
reconcile_api=""
for dir in "${CRATES[@]}"; do
    pkg="${PKG_NAME[$dir]}"
    snapshot="public-api/${pkg}.txt"
    echo "check-public-api: rendering $pkg" >&2
    if ! current=$(cargo public-api -p "$pkg" --simplified 2>/tmp/check-public-api.$$.log); then
        cat /tmp/check-public-api.$$.log >&2
        rm -f /tmp/check-public-api.$$.log
        echo "check-public-api: 'cargo public-api -p $pkg' failed (see above)" >&2
        status=1
        continue
    fi
    rm -f /tmp/check-public-api.$$.log

    if [ "$pkg" = "reconcile" ]; then
        reconcile_api="$current"
    fi

    if [ "$BLESS" -eq 1 ]; then
        printf '%s\n' "$current" >"$snapshot"
        continue
    fi

    if [ ! -f "$snapshot" ]; then
        echo "check-public-api: no snapshot for $pkg at $snapshot — run './scripts/check-public-api.sh --bless'" >&2
        status=1
        continue
    fi

    if ! diff -u "$snapshot" <(printf '%s\n' "$current"); then
        echo "check-public-api: $pkg's public API changed — if this is deliberate, run" >&2
        echo "'./scripts/check-public-api.sh --bless' and commit the updated $snapshot" >&2
        status=1
    fi
done

# Rule 3, checked against the live render (not the snapshot, and not skipped by --bless): a
# blessed snapshot must not be able to launder a rule-3 break.
rule3_hit=0
if [ -n "$reconcile_api" ] && grep -qE '(^|[^A-Za-z0-9_])rbsr::' <<<"$reconcile_api"; then
    echo "check-public-api: reconcile's public API signatures name an rbsr:: symbol:" >&2
    grep -E '(^|[^A-Za-z0-9_])rbsr::' <<<"$reconcile_api" >&2
    rule3_hit=1
fi

# Sub-check b: rustdoc JSON left behind by the `cargo public-api -p reconcile` call above, at the
# path `rustdoc-json` (cargo-public-api's own dependency) always writes to.
reconcile_json="target/doc/reconcile.json"
if [ -f "$reconcile_json" ]; then
    if reexport_hits=$(python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    doc = json.load(f)
for item in doc.get("index", {}).values():
    src = item.get("inner", {}).get("use", {}).get("source", "")
    if src == "rbsr" or src.startswith("rbsr::"):
        print(src)
' "$reconcile_json" 2>/dev/null) && [ -n "$reexport_hits" ]; then
        echo "check-public-api: reconcile re-exports rbsr symbol(s) directly:" >&2
        echo "$reexport_hits" >&2
        rule3_hit=1
    fi
fi

if [ "$rule3_hit" -eq 1 ]; then
    echo "rbsr is deliberately 0.x (AGENTS.md §11, #308) — route this through a reconcile-owned" >&2
    echo "type, or reopen the version-line decision." >&2
    status=1
fi

if [ "$BLESS" -eq 1 ] && [ "$status" -eq 0 ]; then
    echo "check-public-api: snapshots regenerated under public-api/ — review the diff before committing" >&2
fi

exit "$status"

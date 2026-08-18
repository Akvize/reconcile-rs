#!/usr/bin/env bash
# .cargo/mutants.toml's header comment claims two counts for `cargo mutants --list --workspace
# --all-features`: the mutant count as configured, and the count with the file's own
# exclude_globs/exclude_re/skip_calls ignored (via --no-config). The delta between them is what
# proves the exclusions still do what their comments claim; the baseline itself moves with every
# commit that adds or removes mutable code and is not otherwise checked.
#
# `--list` is pure mutant-site discovery -- syntactic, no build -- so both invocations are
# sub-second even cold (measured: ~0.4s each on this workspace). AGENTS.md §10: a number a human
# must re-measure and paste in by hand belongs in a script instead, once re-measuring it is this
# cheap.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

TOML=.cargo/mutants.toml

command -v cargo-mutants >/dev/null || { echo "check-mutant-count: cargo-mutants is required" >&2; exit 1; }

claimed_configured=$(grep -oE 'yields [0-9]+' "$TOML" | grep -oE '[0-9]+')
claimed_unconfigured=$(grep -oE '\([0-9]+ without the exclusions' "$TOML" | grep -oE '[0-9]+')

if [ -z "$claimed_configured" ] || [ -z "$claimed_unconfigured" ]; then
    echo "check-mutant-count: could not parse the claimed counts out of $TOML" >&2
    exit 1
fi

actual_configured=$(cargo mutants --list --workspace --all-features | wc -l)
actual_unconfigured=$(cargo mutants --list --workspace --all-features --no-config | wc -l)

status=0

if [ "$actual_configured" -ne "$claimed_configured" ]; then
    echo "check-mutant-count: $TOML claims $claimed_configured mutants, cargo-mutants finds $actual_configured" >&2
    status=1
fi

if [ "$actual_unconfigured" -ne "$claimed_unconfigured" ]; then
    echo "check-mutant-count: $TOML claims $claimed_unconfigured mutants without exclusions, cargo-mutants finds $actual_unconfigured" >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "check-mutant-count: $TOML's counts ($claimed_configured / $claimed_unconfigured) match cargo-mutants"
else
    echo >&2
    echo "Update the two numbers (and the delta, if it changed) in $TOML's header comment to match." >&2
fi

exit "$status"

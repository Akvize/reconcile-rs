#!/usr/bin/env bash
# Enforces size budgets on this repo's prose docs: the agent instruction files
# (cumulative), and SOTA.md (its own, separate cap).
#
# AGENTS.md + CLAUDE.md are budgeted *cumulatively*, not per-file, because that is how
# the two are consumed: CLAUDE.md opens with `@AGENTS.md`, which imports the whole file
# verbatim, so any reader — human or agent — gets the sum. Budgeting them separately
# would let the pair grow without bound while each file looked disciplined.
#
# Why a budget at all: these two files are read in full, every time, before any work
# happens. Past that length they stop being read closely, which is the failure mode
# AGENTS.md's own preamble names — "This file states rules, not rationale" — and the
# reason it links out instead of explaining in place. The cap turns that intent into a
# failing command (AGENTS.md §10) rather than a habit someone has to remember.
#
# SOTA.md is a different kind of doc — a durable reference, not read in full every
# session (AGENTS.md §9) — so it gets its own cap rather than joining the sum above;
# mixing the two would either gate a reference doc on a read-every-time budget it was
# never meant to fit, or let read-every-time prose hide inside a much larger allowance.
# It is budgeted at all because "durable, not updated for routine changes" has still
# proven not to mean "never grows": PROGRESS.md's retirement redistributed prose here
# with nothing tracking the result (978 -> 1021 lines in that one change). The cap
# exists so that keeps being a decision made in the open, not untracked drift.
#
# When either budget fires, the fix is to *move* prose, not to compress it into denser
# prose:
#   - rationale, measurements and worked examples  -> CONTRIBUTING.md or ARCHITECTURE.md
#   - a rule someone must remember and apply by eye -> a script wired into a hook and CI
# Raising a cap is a deliberate decision, not the default remedy: change the constant
# below and say why in the commit.
set -Eeuo pipefail

AGENTS_MAX_LINES=200
SOTA_MAX_LINES=1100

# Resolve the repo root from the script's own location (not `git rev-parse`, since the
# pre-commit hook runs this against a bare `git checkout-index` copy with no `.git`).
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

AGENTS_FILES=(
    AGENTS.md
    CLAUDE.md
)

status=0

check_missing() {
    local f=$1
    if [ ! -f "$f" ]; then
        echo "check-doc-budget: $f listed but missing — update the script" >&2
        status=1
        return 1
    fi
    return 0
}

echo "AGENTS.md + CLAUDE.md (cumulative, read in full every session):"
total=0
for f in "${AGENTS_FILES[@]}"; do
    check_missing "$f" || continue
    n=$(wc -l <"$f")
    printf '  %-12s %4d\n' "$f" "$n"
    total=$((total + n))
done

if [ "$status" -ne 0 ]; then
    exit "$status"
fi

printf '  %-12s %4d / %d\n' "total" "$total" "$AGENTS_MAX_LINES"

if [ "$total" -gt "$AGENTS_MAX_LINES" ]; then
    echo >&2
    echo "check-doc-budget: AGENTS.md + CLAUDE.md are $total lines, over the $AGENTS_MAX_LINES-line budget" >&2
    echo "by $((total - AGENTS_MAX_LINES))." >&2
    echo >&2
    echo "Move prose out rather than compressing it: rationale and worked examples belong in" >&2
    echo "CONTRIBUTING.md or ARCHITECTURE.md, and a rule enforced by eye belongs in a script" >&2
    echo "wired into a hook and CI (AGENTS.md §10)." >&2
    echo >&2
    status=1
fi

echo
echo "SOTA.md (durable reference, own cap):"
if check_missing SOTA.md; then
    n=$(wc -l <SOTA.md)
    printf '  %-12s %4d / %d\n' "SOTA.md" "$n" "$SOTA_MAX_LINES"
    if [ "$n" -gt "$SOTA_MAX_LINES" ]; then
        echo >&2
        echo "check-doc-budget: SOTA.md is $n lines, over the $SOTA_MAX_LINES-line budget by" >&2
        echo "$((n - SOTA_MAX_LINES))." >&2
        echo >&2
        echo "Either move content that isn't a durable positioning/glossary/bibliography entry" >&2
        echo "elsewhere (AGENTS.md §1/§9), or raise SOTA_MAX_LINES above and say why in the" >&2
        echo "commit — growth is fine, silent growth is not." >&2
        echo >&2
        status=1
    fi
fi

exit "$status"

#!/usr/bin/env bash
# Enforces the size budget on the agent instruction files: AGENTS.md and CLAUDE.md
# together must stay at or under 200 lines.
#
# The budget is *cumulative*, not per-file, because that is how the two are consumed:
# CLAUDE.md opens with `@AGENTS.md`, which imports the whole file verbatim, so any
# reader — human or agent — gets the sum. Budgeting them separately would let the pair
# grow without bound while each file looked disciplined.
#
# Why a budget at all: these two files are read in full, every time, before any work
# happens. Past that length they stop being read closely, which is the failure mode
# AGENTS.md's own preamble names — "This file states rules, not rationale" — and the
# reason it links out instead of explaining in place. The cap turns that intent into a
# failing command (AGENTS.md §10) rather than a habit someone has to remember.
#
# When this fires, the fix is to *move* prose, not to compress it into denser prose:
#   - rationale, measurements and worked examples  -> CONTRIBUTING.md or ARCHITECTURE.md
#   - a rule someone must remember and apply by eye -> a script wired into a hook and CI
# Raising the cap is a deliberate decision, not the default remedy: change MAX_LINES here
# and say why in the commit.
set -Eeuo pipefail

# Raised from 200 (deliberately, per this script's own header comment): documenting the
# structural-relevance mechanism (AGENTS.md §3's new paragraph, CLAUDE.md's §3 pointer) as a rule
# rather than a habit needed 7 more lines than trimming elsewhere could recover without cutting
# content the budget script itself would flag as the wrong kind of compression.
MAX_LINES=207

# Resolve the repo root from the script's own location (not `git rev-parse`, since the
# pre-commit hook runs this against a bare `git checkout-index` copy with no `.git`).
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

BUDGETED_FILES=(
    AGENTS.md
    CLAUDE.md
)

status=0
total=0
for f in "${BUDGETED_FILES[@]}"; do
    if [ ! -f "$f" ]; then
        echo "check-doc-budget: $f listed but missing — update the script" >&2
        status=1
        continue
    fi
    n=$(wc -l <"$f")
    printf '  %-12s %4d\n' "$f" "$n"
    total=$((total + n))
done

if [ "$status" -ne 0 ]; then
    exit "$status"
fi

printf '  %-12s %4d / %d\n' "total" "$total" "$MAX_LINES"

if [ "$total" -gt "$MAX_LINES" ]; then
    echo >&2
    echo "check-doc-budget: AGENTS.md + CLAUDE.md are $total lines, over the $MAX_LINES-line budget" >&2
    echo "by $((total - MAX_LINES))." >&2
    echo >&2
    echo "Move prose out rather than compressing it: rationale and worked examples belong in" >&2
    echo "CONTRIBUTING.md or ARCHITECTURE.md, and a rule enforced by eye belongs in a script" >&2
    echo "wired into a hook and CI (AGENTS.md §10)." >&2
    echo >&2
    exit 1
fi

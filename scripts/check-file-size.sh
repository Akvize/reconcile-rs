#!/usr/bin/env bash
# Line-count budget per file, prod and test scored separately -- the mechanical replacement
# for `.claude/rules/big-files.md` (retired 5d71271 once replica.rs/replicated_map.rs were
# split): that rule was prose a human had to remember to re-check by eye, and by its own
# retirement commit's admission "never covered test files" -- exactly the kind of rule
# AGENTS.md §10 sends to a script instead.
#
# Two tiers per category: WARN is a visible nudge and still exits 0; FAIL blocks. Test files
# get a looser budget than prod on purpose -- a wide table of small, similar cases legitimately
# runs longer than the production code it exercises.
#
# PROD_FAIL/PROD_WARN and TEST_FAIL/TEST_WARN started at 500/300 and 900/600 respectively --
# tight enough for a *newly written* file, not a retroactive claim every current file already
# fit. #427's file-by-file EXCEPTIONS split (largest-file-first) ratchets these down as each
# entry clears: every clearance lowers both budgets by as much as is safe -- never past the
# largest remaining *non-exception* file for that category, so clearing one file never silently
# fails an unrelated one. rsos/src/fingerprint_tree_map.rs's split (#427) landed every sibling
# under 250L against a 482L largest-remaining-prod-file floor (src/replica.rs) and a 689L
# largest-remaining-test-file floor (tests/proptest_fingerprint_tree_map.rs) -- FAIL moved to
# 490/700, WARN (informational only, never fails the build) to 280/400. tests/service.rs's split
# (#427/#452 continuation) landed every sibling under 425L, but doesn't move FAIL/WARN further --
# tests/proptest_fingerprint_tree_map.rs (689L) was already the binding test-file floor before
# and after.
#
# EXCEPTIONS are files already over FAIL when this gate (or a tightened budget) was introduced,
# grandfathered rather than split as a side effect -- each is a candidate for its own
# #421/#425-style split, tracked separately (#427 is the umbrella issue; a specific split gets
# its own #423-style C-design sub-issue when someone is ready to take it). Adding to this list
# needs the same justification in its commit; per check-doc-budget.sh's precedent, it is not the
# default remedy for a file that grows past FAIL after today -- split the file first.
#
# The whitelist is never silent: every run prints its full contents (count + per-file line
# count), pass or fail, and a listed path that no longer exists or no longer needs the grant
# (shrank back under FAIL) fails the run until the entry is fixed -- an EXCEPTIONS line is a
# live claim, not a fire-and-forget opt-out.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

PROD_WARN=280
PROD_FAIL=490
TEST_WARN=400
TEST_FAIL=700

EXCEPTIONS=(
    "rsos/src/fingerprint_tree_map_iter.rs" # 907L prod  -- its iterator family
    "rbsr/src/protocol.rs"                  # 909L prod  -- the RBSR wire protocol state machine
    "gossip/src/auth.rs"                    # 977L prod  -- MAC backends + wire auth, security-sensitive
    "src/read_replica_map.rs"               # 826L prod  -- read-replica facade, mirrors ReplicatedMap's read half
    "gossip/src/replay.rs"                  # 749L prod  -- replay-window bookkeeping, security-sensitive
    "lww-register/src/clock.rs"             # 839L prod  -- HLC + AdmittedTime, load-bearing invariants (ARCHITECTURE §5)
    "rsos/src/encoding.rs"                  # 643L prod  -- the canonical encoding every fingerprint is defined over
    "rbsr/src/policy.rs"                    # 674L prod  -- RBSR split/recursion policy
)

is_exception() {
    local f=$1 e
    for e in "${EXCEPTIONS[@]}"; do
        [ "$f" = "$e" ] && return 0
    done
    return 1
}

status=0
scanned=0
warned=0
declare -A exception_lines # path -> line count, filled in as each is found on disk below

while IFS= read -r -d '' f; do
    rel=${f#./}
    case "$rel" in
    */benches/* | benches/* | */examples/* | examples/*) continue ;;
    */tests.rs | */tests/*.rs | tests/*.rs) category=test ;;
    *) category=prod ;;
    esac

    scanned=$((scanned + 1))
    n=$(wc -l <"$f")
    if [ "$category" = prod ]; then
        warn=$PROD_WARN
        fail=$PROD_FAIL
    else
        warn=$TEST_WARN
        fail=$TEST_FAIL
    fi

    if is_exception "$rel"; then
        exception_lines["$rel"]=$n
        if [ "$n" -le "$fail" ]; then
            echo "check-file-size: $rel is $n lines ($category), no longer over the $fail-line budget" \
                "-- remove it from EXCEPTIONS in scripts/check-file-size.sh" >&2
            status=1
        fi
        continue
    fi

    if [ "$n" -gt "$fail" ]; then
        echo "check-file-size: $rel is $n lines ($category), over the $fail-line hard-fail budget" >&2
        status=1
    elif [ "$n" -gt "$warn" ]; then
        echo "check-file-size: $rel is $n lines ($category), over the $warn-line warning budget (fails at $fail)"
        warned=$((warned + 1))
    fi
done < <(find . -name '*.rs' -not -path './target/*' -print0)

# Every EXCEPTIONS entry must correspond to a file this run actually found -- a stale path (the
# file renamed, moved, or deleted since) would otherwise silently grant nothing to anyone, which
# reads as "handled" when it is really "broken by drift" (AGENTS.md §9).
for e in "${EXCEPTIONS[@]}"; do
    if [ -z "${exception_lines[$e]:-}" ]; then
        echo "check-file-size: EXCEPTIONS lists '$e', which no longer exists" \
            "-- fix or remove the entry in scripts/check-file-size.sh" >&2
        status=1
    fi
done

# Always visible, pass or fail: a silent whitelist is the opposite of what replacing
# big-files.md's by-eye rule was for.
echo "check-file-size: ${#EXCEPTIONS[@]} files whitelisted (over hard-fail, grandfathered in EXCEPTIONS):"
for e in "${EXCEPTIONS[@]}"; do
    printf '  %-42s %s\n' "$e" "${exception_lines[$e]:-MISSING} lines"
done

if [ "$status" -eq 0 ]; then
    echo "check-file-size: $scanned files scanned, $warned over warning budget, none over hard-fail outside the whitelist above"
else
    echo >&2
    echo "Split the file (mirroring #421/#425) rather than raising FAIL. If it genuinely cannot" >&2
    echo "decompose further, add it to EXCEPTIONS in scripts/check-file-size.sh and say why in" >&2
    echo "the commit." >&2
fi

exit "$status"

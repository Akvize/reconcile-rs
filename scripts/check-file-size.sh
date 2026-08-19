#!/usr/bin/env bash
# Line-count budget per file, prod and test scored separately -- the mechanical replacement
# for `.claude/rules/big-files.md` (retired 5d71271 once replica.rs/replicated_map.rs were
# split): that rule was prose a human had to remember to re-check by eye, and by its own
# retirement commit's admission "never covered test files" -- exactly the kind of rule
# AGENTS.md §10 sends to a script instead.
#
# Two tiers per category: WARN is a visible nudge and still exits 0; FAIL blocks. Test files
# get a looser budget than prod on purpose -- a wide table of small, similar cases legitimately
# runs longer than the production code it exercises. Both anchor to this repo's own precedent:
# #421/#425 landed prod split-siblings under ~500 lines (run.rs, the largest, is 503, called
# out in #425 as the one seam that doesn't decompose further) and test split-siblings under
# ~460 (replicated_map/tests/membership.rs, 459).
#
# EXCEPTIONS are files already over FAIL when this gate was introduced, grandfathered rather
# than split as part of adding it -- each is a candidate for its own #421/#425-style split,
# tracked separately. Adding to this list needs the same justification in its commit; per
# check-doc-budget.sh's precedent, it is not the default remedy for a file that grows past
# FAIL after today -- split the file first.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

PROD_WARN=500
PROD_FAIL=800
TEST_WARN=600
TEST_FAIL=900

EXCEPTIONS=(
    "rsos/src/fingerprint_tree_map.rs"      # 1426L prod -- order-statistics tree, #(follow-up TBD)
    "rsos/src/fingerprint_tree_map_iter.rs" # 907L prod  -- its iterator family, same follow-up
    "rbsr/src/protocol.rs"                  # 854L prod  -- the RBSR wire protocol state machine
    "gossip/src/auth.rs"                    # 820L prod  -- MAC backends + wire auth, security-sensitive
    "tests/service.rs"                      # 1214L test -- top-level end-to-end oracle suite
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

while IFS= read -r -d '' f; do
    rel=${f#./}
    case "$rel" in
    */benches/* | benches/* | */examples/* | examples/*) continue ;;
    */tests.rs | */tests/*.rs | tests/*.rs) category=test ;;
    *) category=prod ;;
    esac

    scanned=$((scanned + 1))
    is_exception "$rel" && continue

    n=$(wc -l <"$f")
    if [ "$category" = prod ]; then
        warn=$PROD_WARN
        fail=$PROD_FAIL
    else
        warn=$TEST_WARN
        fail=$TEST_FAIL
    fi

    if [ "$n" -gt "$fail" ]; then
        echo "check-file-size: $rel is $n lines ($category), over the $fail-line hard-fail budget" >&2
        status=1
    elif [ "$n" -gt "$warn" ]; then
        echo "check-file-size: $rel is $n lines ($category), over the $warn-line warning budget (fails at $fail)"
        warned=$((warned + 1))
    fi
done < <(find . -name '*.rs' -not -path './target/*' -print0)

if [ "$status" -eq 0 ]; then
    echo "check-file-size: $scanned files scanned, $warned over warning budget, none over hard-fail"
else
    echo >&2
    echo "Split the file (mirroring #421/#425) rather than raising FAIL. If it genuinely cannot" >&2
    echo "decompose further, add it to EXCEPTIONS in scripts/check-file-size.sh and say why in" >&2
    echo "the commit." >&2
fi

exit "$status"

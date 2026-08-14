#!/usr/bin/env bash
# Every issue-state claim a document makes must still be true.
#
# The docs assert issue state in prose all over -- PROGRESS.md alone carries ~100 issue references
# -- and that state moves without the prose moving with it. PROGRESS.md records the failure twice in
# its own text ("this file had claimed that prematurely while the issue was still open", #179 and
# #180), which is what makes this an invariant worth a command rather than a habit.
#
# ---------------------------------------------------------------------------------------------
# Scope, and why it is this narrow.
#
# This checks ONE form: a Markdown issue link immediately followed by a parenthesised state word.
#
#     [#128](https://github.com/Akvize/reconcile-rs/issues/128) (closed)
#     [#184](…/issues/184) (closed, decision recorded in `ARCHITECTURE.md` §7)
#
# It deliberately does NOT read free prose, because that was measured and it does not work. A
# matcher that accepted "a state word near an issue number" produced three hits on this tree and all
# three were wrong:
#
#   "…([#257](…), closed); `t` unsettled — [#315](…)"   -> "closed" belongs to #257, not #315
#   "#257 landed 2026-08-11 via #307; #315 and #318…"    -> "landed" belongs to #257
#   "…#185 weighs a fixed-capacity IBLT…"                -> matched the "fixed" in "fixed-capacity"
#
# A weekly job that cries wolf three times out of three is a job someone turns off. The annotated
# form is unambiguous by construction, it is already the convention in these files (six uses at the
# time of writing, all correct), and an unannotated mention simply is not a claim about state.
# ---------------------------------------------------------------------------------------------
#
# Runs weekly rather than per-push: like its sibling jobs, the subject is the tracker's existing
# state, not any one diff. It reports and fails the scheduled run -- producing work -- rather than
# blocking a commit on a fact that changed outside the committer's control.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

REPO="${GITHUB_REPOSITORY:-Akvize/reconcile-rs}"

command -v gh >/dev/null || { echo "check-doc-issue-claims: gh is required" >&2; exit 1; }

status=0
checked=0

# `[#N](any-url) (state…)` -- the state word must open the parenthesis, so "(see #99)" and
# "(measured in …)" are not claims.
CLAIM_RE='\[#[0-9]+\]\([^)]*\)[[:space:]]*\((closed|open|resolved|fixed|merged)\b'

# `grep -o` so each hit is a self-contained `[#N](url) (state` pair. Extracting the number and the
# state word independently from a whole *line* would reattach them wrongly whenever a line carries
# several references -- PROGRESS.md has such a line, where `[#110]` and `[#109]` precede the only
# actual claim, which is `[#184](…) (closed, …)`. That is the same misattribution this script exists
# to avoid, so it must not commit it itself.
while IFS= read -r hit; do
    file=${hit%%:*}
    rest=${hit#*:}
    line=${rest%%:*}
    text=${rest#*:}

    num=$(grep -oE '^\[#[0-9]+\]' <<<"$text" | tr -cd '0-9')
    claim=$(grep -oiE '\((closed|open|resolved|fixed|merged)$' <<<"$text" | tr -d '(' | tr '[:upper:]' '[:lower:]')
    [ -n "$num" ] && [ -n "$claim" ] || continue

    actual=$(gh issue view "$num" --repo "$REPO" --json state --jq '.state' 2>/dev/null || echo "UNKNOWN")
    checked=$((checked + 1))

    case "$actual" in
        UNKNOWN)
            echo "check-doc-issue-claims: $file:$line: #$num could not be read from $REPO" >&2
            status=1
            ;;
        OPEN)
            if [ "$claim" != "open" ]; then
                echo "check-doc-issue-claims: $file:$line: claims #$num is '$claim', but it is OPEN" >&2
                status=1
            fi
            ;;
        CLOSED)
            if [ "$claim" = "open" ]; then
                echo "check-doc-issue-claims: $file:$line: claims #$num is 'open', but it is CLOSED" >&2
                status=1
            fi
            ;;
    esac
done < <(grep -ronE "$CLAIM_RE" -- *.md 2>/dev/null || true)

echo "check-doc-issue-claims: $checked annotated issue-state claims checked"

if [ "$status" -ne 0 ]; then
    echo >&2
    echo "A document asserts an issue state the tracker disagrees with. Update the prose, or reopen/" >&2
    echo "close the issue if the document was right (AGENTS.md §9: the docs and the tracker are one" >&2
    echo "fact, and it lives in the tracker)." >&2
fi

exit "$status"

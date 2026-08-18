#!/usr/bin/env bash
# rustdoc must not cite a closed issue.
#
# rustdoc is published, third-party-facing documentation (docs.rs renders exactly these `///`/`//!`
# comments for `reconcile`, the one crate on crates.io today). A reader there has no account on
# this repository's issue tracker and no reason to open one just to understand a doc comment -- so
# once an issue closes, citing it stops being information and starts being a vestige of which
# ticket produced the prose. Every citation actually in this tree already states its fact in full
# before the parenthetical (`clock.rs`, `replicated_map.rs`, `system.rs`/`netem.rs`, before #388
# stripped their now-closed #280/#288 citations down to that prose) -- so removing a closed
# reference costs nothing but the vestige.
#
# This is deliberately the opposite policy from SOTA.md/ARCHITECTURE.md, where a closed-issue
# citation is the point: those are durable-reference docs (excluded from the published package,
# Cargo.toml's `exclude`), and `check-doc-issue-claims.sh` already polices
# them on a narrower question -- not "is this issue closed" but "does this document's own state
# *claim* about it still match reality". That script's annotated-claim form and this script's bare
# reference form do not overlap: a `[#N](url) (closed)` claim in a `.md` file is correct by
# definition once #N actually is closed, which is exactly this script's forbidden case turned
# inside out for rustdoc.
#
# Runs on the same cadence as its `.md` sibling and for the same reason (issue-integrity.yml's
# header): the subject is the tracker's *current* state, which drifts independently of any commit
# to the `.rs` file doing the citing.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."
# shellcheck source=scripts/lib-rustdoc-issue-refs.sh
source "$SCRIPT_DIR/lib-rustdoc-issue-refs.sh"

REPO="${GITHUB_REPOSITORY:-Akvize/reconcile-rs}"

command -v gh >/dev/null || { echo "check-closed-issue-rustdoc-refs: gh is required" >&2; exit 1; }

status=0
checked=0
declare -A state_of=()

while IFS=: read -r file line num; do
    [ -n "$num" ] || continue
    if [ -z "${state_of[$num]+x}" ]; then
        state_of[$num]=$(gh issue view "$num" --repo "$REPO" --json state --jq '.state' 2>/dev/null || echo "UNKNOWN")
        checked=$((checked + 1))
    fi
    case "${state_of[$num]}" in
        CLOSED)
            echo "check-closed-issue-rustdoc-refs: $file:$line: cites #$num, which is CLOSED -- inline the fact in prose (it likely already is, right before the citation) and drop the reference; rustdoc is published documentation, not a tracker mirror" >&2
            status=1
            ;;
        UNKNOWN)
            echo "check-closed-issue-rustdoc-refs: $file:$line: #$num could not be read from $REPO" >&2
            status=1
            ;;
    esac
done < <(rustdoc_issue_refs)

echo "check-closed-issue-rustdoc-refs: $checked distinct issue(s) referenced from rustdoc, checked against $REPO"

if [ "$status" -ne 0 ]; then
    echo >&2
    echo "A closed issue is still cited from rustdoc. Remove the citation (AGENTS.md §9: rustdoc is" >&2
    echo "the fact's home once the issue settles, not a pointer to it), or reopen the issue if the" >&2
    echo "work it names is not actually done." >&2
fi

exit "$status"

#!/usr/bin/env bash
# The converse of check-closed-issue-rustdoc-refs.sh, checked before merge instead of after: a PR
# that closes issue #N (a recognized closing keyword in its own description -- exactly
# check-pr-closes-issues.sh's rule, shared via lib-closing-refs.sh) must not leave a rustdoc
# citation of #N anywhere in the tree at its own HEAD. Merging such a PR flips #N to CLOSED and
# instantly creates the violation the sibling script polices, on a commit nobody pushed -- main
# would break the moment the PR lands, and issue-integrity.yml would not notice until its next
# scheduled run. Catching it here, before the merge, costs one PR comment instead of a Monday
# audit with no author still holding the context.
#
# No `gh` call and no network: whether this PR closes #N is read from its own description
# ($PR_BODY, an env var -- never interpolated into this script's text, a PR body is
# attacker-controlled), and whether #N is cited from rustdoc is a property of the checked-out tree
# at this PR's HEAD, not of the tracker. That also means it costs nothing to run on every PR event,
# unlike its `gh`-backed siblings.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."
# shellcheck source=scripts/lib-closing-refs.sh
source "$SCRIPT_DIR/lib-closing-refs.sh"
# shellcheck source=scripts/lib-rustdoc-issue-refs.sh
source "$SCRIPT_DIR/lib-rustdoc-issue-refs.sh"

BODY="${PR_BODY:-}"
mapfile -t closing_refs < <(grep -oPi "$CLOSING_RE" <<<"$BODY" | grep -oE '[0-9]+' | sort -u)

status=0
if [ "${#closing_refs[@]}" -gt 0 ]; then
    while IFS=: read -r file line num; do
        [ -n "$num" ] || continue
        if is_in "$num" "${closing_refs[@]}"; then
            echo "check-pr-closing-issue-rustdoc-refs: $file:$line: cites #$num, which this PR's own description closes -- merging would leave a closed-issue citation in rustdoc from the moment it lands. Inline the fact and drop the reference, or drop the closing keyword if #$num is not actually resolved by this change." >&2
            status=1
        fi
    done < <(rustdoc_issue_refs)
fi

exit "$status"

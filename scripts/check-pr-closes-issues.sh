#!/usr/bin/env bash
# Every open issue mentioned in a PR's own description must state its intent: either a
# recognized GitHub closing keyword (Closes/Fixes/Resolves #N -- closes it at merge time) or an
# explicit non-closing lead-in (relates to/see/tracks/blocked by/part of/ref #N). A bare "#N" with
# neither is exactly the gap that let #203/#230/#283/#309/#312 get closed by hand before the PR
# that was supposed to close them had even merged -- nothing forced the author (or a reviewer) to
# say which one was meant.
#
# Reads the PR body from $PR_BODY (an env var, not interpolated into this script's text) and
# looks up issue state via `gh` -- deliberately not `gh pr view`: this script only needs the body
# the caller already has from the `pull_request` event payload.
set -Eeuo pipefail

REPO="${GITHUB_REPOSITORY:-Akvize/reconcile-rs}"
BODY="${PR_BODY:-}"

CLOSING_RE='(?i)\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b:?[[:space:]]*#([0-9]+)'
NONCLOSING_RE='(?i)\b(relates?\s+to|see|tracks?|blocked\s+by|part\s+of|ref(erences?)?)\b:?[[:space:]]*#([0-9]+)'

mapfile -t all_refs < <(grep -oE '#[0-9]+' <<<"$BODY" | tr -d '#' | sort -u)
mapfile -t closing_refs < <(grep -oPi "$CLOSING_RE" <<<"$BODY" | grep -oE '[0-9]+' | sort -u)
mapfile -t nonclosing_refs < <(grep -oPi "$NONCLOSING_RE" <<<"$BODY" | grep -oE '[0-9]+' | sort -u)

is_in() {
    local needle="$1"
    shift
    for x in "$@"; do
        [ "$x" = "$needle" ] && return 0
    done
    return 1
}

status=0
for n in "${all_refs[@]:-}"; do
    [ -z "$n" ] && continue
    is_in "$n" "${closing_refs[@]:-}" && continue
    is_in "$n" "${nonclosing_refs[@]:-}" && continue
    # Numbers are a shared namespace between issues and PRs; a self-reference or a reference to
    # another PR fails this lookup and is silently skipped -- this check is about issues only.
    state=$(gh issue view "$n" --repo "$REPO" --json state -q .state 2>/dev/null || echo "")
    if [ "$state" = "OPEN" ]; then
        echo "check-pr-closes-issues: #$n is open and mentioned without stating intent -- add 'Closes #$n' if this PR resolves it, or 'relates to #$n' if it doesn't" >&2
        status=1
    fi
done

exit "$status"

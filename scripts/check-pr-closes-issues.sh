#!/usr/bin/env bash
# Every open issue mentioned in a PR's own description must state its intent: either a
# recognized GitHub closing keyword (Closes/Fixes/Resolves #N -- closes it at merge time) or an
# explicit non-closing lead-in (relates to/see/tracks/blocked by/part of/ref #N). A bare "#N" with
# neither is exactly the gap that let #203/#230/#283/#309/#312 get closed by hand before the PR
# that was supposed to close them had even merged -- nothing forced the author (or a reviewer) to
# say which one was meant.
#
# Second proposition, same subject seen from the other end: an issue this PR *closes* must have
# nothing left unticked in it. `check-issue-triage.sh`'s rule 7 already reports that, but only
# after the fact -- on a weekly tick, or on whichever unrelated pull request happens to run it
# next. By then the author has moved on, and whoever inherits the failure has to reconstruct from
# the outside what the closer knew at the time. Measured on 2026-08-15: four issues closed within
# three minutes surfaced hours later on a pull request that had nothing to do with any of them,
# and settling them took reading four issues, three merged pull requests and `main` itself.
#
# Here the same question costs seconds, because it is asked of the person who is about to close
# the issue, while they still have the context, and before the close rather than after: tick what
# this pull request delivers, and split or re-home what it does not. Both of those are the right
# answer at merge time and archaeology afterwards.
#
# Rule 7 stays as the backstop for the closes this cannot see -- by hand in the UI, or by a commit
# message rather than a description.
#
# Reads the PR body from $PR_BODY (an env var, not interpolated into this script's text) and
# looks up issues via `gh` -- deliberately not `gh pr view`: this script only needs the body the
# caller already has from the `pull_request` event payload.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/lib-closing-refs.sh
source "$SCRIPT_DIR/lib-closing-refs.sh"

REPO="${GITHUB_REPOSITORY:-Akvize/reconcile-rs}"
BODY="${PR_BODY:-}"

mapfile -t all_refs < <(grep -oE '#[0-9]+' <<<"$BODY" | tr -d '#' | sort -u)
mapfile -t closing_refs < <(grep -oPi "$CLOSING_RE" <<<"$BODY" | grep -oE '[0-9]+' | sort -u)
mapfile -t nonclosing_refs < <(grep -oPi "$NONCLOSING_RE" <<<"$BODY" | grep -oE '[0-9]+' | sort -u)

status=0
for n in "${all_refs[@]:-}"; do
    [ -z "$n" ] && continue
    is_in "$n" "${closing_refs[@]:-}" && continue
    is_in "$n" "${nonclosing_refs[@]:-}" && continue
    # Numbers are a shared namespace between issues and PRs, and this lookup does **not** separate
    # them: `gh issue view` resolves a pull request number too, and reports `OPEN` for an open one.
    # So an open PR mentioned here needs an intent word exactly like an issue does. Measured on
    # PR #379, whose body named an open sibling PR and failed on it -- this comment previously
    # claimed such a reference "fails this lookup and is silently skipped", which was never true.
    #
    # What is genuinely skipped: a merged or closed PR (state is not OPEN, same as a closed issue)
    # and a number that resolves to nothing at all.
    state=$(gh issue view "$n" --repo "$REPO" --json state -q .state 2>/dev/null || echo "")
    if [ "$state" = "OPEN" ]; then
        echo "check-pr-closes-issues: #$n is open and mentioned without stating intent -- add 'Closes #$n' if this PR resolves it, or 'relates to #$n' if it doesn't" >&2
        status=1
    fi
done

# Nothing left unticked in what this PR is about to close.
#
# The count is of `- [ ]` rows anywhere in the body, matching rule 7 exactly, so the two agree by
# construction rather than by two regexes kept in step by hand. Deliberately not scoped to an
# `## Acceptance` heading: #72's four genuinely-undone rows lived under `## Scope` and it had no
# acceptance section at all, so a heading-scoped matcher would have missed the one case here that
# actually lost tracked work.
#
# Only for issues still open. An already-closed number named by `Closes #N` is a re-land or a
# stale description, and its boxes are rule 7's business by then -- checking them here would fail
# a pull request on history it did not create, including the backlog rule 7 deliberately
# grandfathers by date.
for n in "${closing_refs[@]:-}"; do
    [ -z "$n" ] && continue
    # Two calls rather than one `--json state,body`: the body is multi-line, so packing both into
    # one shell variable means escaping newlines out and back, and a gate script is the last place
    # to be clever. Closing refs are one or two per pull request.
    [ "$(gh issue view "$n" --repo "$REPO" --json state -q .state 2>/dev/null || echo "")" = "OPEN" ] || continue
    body=$(gh issue view "$n" --repo "$REPO" --json body -q .body 2>/dev/null || echo "")
    open_boxes=$(grep -cE '^[[:space:]]*[-*][[:space:]]+\[ \]' <<<"$body" || true)
    if [ "${open_boxes:-0}" -gt 0 ]; then
        echo "check-pr-closes-issues: this PR closes #$n, which still has $open_boxes unticked box(es) -- tick what this PR delivers, and split or re-home what it does not, before merging" >&2
        status=1
    fi
done

exit "$status"

#!/usr/bin/env bash
# CLOSING_RE/NONCLOSING_RE/is_in: shared between check-pr-closes-issues.sh and
# check-pr-closing-issue-rustdoc-refs.sh so "this PR closes #N" has exactly one definition instead
# of two regexes kept in step by hand -- check-pr-closes-issues.sh's own header names that as
# precisely the mistake its rustdoc-vs-PROGRESS.md agreement-by-construction was written to avoid.
CLOSING_RE='(?i)\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b:?[[:space:]]*#([0-9]+)'
NONCLOSING_RE='(?i)\b(relates?\s+to|see|tracks?|blocked\s+by|part\s+of|ref(erences?)?)\b:?[[:space:]]*#([0-9]+)'

is_in() {
    local needle="$1"
    shift
    for x in "$@"; do
        [ "$x" = "$needle" ] && return 0
    done
    return 1
}

#!/usr/bin/env bash
# Enforces the label invariants that `.github/labels.tsv` defines, over every open issue.
#
# A taxonomy nobody applies is worse than none: it reads as a classification while the
# backlog stays a flat list, so a query returns a partial answer that looks complete. The
# four namespaces are only worth their weight if "exactly one C-, at least one A-, exactly
# one S-" holds everywhere, so that is checked rather than remembered (AGENTS.md §10).
#
#   1. exactly one C-*   what artifact closes this issue
#   2. at least one A-*  where it lands
#   3. exactly one S-*   whether it can be acted on now
#   4. S-blocked names its blocker as #NNN in the body
#   6. a named gate that has already been satisfied is reported (see below)
#
# "Does 1.0.0 wait on this" is a milestone, not a label (`.github/labels.tsv`), so there is
# nothing here to check for it: GitHub maintains milestone membership natively, which is the
# whole reason it is a milestone.
#
# `S-needs-triage` is the untriaged state, so it is exempt from 1 and 2 — that is the whole
# point of having it, and it is how rust-lang/rust's own `needs-triage` works. It is not a
# hiding place: set TRIAGE_SLA_DAYS to fail on anything that has sat there too long.
#
# Rule 5 is checked and "S-parked states its wake-up trigger" is not, because the first is
# a pattern and the second is a judgement. Parking without a trigger is how a backlog
# accumulates issues nobody will ever act on or close; that one stays a review question,
# deliberately.
#
# Rule 6 does NOT revisit that. It is a different proposition, and the difference is the whole
# reason it can be a command:
#
#   declined, still declined  — "every S-parked issue states a trigger"   (∃, a judgement)
#   rule 6                    — "a trigger that IS named is not already met" (∀, a pattern)
#
# Nothing here requires a trigger, judges whether a sentence is one, or reads free prose. It
# validates only what an issue opted into stating, which is how §4.2's bibliography format
# migrates in `SOTA.md` too: grandfather everything, bind what opts in.
#
# The defect it catches is real and was live on 2026-08-14: #185 sat `S-parked` reading "gated
# on #280" for hours after #280 closed. Nothing pointed at it; the label said "not now" while
# the reason had evaporated. That is the failure mode `S-parked` has by construction — it is
# the one state whose correctness depends on something *outside* the issue.
#
# Two forms, because the two labels carry different guarantees:
#
#   S-blocked   rule 4 already guarantees a #NNN is named, and the label means exactly "blocked
#               by another issue". So: if EVERY issue the body references is closed, it cannot
#               still be blocked. No judgement about which reference is "the" blocker — if any
#               candidate is still open, this says nothing.
#   S-parked    the trigger may not be an issue at all ("when a user asks for it"), so an
#               all-closed rule would be wrong. Only an explicit annotation is read:
#               `gated on #N`, `blocked by #N`, `parked on #N`, `waits on #N`, `unparked when
#               #N`. Free prose is deliberately out of scope — "Related: #280 (the message
#               column stays unpriced either way)" in #315 means the opposite of a dependency,
#               and a matcher that read it as one would be wrong on its first try.
#
# Usage:
#   ./scripts/check-issue-triage.sh                  # report + fail on violations
#   TRIAGE_SLA_DAYS=14 ./scripts/check-issue-triage.sh
#
# Requires: gh (authenticated, or GH_TOKEN in the environment) and jq.
# CI-only by design (AGENTS.md §3): it needs the network and the issue tracker, so it fits
# neither hook tier — a pre-commit that calls GitHub is a pre-commit people bypass.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

REPO=${REPO:-Akvize/reconcile-rs}
LABELS_FILE=.github/labels.tsv

if ! command -v jq >/dev/null 2>&1; then
    echo "check-issue-triage: jq is required" >&2
    exit 1
fi
if [ -z "${ISSUES_JSON:-}" ] && ! command -v gh >/dev/null 2>&1; then
    echo "check-issue-triage: the GitHub CLI (gh) is required, or set ISSUES_JSON" >&2
    exit 1
fi

# Every label the taxonomy declares — an issue carrying anything else is carrying a label
# that survived a `--prune`-less sync, which is exactly the drift this file exists to catch.
known=$(grep -v '^#' "$LABELS_FILE" | grep -v '^$' | cut -f1)

# ISSUES_JSON points at a captured payload, so the rules below can be exercised against a
# fixture without a network round trip or a token.
if [ -n "${ISSUES_JSON:-}" ]; then
    issues=$(cat "$ISSUES_JSON")
else
    issues=$(gh issue list --repo "$REPO" --state open --limit 500 \
        --json number,title,labels,body,createdAt)
fi

violations=0
report() {
    violations=$((violations + 1))
    printf '  #%-5s %s\n' "$1" "$2"
}

while IFS=$'\t' read -r number kinds areas statuses unknown blocked_ok title; do
    [ -n "$number" ] || continue

    if [ "$statuses" -ne 1 ]; then
        report "$number" "$statuses S- labels, expected exactly 1 — $title"
        continue
    fi

    # Untriaged is a legitimate state; it is simply not one you may also claim to have
    # classified. Rules 1 and 2 resume the moment it leaves.
    if [ "$kinds" -eq 0 ] && [ "$areas" -eq 0 ]; then
        untriaged_state=true
    else
        untriaged_state=false
    fi

    if ! $untriaged_state; then
        [ "$kinds" -eq 1 ] || report "$number" "$kinds C- labels, expected exactly 1 — $title"
        [ "$areas" -ge 1 ] || report "$number" "no A- label — $title"
    fi

    [ "$unknown" -eq 0 ] || report "$number" "$unknown label(s) absent from $LABELS_FILE — $title"
    [ "$blocked_ok" = "ok" ] || report "$number" "S-blocked with no #NNN blocker in the body — $title"
done < <(
    jq -r --arg known "$known" '
        ($known | split("\n")) as $known |
        .[] |
        [ .number,
          ([.labels[].name | select(startswith("C-"))] | length),
          ([.labels[].name | select(startswith("A-"))] | length),
          ([.labels[].name | select(startswith("S-"))] | length),
          ([.labels[].name | select(. as $n | $known | index($n) | not)] | length),
          (if ([.labels[].name] | index("S-blocked"))
           then (if ((.body // "") | test("#[0-9]+")) then "ok" else "missing" end)
           else "ok" end),
          .title
        ] | @tsv
    ' <<<"$issues"
)

# --- Rule 6: a named gate that has already been satisfied ---------------------------------
# Needs one lookup per distinct referenced issue, so references are collected first and each
# number resolved at most once. Skipped without `gh` — in fixture mode there is nothing to
# resolve against, and guessing would be worse than not checking.
declare -A REF_STATE
ref_is_closed() {
    local n=$1
    if [ -z "${REF_STATE[$n]:-}" ]; then
        REF_STATE[$n]=$(gh issue view "$n" --repo "$REPO" --json state --jq .state 2>/dev/null || echo UNKNOWN)
    fi
    [ "${REF_STATE[$n]}" = "CLOSED" ]
}

if command -v gh >/dev/null 2>&1; then
    while IFS=$'\t' read -r number which refs title; do
        [ -n "$number" ] || continue
        [ "$refs" = "-" ] && continue

        open_left=0
        closed_refs=""
        for r in $refs; do
            if ref_is_closed "$r"; then
                closed_refs="$closed_refs #$r"
            else
                open_left=$((open_left + 1))
            fi
        done
        [ -n "$closed_refs" ] || continue

        if [ "$which" = "S-blocked" ]; then
            # Only when nothing referenced is still open: with an open candidate present, which
            # one is "the" blocker is a judgement, and this stays out of it.
            [ "$open_left" -eq 0 ] || continue
            report "$number" "S-blocked, but every issue it references is closed —$closed_refs — $title"
        else
            report "$number" "S-parked on$closed_refs, which is closed — the stated gate is met — $title"
        fi
    done < <(
        # `-` rather than an empty refs field, because IFS=$'\t' makes tab an *IFS whitespace*
        # character: bash collapses runs of it, so an empty middle field silently shifts every
        # column after it and the title lands in `refs`. The rules above never emit an empty
        # field, which is why the same `read` has been safe there.
        jq -r '
            def refs_all: [ (.body // "") | scan("#([0-9]+)") | .[0] ] | unique;
            def refs_annotated: [ (.body // "")
                | scan("(?i)(?:gated on|blocked by|parked on|waits on|waiting on|unparked when)[ \t:]*#([0-9]+)")
                | .[0] ] | unique;
            def field: join(" ") | if . == "" then "-" else . end;
            .[]
            | . as $i
            | ([$i.labels[].name]) as $l
            | if ($l | index("S-blocked")) then
                  [ $i.number, "S-blocked", (($i | refs_all) | field), $i.title ]
              elif ($l | index("S-parked")) then
                  [ $i.number, "S-parked", (($i | refs_annotated) | field), $i.title ]
              else empty end
            | @tsv
        ' <<<"$issues"
    )
fi

total=$(jq 'length' <<<"$issues")
untriaged=$(jq '[.[] | select([.labels[].name] | index("S-needs-triage"))] | length' <<<"$issues")

echo
printf '  %d open issues, %d awaiting triage, %d violation(s)\n' "$total" "$untriaged" "$violations"

if [ -n "${TRIAGE_SLA_DAYS:-}" ] && [ "$untriaged" -gt 0 ]; then
    cutoff=$(date -u -d "${TRIAGE_SLA_DAYS} days ago" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
        date -u -v-"${TRIAGE_SLA_DAYS}"d +%Y-%m-%dT%H:%M:%SZ)
    stale=$(jq -r --arg cutoff "$cutoff" '
        [.[] | select(([.labels[].name] | index("S-needs-triage")) and .createdAt < $cutoff)]
        | map("#\(.number) \(.title)") | .[]' <<<"$issues")
    if [ -n "$stale" ]; then
        echo >&2
        echo "  awaiting triage for more than ${TRIAGE_SLA_DAYS} days:" >&2
        echo "$stale" | sed 's/^/    /' >&2
        violations=$((violations + $(wc -l <<<"$stale")))
    fi
fi

if [ "$violations" -gt 0 ]; then
    echo >&2
    echo "check-issue-triage: $violations issue(s) do not satisfy .github/labels.tsv's invariants." >&2
    echo "Fix the labels, or change the taxonomy in that file and say why in the commit." >&2
    exit 1
fi

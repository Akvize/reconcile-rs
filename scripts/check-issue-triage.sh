#!/usr/bin/env bash
# Enforces the label invariants that `.github/labels.tsv` defines, over every open issue.
#
# A taxonomy nobody applies is worse than none: it reads as a classification while the
# backlog stays a flat list, so a query returns a partial answer that looks complete. The
# four namespaces are only worth their weight if "exactly one kind/, at least one area/,
# exactly one status/" holds everywhere, so that is checked rather than remembered
# (AGENTS.md §10).
#
#   1. exactly one kind/*      what closes this issue
#   2. at least one area/*     where it lands
#   3. exactly one status/*    what it is waiting on
#   4. at most one gate/*      what shipping without it costs
#   5. status/blocked names its blocker as #NNN in the body
#
# `status/needs-triage` is the untriaged state, so it is exempt from 1 and 2 — that is the
# whole point of having it, and it is how kubernetes' `needs-triage` works. It is not a
# hiding place: set TRIAGE_SLA_DAYS to fail on anything that has sat there too long.
#
# Rule 5 is checked and "status/parked states its wake-up trigger" is not, because the
# first is a pattern and the second is a judgement. Parking without a trigger is how a
# backlog accumulates issues nobody will ever act on or close; that one stays a review
# question, deliberately.
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

while IFS=$'\t' read -r number kinds areas statuses gates unknown blocked_ok title; do
    [ -n "$number" ] || continue

    if [ "$statuses" -ne 1 ]; then
        report "$number" "$statuses status/ labels, expected exactly 1 — $title"
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
        [ "$kinds" -eq 1 ] || report "$number" "$kinds kind/ labels, expected exactly 1 — $title"
        [ "$areas" -ge 1 ] || report "$number" "no area/ label — $title"
    fi

    [ "$gates" -le 1 ] || report "$number" "$gates gate/ labels, expected at most 1 — $title"
    [ "$unknown" -eq 0 ] || report "$number" "$unknown label(s) absent from $LABELS_FILE — $title"
    [ "$blocked_ok" = "ok" ] || report "$number" "status/blocked with no #NNN blocker in the body — $title"
done < <(
    jq -r --arg known "$known" '
        ($known | split("\n")) as $known |
        .[] |
        [ .number,
          ([.labels[].name | select(startswith("kind/"))]   | length),
          ([.labels[].name | select(startswith("area/"))]   | length),
          ([.labels[].name | select(startswith("status/"))] | length),
          ([.labels[].name | select(startswith("gate/"))]   | length),
          ([.labels[].name | select(. as $n | $known | index($n) | not)] | length),
          (if ([.labels[].name] | index("status/blocked"))
           then (if ((.body // "") | test("#[0-9]+")) then "ok" else "missing" end)
           else "ok" end),
          .title
        ] | @tsv
    ' <<<"$issues"
)

total=$(jq 'length' <<<"$issues")
untriaged=$(jq '[.[] | select([.labels[].name] | index("status/needs-triage"))] | length' <<<"$issues")

echo
printf '  %d open issues, %d awaiting triage, %d violation(s)\n' "$total" "$untriaged" "$violations"

if [ -n "${TRIAGE_SLA_DAYS:-}" ] && [ "$untriaged" -gt 0 ]; then
    cutoff=$(date -u -d "${TRIAGE_SLA_DAYS} days ago" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
        date -u -v-"${TRIAGE_SLA_DAYS}"d +%Y-%m-%dT%H:%M:%SZ)
    stale=$(jq -r --arg cutoff "$cutoff" '
        [.[] | select(([.labels[].name] | index("status/needs-triage")) and .createdAt < $cutoff)]
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

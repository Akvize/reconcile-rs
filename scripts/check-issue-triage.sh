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
#   4. S-blocked names its blocker as "blocked by #NNN", not as a bare #NNN
#   6. a named gate that has already been satisfied is reported (see below)
#   7. an issue closed on or after TRIAGE_BOXES_SINCE carries no unticked acceptance box
#   8. an open parent whose every sub-issue is closed is reported
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
#   S-blocked   two tiers, because "which reference is *the* blocker" is a judgement only when the
#               issue declined to say. If the body carries an explicit annotation, that IS the
#               blocker set and nothing else counts; otherwise every referenced issue is a
#               candidate and all of them must be closed. Either way the rule fires only when no
#               blocker is left open, so it never guesses.
#
#               The fallback alone was measured too weak on 2026-08-15: #356 read "blocked by #355"
#               *and* carried a "Related: #352" footer. #355 closed, #352 did not, and the
#               all-references tier stayed silent on a discharged blocker — the exact failure this
#               rule exists to catch, defeated by an incidental mention.
#   S-parked    the trigger may not be an issue at all ("when a user asks for it"), so an
#               all-closed rule would be wrong. Only an explicit annotation is read — the
#               vocabulary is `BLOCKER_KEYWORDS` below. Free prose is deliberately out of scope
#               — "Related: #280 (the message column stays unpriced either way)" in #315 means
#               the opposite of a dependency, and a matcher that read it as one would be wrong
#               on its first try.
#
# Usage:
#   ./scripts/check-issue-triage.sh                  # report + fail on violations
#   TRIAGE_SLA_DAYS=14 ./scripts/check-issue-triage.sh
#   TRIAGE_BOXES_SINCE=2026-08-15 ./scripts/check-issue-triage.sh    # rule 7's window
#   TRIAGE_GRACE_MINUTES=0 ./scripts/check-issue-triage.sh           # bind rules 1-4 immediately
#
# Requires: gh (authenticated, or GH_TOKEN in the environment) and jq.
# CI-only by design (AGENTS.md §3): it needs the network and the issue tracker, so it fits
# neither hook tier — a pre-commit that calls GitHub is a pre-commit people bypass.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

REPO=${REPO:-Akvize/reconcile-rs}
LABELS_FILE=.github/labels.tsv

# The annotation vocabulary rules 4 and 6 read, written once and passed into every jq program that
# needs it. Two copies of an alternation are two things to keep in step, and the one that drifts is
# the one nobody re-reads (AGENTS.md §9). `blocked on` is in the list because it is what people
# actually write — #328 said "Blocked on #292's `sync_state()`" and went unread for that alone.
BLOCKER_KEYWORDS='gated on|blocked by|blocked on|parked on|waits on|waiting on|unparked when'

# Rule 7's window. Everything closed before this predates the rule, and there is no honest way to
# drain that tail: ticking an acceptance box on a closed issue to satisfy a linter records work as
# done that nobody did. So the rule is *bounded* rather than grandfathered issue by issue — it fails
# from the cutoff onward and reports the history as a count, which is the fact; the list is not.
BOXES_SINCE=${TRIAGE_BOXES_SINCE:-2026-08-15}

# How long an issue may exist before the label rules bind. Not politeness -- a correctness fix.
# Rule 3 fires before the `S-needs-triage` exemption below can, so an issue with no labels *at all*
# is a violation from the instant it is created, and stays one until whoever filed it finishes
# typing. Measured on 2026-08-15: #383 was created at 13:07:52 and labelled at 13:07:56, and this
# check ran at 13:07:53 -- it failed a pull request on a four-second window that had already closed
# by the time anyone read the log. That is noise, and noise is what makes a gate ignorable.
#
# Freshly-filed issues are still counted and named, just not failed on: "not yet triaged" is a real
# state, and staying silent about it would trade one wrong answer for another. `TRIAGE_SLA_DAYS`
# remains the deadline that eventually does bind.
GRACE_MINUTES=${TRIAGE_GRACE_MINUTES:-60}
grace_cutoff=$(date -u -d "${GRACE_MINUTES} minutes ago" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
    date -u -v-"${GRACE_MINUTES}"M +%Y-%m-%dT%H:%M:%SZ)

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

# A finding that does not fail the run. Two uses, and both are deliberate:
#
#   grandfathering  a convention tightened after the backlog was written cannot fail on the
#                   issues that predate it, or the check is red until someone edits history.
#                   It reports, the backlog drains, and the rule is promoted afterwards.
#   missing data    "cannot tell" is not "conforms". Saying so out loud is what stops a silent
#                   skip from reading as a pass.
notes=0
note() {
    notes=$((notes + 1))
    printf '  #%-5s · %s\n' "$1" "$2"
}

while IFS=$'\t' read -r number kinds areas statuses unknown blocked_ok fresh title; do
    [ -n "$number" ] || continue

    # Within the grace window every finding below downgrades to a note. One `if` rather than a
    # branch at each call site: whichever rule an issue trips in its first hour, the answer is the
    # same -- say so, do not fail on it.
    if [ "$fresh" = "fresh" ]; then
        say=note
    else
        say=report
    fi

    if [ "$statuses" -ne 1 ]; then
        $say "$number" "$statuses S- labels, expected exactly 1 — $title"
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
        [ "$kinds" -eq 1 ] || $say "$number" "$kinds C- labels, expected exactly 1 — $title"
        [ "$areas" -ge 1 ] || $say "$number" "no A- label — $title"
    fi

    [ "$unknown" -eq 0 ] || $say "$number" "$unknown label(s) absent from $LABELS_FILE — $title"

    # Three states, not two. "bare" is a #NNN somewhere in the body with no keyword in front of
    # it — enough for rule 4 as originally written, and useless to rule 6, which cannot tell a
    # blocker from a "Part of", a "Related:" or a number in a sentence. That is how the #271
    # cluster sat blocked on a discharged design call with nothing to notice it.
    #
    # Both tiers fail. `bare` noted rather than failed while the backlog it postdated was annotated;
    # that drained on 2026-08-15 (the eleven then-open S-blocked issues), so the distinction is now
    # only in the message — a bare reference and no reference are different mistakes to fix.
    case "$blocked_ok" in
        ok) ;;
        bare) $say "$number" "S-blocked names #NNN but not as 'blocked by #NNN' — rule 6 cannot read it — $title" ;;
        *)  $say "$number" "S-blocked with no #NNN blocker in the body — $title" ;;
    esac
done < <(
    jq -r --arg known "$known" --arg kw "$BLOCKER_KEYWORDS" --arg grace "$grace_cutoff" '
        ($known | split("\n")) as $known |
        .[] |
        [ .number,
          ([.labels[].name | select(startswith("C-"))] | length),
          ([.labels[].name | select(startswith("A-"))] | length),
          ([.labels[].name | select(startswith("S-"))] | length),
          ([.labels[].name | select(. as $n | $known | index($n) | not)] | length),
          (if ([.labels[].name] | index("S-blocked"))
           then (if ((.body // "") | test("(?i)(?:" + $kw + ")[ \t:*_`]*#[0-9]+")) then "ok"
                 elif ((.body // "") | test("#[0-9]+")) then "bare"
                 else "missing" end)
           else "ok" end),
          (if (.createdAt // "") >= $grace then "fresh" else "-" end),
          .title
        ] | @tsv
    ' <<<"$issues"
)

# --- Rule 6: a named gate that has already been satisfied ---------------------------------
# Needs one lookup per distinct referenced issue, so references are collected first and each
# number resolved at most once. Skipped without `gh` — in fixture mode there is nothing to
# resolve against, and guessing would be worse than not checking.
declare -A REF_STATE
ref_state() {
    local n=$1
    if [ -z "${REF_STATE[$n]:-}" ]; then
        REF_STATE[$n]=$(gh issue view "$n" --repo "$REPO" --json state --jq .state 2>/dev/null || echo UNKNOWN)
    fi
    printf '%s' "${REF_STATE[$n]}"
}

if command -v gh >/dev/null 2>&1; then
    while IFS=$'\t' read -r number which refs title; do
        [ -n "$number" ] || continue
        [ "$refs" = "-" ] && continue

        open_left=0
        closed_refs=""
        unresolved=""
        for r in $refs; do
            case "$(ref_state "$r")" in
                CLOSED)  closed_refs="$closed_refs #$r" ;;
                UNKNOWN) unresolved="$unresolved #$r" ;;
                *)       open_left=$((open_left + 1)) ;;
            esac
        done

        # "Cannot tell" is its own outcome. Folding it into "open" — which is what comparing
        # against CLOSED used to do — buries the issue silently and permanently: a number that
        # never resolves (deleted, transferred, a typo, another repository) reads as a blocker
        # that is always still open, so the rule can never fire on that issue again and says
        # nothing about why.
        [ -z "$unresolved" ] || note "$number" "$which references$unresolved, which resolve to no issue in $REPO — state unknown, rule 6 skipped — $title"
        [ -z "$unresolved" ] || continue

        [ -n "$closed_refs" ] || continue

        if [ "$which" = "S-blocked" ]; then
            # Only when nothing in the blocker set is still open. Which set that is was decided
            # above: the annotated one when the body named it, every reference otherwise.
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
        jq -r --arg kw "$BLOCKER_KEYWORDS" '
            def refs_all: [ (.body // "") | scan("#([0-9]+)") | .[0] ] | unique;
            # One keyword, then the whole run of numbers it introduces — "blocked by #273, #274 and
            # #275" names three blockers, and reading only the first is worse than reading none: the
            # rule would fire the moment #273 closed, while #274 and #275 were still open. The run
            # stops at the first thing that is not another #NNN, so "blocked by #276, which #277
            # supersedes" still yields #276 alone.
            def refs_annotated: [ (.body // "")
                | scan("(?i)(?:" + $kw + ")[ \t:*_`]*((?:#[0-9]+(?:[ \t]*(?:,|and|&|/)[ \t]*)?)+)")
                | .[0] | scan("#([0-9]+)") | .[0] ] | unique;
            def field: join(" ") | if . == "" then "-" else . end;
            .[]
            | . as $i
            | ([$i.labels[].name]) as $l
            | if ($l | index("S-blocked")) then
                  # Annotated blockers win when present; the all-references tier is the fallback
                  # for a body that named no blocker in a machine-readable form.
                  [ $i.number, "S-blocked",
                    (($i | refs_annotated) as $named
                     | (if ($named | length) > 0 then $named else ($i | refs_all) end) | field),
                    $i.title ]
              elif ($l | index("S-parked")) then
                  [ $i.number, "S-parked", (($i | refs_annotated) | field), $i.title ]
              else empty end
            | @tsv
        ' <<<"$issues"
    )
fi

# --- Rule 7: a closed issue still carrying unticked acceptance boxes -----------------------
# The tracker's unit of reference is the issue number, so anything finer is invisible to every
# mechanism here. An issue that bundles separable work therefore cannot be half-closed
# *legibly*: closing it discharges the whole number, including the parts nobody did.
#
# Measured on 2026-08-15: #355 carried three arms, one landed, and it was closed `completed`
# with three unticked boxes and a bolded paragraph saying the other two remained open. #356 was
# blocked on one of those two, could only name the issue, and read as unblocked.
#
# This does not forbid bundling — that is a judgement about how to write an issue. It makes the
# half-close loud, which is the damage. The fix at the source is sub-issues: one number each,
# closing one does not close the others, and a dependency becomes expressible.
#
# Bounded by `BOXES_SINCE` (declared above with the reason). ISO-8601 sorts lexicographically, so a
# date-only cutoff compares directly against `closedAt`'s full timestamp — no date arithmetic, and
# nothing to get wrong across the `date` implementations §3's SLA block already has to straddle.
if [ -n "${CLOSED_ISSUES_JSON:-}" ]; then
    closed=$(cat "$CLOSED_ISSUES_JSON")
elif [ -z "${ISSUES_JSON:-}" ] && command -v gh >/dev/null 2>&1; then
    closed=$(gh issue list --repo "$REPO" --state closed --limit 500 \
        --json number,title,body,closedAt)
else
    closed=""
fi

if [ -n "$closed" ]; then
    while IFS=$'\t' read -r number open_boxes title; do
        [ -n "$number" ] || continue
        report "$number" "closed with $open_boxes unticked acceptance box(es) — split it, or tick them — $title"
    done < <(
        jq -r --arg since "$BOXES_SINCE" '
            .[]
            | . as $i
            | select((.closedAt // "") >= $since)
            | ([ (.body // "") | scan("(?m)^[ \t]*[-*][ \t]+\\[[ ]\\]") ] | length) as $open
            | select($open > 0)
            | [ $i.number, $open, $i.title ] | @tsv
        ' <<<"$closed"
    )

    # The tail is a count, not a list: naming each of them every run would bury the window's
    # findings under history that is, by construction, never going to change.
    historical=$(jq --arg since "$BOXES_SINCE" '
        [ .[] | select((.closedAt // "") < $since)
              | select(([ (.body // "") | scan("(?m)^[ \t]*[-*][ \t]+\\[[ ]\\]") ] | length) > 0) ]
        | length' <<<"$closed")
    [ "$historical" -eq 0 ] ||
        echo "  · $historical issue(s) closed before $BOXES_SINCE carry unticked boxes — predate the rule, not gated"
fi

# --- Rule 8: every sub-issue closed, parent still open -------------------------------------
# The mirror of rule 7, and the reason sub-issues are the recommended shape rather than merely
# an allowed one: once work is split across numbers, "is the parent done" becomes a query
# instead of a memory. `C-tracking-issue` makes it airtight — `.github/labels.tsv` defines it as
# carrying no work of its own, so an open one with every child closed is a contradiction, not a
# judgement call. For any other parent the residual work may be real, so that one only notes.
#
# `sub_issues_summary` rides on the REST issue object; `gh issue list --json` does not expose
# it, hence the separate call. If the field is absent — older API, or a repository without the
# feature — the rule says so and skips, rather than reading a missing count as zero.
if [ -z "${ISSUES_JSON:-}" ] && command -v gh >/dev/null 2>&1; then
    parents=$(gh api "repos/$REPO/issues?state=open&per_page=100" --paginate 2>/dev/null \
        | jq -s 'add // [] | map(select(.pull_request == null))' 2>/dev/null || echo '[]')

    if [ "$(jq '[.[] | select(has("sub_issues_summary"))] | length' <<<"$parents")" -eq 0 ]; then
        echo "  · sub_issues_summary absent from the REST payload — rule 8 skipped, not passed"
    else
        while IFS=$'\t' read -r number tracking total title; do
            [ -n "$number" ] || continue
            if [ "$tracking" = "tracking" ]; then
                report "$number" "C-tracking-issue open with all $total sub-issues closed — it carries no work of its own — $title"
            else
                note "$number" "open with all $total sub-issues closed — check whether anything of its own is left — $title"
            fi
        done < <(
            jq -r '
                .[]
                | . as $i
                | (.sub_issues_summary // {}) as $s
                | select(($s.total // 0) > 0 and ($s.completed // 0) == $s.total)
                | [ $i.number,
                    (if ([$i.labels[].name] | index("C-tracking-issue")) then "tracking" else "-" end),
                    $s.total,
                    $i.title ] | @tsv
            ' <<<"$parents"
        )
    fi
fi

total=$(jq 'length' <<<"$issues")
untriaged=$(jq '[.[] | select([.labels[].name] | index("S-needs-triage"))] | length' <<<"$issues")

echo
printf '  %d open issues, %d awaiting triage, %d violation(s), %d note(s)\n' \
    "$total" "$untriaged" "$violations" "$notes"

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

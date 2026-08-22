#!/usr/bin/env bash
# Every closed `C-decision` issue must be cited in ARCHITECTURE.md or AGENTS.md -- the durable
# record its own label promises.
#
# `.github/labels.tsv`'s `C-decision` row reads "Closes with a recorded call, in ARCHITECTURE.md or
# AGENTS.md." Nothing checked that until now, and #410 found it was already false for five closed
# issues carrying the label: #298/#184 (recorded correctly, in ARCHITECTURE.md §7), #308 (recorded
# in AGENTS.md §11, not §7 as an earlier wording of the label implied), #288 (recorded in
# ARCHITECTURE.md §3.2 -- correctly, since #288's own acceptance box asked for §3.2, never §7), and
# #204 (recorded nowhere durable at all: its body pointed at `PROGRESS.md` §6, which no longer
# exists, and at #206 §5, itself just an open issue). That last one is what #410 fixed in the same
# PR that added this script -- see AGENTS.md §11's "Version to cut" line.
#
# What this checks, and what it deliberately does not:
#
#   - A citation is `#N` or `[#N]`, matching the citation styles both files already use --
#     the same two forms `check-doc-issue-claims.sh` and `check-closed-issue-rustdoc-refs.sh`
#     already parse for the same reason (`lib-...` scripts are not shared here only because the
#     bracket/bare split is a two-line grep, not the multi-line state machine those scripts need).
#   - It does not check that the citation is a good one -- only that one exists. Whether ARCHITECTURE.md
#     §3.2 or §7 is the right section for a given decision is a review question (#410's point 2:
#     a port decision belongs next to the port table, not force-marched into §7), not a mechanical
#     one this script can settle.
#   - It does not check open `C-decision` issues at all: an open decision has nothing to record yet
#     by definition.
#
# Runs on the same schedule as its `issue-integrity.yml` siblings for the same reason
# (`check-doc-issue-claims.sh`'s header): the subject is the tracker's *existing* state against the
# docs' existing state, not any one commit's diff -- a decision issue closing is nobody's push, and
# failing a commit for a fact that changed outside the committer's control punishes the wrong
# person at the wrong time. `pull_request` still re-runs it when this script or either file it reads
# changes, so a change to the check tests itself immediately rather than first executing on a
# schedule days later.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

REPO="${GITHUB_REPOSITORY:-Akvize/reconcile-rs}"

command -v gh >/dev/null || { echo "check-decision-issues-recorded: gh is required" >&2; exit 1; }

status=0
checked=0

# Every `#N` and `[#N]` across both files, deduplicated once -- a decision cited twice in one file,
# or once in each, is still recorded.
cited_numbers=$(grep -ohE '\[?#[0-9]+\]?' ARCHITECTURE.md AGENTS.md 2>/dev/null \
    | tr -d '[]#' | sort -un)

while IFS=$'\t' read -r number title; do
    [ -n "$number" ] || continue
    checked=$((checked + 1))

    if ! grep -qxF "$number" <<<"$cited_numbers"; then
        echo "check-decision-issues-recorded: #$number ($title) is a closed C-decision issue with no citation in ARCHITECTURE.md or AGENTS.md" >&2
        status=1
    fi
done < <(gh issue list --repo "$REPO" --state closed --label C-decision \
    --json number,title --jq '.[] | [.number, .title] | @tsv' 2>/dev/null || true)

echo "check-decision-issues-recorded: $checked closed C-decision issues checked"

if [ "$status" -ne 0 ]; then
    echo >&2
    echo "A closed C-decision issue's call must be cited in ARCHITECTURE.md or AGENTS.md -- wherever" >&2
    echo "it is topically at home (a port decision next to the port table, a publishing call in" >&2
    echo "AGENTS.md §11, a BYO-extension-point call in ARCHITECTURE.md §7), not necessarily §7 for" >&2
    echo "every decision (#410)." >&2
fi

exit "$status"

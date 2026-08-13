#!/usr/bin/env bash
# Applies `.github/labels.tsv` to the repository's GitHub labels, idempotently.
#
# The file is the source of truth; GitHub is the copy. This is the kubernetes/test-infra
# `label_sync` pattern at one-repo scale: the label set is reviewed as a diff like any other
# change, and a label created by hand in the web UI is either added to the file or removed
# by `--prune`. Without this, a taxonomy is a convention, and a convention held by eye is
# what AGENTS.md §10 forbids.
#
# Usage:
#   ./scripts/sync-labels.sh                 # dry run — prints the plan, changes nothing
#   ./scripts/sync-labels.sh --apply         # create and update
#   ./scripts/sync-labels.sh --apply --prune # also delete labels absent from the file
#
# `--prune` is opt-in and separate because deleting a label removes it from every issue that
# carries it, and that is not recoverable from this file. Run the dry run first, read the
# DELETE lines, then decide.
#
# Renaming beats deleting: `gh label edit OLD --name NEW` keeps the label on every issue it
# is already applied to. The three GitHub defaults this repository has used map straight
# across, so run these *before* the first sync and migration keeps its history:
#   gh label edit bug           --name C-bug
#   gh label edit documentation --name C-docs
#   gh label edit enhancement   --name C-feature
#
# Requires: gh (authenticated, or GH_TOKEN in the environment with `issues: write`).
set -Eeuo pipefail

# Resolve the repo root from the script's own location, matching the other scripts here.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

LABELS_FILE=.github/labels.tsv
REPO=${REPO:-Akvize/reconcile-rs}

apply=false
prune=false
for arg in "$@"; do
    case "$arg" in
        --apply) apply=true ;;
        --prune) prune=true ;;
        *)
            echo "sync-labels: unknown argument '$arg' (expected --apply and/or --prune)" >&2
            exit 2
            ;;
    esac
done

if [ ! -f "$LABELS_FILE" ]; then
    echo "sync-labels: $LABELS_FILE is missing" >&2
    exit 1
fi

if ! command -v gh >/dev/null 2>&1; then
    echo "sync-labels: the GitHub CLI (gh) is required" >&2
    exit 1
fi

# Existing labels, one name per line. `gh label list` paginates at 30 by default, which is
# under the size of the taxonomy itself — the explicit limit is load-bearing, not cosmetic.
existing=$(gh label list --repo "$REPO" --limit 200 --json name --jq '.[].name')

declare -A wanted=()
status=0
created=0
updated=0

while IFS=$'\t' read -r name color description || [ -n "$name" ]; do
    # Skip comments and blank lines. Leading whitespace is not tolerated: a `#` that is not
    # in column 1 is far more likely to be a typo in a description than a deliberate comment.
    case "$name" in ''|'#'*) continue ;; esac

    if [ -z "$color" ] || [ -z "$description" ]; then
        echo "sync-labels: '$name' is missing a colour or a description (expected 3 tab-separated fields)" >&2
        status=1
        continue
    fi
    if ! [[ "$color" =~ ^[0-9a-fA-F]{6}$ ]]; then
        echo "sync-labels: '$name' has colour '$color', expected 6 hex digits with no leading #" >&2
        status=1
        continue
    fi

    wanted["$name"]=1

    if grep -Fxq "$name" <<<"$existing"; then
        updated=$((updated + 1))
        printf '  UPDATE  %-28s #%s\n' "$name" "$color"
        if $apply; then
            gh label edit "$name" --repo "$REPO" --color "$color" --description "$description" >/dev/null
        fi
    else
        created=$((created + 1))
        printf '  CREATE  %-28s #%s\n' "$name" "$color"
        if $apply; then
            gh label create "$name" --repo "$REPO" --color "$color" --description "$description" >/dev/null
        fi
    fi
done <"$LABELS_FILE"

if [ "$status" -ne 0 ]; then
    echo >&2
    echo "sync-labels: $LABELS_FILE has malformed rows; nothing was applied" >&2
    exit "$status"
fi

deleted=0
while IFS= read -r name; do
    [ -n "$name" ] || continue
    if [ -z "${wanted[$name]+set}" ]; then
        deleted=$((deleted + 1))
        printf '  DELETE  %-28s (absent from %s)\n' "$name" "$LABELS_FILE"
        if $apply && $prune; then
            gh label delete "$name" --repo "$REPO" --yes >/dev/null
        fi
    fi
done <<<"$existing"

echo
printf '  %d to create, %d to update, %d not in the file\n' "$created" "$updated" "$deleted"
if ! $apply; then
    echo "  dry run — re-run with --apply (and --prune to act on the DELETE lines)"
elif [ "$deleted" -gt 0 ] && ! $prune; then
    echo "  the DELETE lines were left alone — pass --prune to act on them"
fi

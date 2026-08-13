#!/usr/bin/env bash
# Catches the failure mode found live in #203/#312: an issue closed as fixed, citing a commit SHA
# as the resolution, where that SHA only ever landed on the PR branch that claimed to close the
# issue -- and the PR itself was never merged. `git log --all` finds the commit fine (it is real,
# on some branch), so nothing before this script would ever flag it; `git merge-base --is-ancestor
# <sha> main` does not, which is exactly the gap this closes. Same class as the #218-#221 pattern
# PROGRESS.md already documents for PR bodies -- this is the analogous check for issue bodies.
#
# Requires `gh` (authenticated -- GH_TOKEN/GITHUB_TOKEN in env, as GitHub Actions sets by default)
# and a full clone: a shallow checkout would make every real, merged SHA look unreachable too.
#
# Scheduled (.github/workflows/issue-integrity.yml), not gated per-push: this audits the issue
# tracker's existing state, not the diff of any one commit, so there is no single push it belongs
# to.
set -Eeuo pipefail

REPO="${GITHUB_REPOSITORY:-Akvize/reconcile-rs}"
# `origin/main`, not `main`: a local branch pointer can be stale relative to the remote (e.g. this
# script run from a feature branch's checkout), and staleness here would misreport a merged SHA as
# missing. `actions/checkout` leaves `origin/main` accurate regardless of which ref is checked out.
BASE="${1:-origin/main}"

if [ "$(git rev-parse --is-shallow-repository)" = "true" ]; then
    echo "check-closed-issue-shas: shallow clone -- fetch full history first (checkout: fetch-depth: 0)" >&2
    exit 1
fi

status=0
count=0

# Process substitution, not a pipe: a `... | while read` pipeline runs the loop body in a
# subshell, so `status=1` set inside it would be invisible to the `exit $status` below.
while IFS= read -r issue; do
    number=$(jq -r '.number' <<<"$issue")
    url=$(jq -r '.url' <<<"$issue")
    body=$(jq -r '.body' <<<"$issue")

    # Only backtick-quoted hex tokens: the repo's own convention for citing a resolving commit
    # (see #203, #312 as closed). A bare hex-looking word loose in prose is not a claim to check,
    # and checking it anyway would make this fail on coincidence rather than on a real citation.
    shas=$(grep -oE '`[0-9a-f]{7,40}`' <<<"$body" | tr -d '`' | sort -u || true)

    for sha in $shas; do
        count=$((count + 1))
        if ! git cat-file -e "${sha}^{commit}" 2>/dev/null; then
            continue # not a commit this clone knows about -- prose, not a citation
        fi
        if ! git merge-base --is-ancestor "$sha" "$BASE" 2>/dev/null; then
            echo "check-closed-issue-shas: issue #$number ($url) cites $sha, which is not an ancestor of $BASE" >&2
            status=1
        fi
    done
done < <(gh issue list --repo "$REPO" --state closed --limit 500 --json number,url,body | jq -c '.[]')

echo "check-closed-issue-shas: checked $count cited SHA(s) across closed issues"
exit "$status"

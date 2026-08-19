#!/usr/bin/env bash
set -Eeuo pipefail

# Non-gating cognitive-complexity and duplication trend report -- no threshold here fails a build,
# unlike every `check-*.sh` script in this directory. It exists so a human can watch a trend line
# without adding a new number a script has to enforce by eye (AGENTS.md §10's "wired into a hook
# and CI" only applies to a rule that actually has a right answer; "should this function be
# simpler" doesn't). Requires `rust-code-analysis-cli` and `jscpd` on PATH -- the `code-quality`
# job in `.github/workflows/main.yml` installs both; this script never installs anything itself so
# it can also be run from a local checkout without network access assumptions.
#
# Only exits non-zero if a tool itself fails (bad install, unreadable source) -- never on a
# complexity or duplication finding.

GIT_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$GIT_ROOT"

CRATE_SRC_DIRS=(src rsos/src rbsr/src lww-register/src gossip/src)
TOP_N=15

RCA_OUT=$(mktemp -d)
JSCPD_OUT=$(mktemp -d)
trap 'rm -rf "$RCA_OUT" "$JSCPD_OUT"' EXIT

echo "## Cognitive complexity (rust-code-analysis)"
echo
# -p takes one path per occurrence, not a list -- build the flag pairs rather than passing
# CRATE_SRC_DIRS as one argument.
RCA_PATH_ARGS=()
for dir in "${CRATE_SRC_DIRS[@]}"; do
    RCA_PATH_ARGS+=(-p "$dir")
done
rust-code-analysis-cli -m -O json -o "$RCA_OUT" "${RCA_PATH_ARGS[@]}" -j "$(nproc)" >/dev/null

echo "Top $TOP_N functions by cognitive complexity (higher = harder to hold in your head reading"
echo "top to bottom -- Mozilla's rust-code-analysis definition, not cyclomatic path count):"
echo
echo '| complexity | function |'
echo '|---|---|'
# Slice the top N with awk's NR rather than piping through `head`: `head` closing the pipe early
# sends `sort` a SIGPIPE, which `pipefail` (set above) turns into a spurious script failure.
find "$RCA_OUT" -name '*.json' -print0 |
    xargs -0 -I{} jq -r '.. | objects | select(.kind? == "function") | "\(.metrics.cognitive.sum)\t\(.name)"' {} |
    sort -t $'\t' -k1 -rn |
    awk -F'\t' -v n="$TOP_N" 'NR <= n {printf "| %s | `%s` |\n", $1, $2}'

echo
echo "## Duplication (jscpd, >=10 lines / >=50 tokens)"
echo
# --silent only quiets the progress bar; it still prints a one-line summary regardless of
# reporter choice. Redirect it -- the parsed JSON below is the report this script emits.
jscpd --min-lines 10 --min-tokens 50 --reporters json --silent -o "$JSCPD_OUT" \
    --ignore '**/target/**' "${CRATE_SRC_DIRS[@]}" >/dev/null

jq -r '
  .statistics.total as $t
  | "- \($t.clones) clone pairs across \($t.sources) files",
    "- \($t.duplicatedLines) duplicated lines / \($t.lines) total (\($t.percentage)%)"
' "$JSCPD_OUT/jscpd-report.json"

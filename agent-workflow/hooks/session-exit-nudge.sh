#!/usr/bin/env bash
# Stop hook — one-shot reminder to run the exit review.
#
# Design constraint worth stating plainly: SessionEnd hooks cannot drive the
# model (no context injection, 1.5s budget), so a genuinely automatic
# "run the retrospective as the session closes" is impossible. Stop is the only
# event that both fires late and can reach the model. It fires every turn, so
# this hook is strictly one-shot and only trips once real work exists.
#
# Modes via AW_RETRO_MODE: nudge (default) | block | off

set -uo pipefail
trap 'exit 0' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh" 2>/dev/null || exit 0

MODE="${AW_RETRO_MODE:-nudge}"
[ "$MODE" = "off" ] && exit 0

INPUT="$(cat 2>/dev/null)"
SESSION_ID="$(aw_json_get "$INPUT" session_id)"
CWD="$(aw_json_get "$INPUT" cwd)"
[ -n "$CWD" ] || CWD="$PWD"
[ -n "$SESSION_ID" ] || exit 0

SDIR="$(aw_session_dir "$SESSION_ID" "$CWD")"
[ -f "$SDIR/retro-done" ] && exit 0
[ -f "$SDIR/nudged" ] && [ "$MODE" != "block" ] && exit 0

[ -f "$SDIR/baseline.env" ] || exit 0
# shellcheck disable=SC1091
. "$SDIR/baseline.env" 2>/dev/null || exit 0

ROOT="${root:-$(aw_repo_root "$CWD")}"
BASE="${base_sha:-}"
[ -n "$BASE" ] && [ "$BASE" != "none" ] || exit 0

COMMITS="$(git -C "$ROOT" rev-list --count "$BASE..HEAD" 2>/dev/null || echo 0)"
NOW_DIRTY="$(aw_status_count "$ROOT")"
NOW_HASH="$(aw_status_hash "$ROOT")"
BASE_DIRTY="${base_dirty:-0}"
BASE_HASH="${base_status_hash:-}"

# Only trip once the session has actually produced something. Compare the whole
# working-tree fingerprint, not a file count: counts miss both new untracked
# directories and edits that net out to the same total.
if [ "${COMMITS:-0}" -eq 0 ] && [ -n "$BASE_HASH" ] && [ "$NOW_HASH" = "$BASE_HASH" ]; then
  exit 0
fi

touch "$SDIR/nudged" 2>/dev/null

MSG="Exit review not yet run for this session ($COMMITS commit(s), $NOW_DIRTY uncommitted file(s) vs $BASE_DIRTY at start).

Before this session ends, run the \`session-end\` skill (\`/session-end\`). It gates on:
doc consistency, convention conformance, scope drift, verification honesty,
blast radius + secret sweep, agent retrospective, repo retrospective, handoff.

Journal evidence: \`$SDIR/journal.jsonl\`. Do not synthesise the retrospective from memory.
If the user is clearly mid-task, finish the task first — this is a reminder, not an interrupt."

if [ "$MODE" = "block" ]; then
  if command -v python3 >/dev/null 2>&1; then
    AW_MSG="$MSG" python3 -c '
import json, os
print(json.dumps({"decision": "block", "reason": os.environ["AW_MSG"]}))
' 2>/dev/null && exit 0
  fi
fi

aw_emit_context "Stop" "$MSG"
exit 0

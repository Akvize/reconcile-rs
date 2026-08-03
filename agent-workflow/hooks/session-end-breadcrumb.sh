#!/usr/bin/env bash
# SessionEnd hook — the safety net.
#
# SessionEnd cannot talk to the model, so it cannot run the retrospective. What
# it CAN do is leave a breadcrumb, so the next session's bootstrap notices the
# missed review and offers to run it over the preserved journal. That closes the
# loop without needing the departing session to cooperate.

set -uo pipefail
trap 'exit 0' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh" 2>/dev/null || exit 0

INPUT="$(cat 2>/dev/null)"
SESSION_ID="$(aw_json_get "$INPUT" session_id)"
CWD="$(aw_json_get "$INPUT" cwd)"
REASON="$(aw_json_get "$INPUT" reason)"
[ -n "$CWD" ] || CWD="$PWD"
[ -n "$SESSION_ID" ] || exit 0

STATE_DIR="$(aw_state_dir "$CWD")"
SDIR="$(aw_session_dir "$SESSION_ID" "$CWD")"

aw_journal_append "$SDIR" session_end "reason=${REASON:-other}"

if [ -f "$SDIR/retro-done" ]; then
  rm -f "$STATE_DIR/retro-pending" 2>/dev/null
else
  printf '%s\n' "$SDIR/journal.jsonl" >"$STATE_DIR/retro-pending" 2>/dev/null
fi
exit 0

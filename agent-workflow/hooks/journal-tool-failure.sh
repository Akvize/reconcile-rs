#!/usr/bin/env bash
# PostToolUseFailure hook — free, automatic friction capture.
#
# Every failed tool call is a bump: a wrong assumption, a missing dependency, a
# command that does not exist here, a permission that was not granted. Recording
# them costs nothing and turns the exit retrospective from recollection into
# evidence. This is the single highest-leverage hook in the bundle.

set -uo pipefail
trap 'exit 0' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh" 2>/dev/null || exit 0

INPUT="$(cat 2>/dev/null)"
SESSION_ID="$(aw_json_get "$INPUT" session_id)"
CWD="$(aw_json_get "$INPUT" cwd)"
TOOL="$(aw_json_get "$INPUT" tool_name)"
ERR="$(aw_json_get "$INPUT" error)"
[ -n "$CWD" ] || CWD="$PWD"
[ -n "$SESSION_ID" ] || exit 0

# Keep entries bounded — a 400-line stack trace helps nobody at synthesis time.
ERR="$(printf '%s' "$ERR" | tr '\n' ' ' | cut -c1-400)"

SDIR="$(aw_session_dir "$SESSION_ID" "$CWD")"
aw_journal_append "$SDIR" tool_failure "${TOOL:-unknown}: ${ERR:-(no error text)}"
exit 0

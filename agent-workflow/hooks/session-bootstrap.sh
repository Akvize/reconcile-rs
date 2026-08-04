#!/usr/bin/env bash
# SessionStart hook — deterministic half of session bootstrap.
#
# Establishes the facts an agent otherwise guesses at: what proves this code
# correct, what conventions bind it, where the git baseline sits, and what the
# previous session left unfinished. Injected as additionalContext.
#
# Reasoning is deliberately NOT done here — that belongs to the session-start
# skill. This hook only supplies evidence.

set -uo pipefail
trap 'exit 0' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh" 2>/dev/null || exit 0

INPUT="$(cat 2>/dev/null)"
SESSION_ID="$(aw_json_get "$INPUT" session_id)"
SOURCE="$(aw_json_get "$INPUT" source)"
CWD="$(aw_json_get "$INPUT" cwd)"
[ -n "$CWD" ] || CWD="$PWD"
[ -n "$SESSION_ID" ] || SESSION_ID="unknown"

# Resumed/compacted sessions already carry the bootstrap in context.
case "$SOURCE" in
resume | compact) exit 0 ;;
esac

ROOT="$(aw_repo_root "$CWD")"
STATE_DIR="$(aw_state_dir "$CWD")"
SDIR="$(aw_session_dir "$SESSION_ID" "$CWD")"

BRANCH="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '(no git)')"
HEAD_SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo none)"
DIRTY="$(aw_status_count "$ROOT")"
STATUS_HASH="$(aw_status_hash "$ROOT")"

# Baseline snapshot: lets the exit gate diff exactly what THIS session changed,
# rather than trusting the agent's recollection of what it touched.
{
  printf 'session_id=%s\n' "$SESSION_ID"
  printf 'started=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)"
  printf 'root=%s\n' "$ROOT"
  printf 'branch=%s\n' "$BRANCH"
  printf 'base_sha=%s\n' "$HEAD_SHA"
  printf 'base_dirty=%s\n' "$DIRTY"
  printf 'base_status_hash=%s\n' "$STATUS_HASH"
} >"$SDIR/baseline.env" 2>/dev/null

printf '%s\n' "$SESSION_ID" >"$STATE_DIR/current-session" 2>/dev/null
aw_journal_append "$SDIR" session_start "branch=$BRANCH base=$HEAD_SHA dirty=$DIRTY"

# Expose the session's paths to every later Bash command. This is what removes
# the need for a bespoke CLI: the skills can use plain one-liners against these
# variables, so there is a single code path whether or not anything else is
# installed. The values are also printed below, in case the env file is absent.
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  {
    printf 'AW_SESSION_DIR=%s\n' "$SDIR"
    printf 'AW_JOURNAL=%s\n' "$SDIR/journal.tsv"
    printf 'AW_BASE_SHA=%s\n' "$HEAD_SHA"
    printf 'AW_STATE_DIR=%s\n' "$STATE_DIR"
  } >>"$CLAUDE_ENV_FILE" 2>/dev/null
fi

CONVENTIONS="$(aw_find_convention_files "$ROOT")"
LINTERS="$(aw_find_lint_configs "$ROOT")"
VERIFY="$(aw_detect_verification "$ROOT")"
DOCS="$(aw_doc_surface "$ROOT")"

OUT="## Session bootstrap (agent-workflow)

**Baseline** — branch \`$BRANCH\`, HEAD \`${HEAD_SHA:0:12}\`, $DIRTY uncommitted file(s) at start.

\`\$AW_BASE_SHA\`, \`\$AW_JOURNAL\`, \`\$AW_SESSION_DIR\` and \`\$AW_STATE_DIR\` are set for
this session. Record friction the moment it happens — one line, no ceremony:

\`\`\`bash
printf '%s\\tfriction\\t%s\\n' \"\$(date -u +%FT%TZ)\" \"README documents a flag the binary rejects\" >> \"\$AW_JOURNAL\"
\`\`\`

Kinds: \`friction\` \`decision\` \`anomaly\` \`assumption\` \`unverified\` \`scope\` \`debt\`.
Failed tool calls are journalled automatically; record what a hook cannot see.
Journal: \`$SDIR/journal.tsv\`

### How this project proves itself
Run these before claiming anything works. Do not invent commands.
${VERIFY:-  (none detected — ASK the user how to build/test/lint before writing code)}

### Convention sources
${CONVENTIONS:-  (none found)}
### Linter/formatter configs
${LINTERS:-  (none found)}
### Docs whose accuracy is a deliverable
${DOCS:-  (none found)}
"

# A repo driven by agents with no agent-instruction file is a standing defect:
# every session re-derives the same conventions and re-makes the same mistakes.
if ! printf '%s' "$CONVENTIONS" | grep -qE '^(CLAUDE\.md|AGENTS\.md)$'; then
  OUT="$OUT
> **Anomaly:** no \`CLAUDE.md\` or \`AGENTS.md\` in this repo. Conventions must be
> re-derived from scratch every session. Flag this in the exit retrospective and
> propose one seeded from what you learn today.
"
fi

# Unfinished business from last time.
if [ -f "$STATE_DIR/HANDOFF.md" ]; then
  OUT="$OUT
### Previous session handoff
\`\`\`
$(head -60 "$STATE_DIR/HANDOFF.md" 2>/dev/null)
\`\`\`
"
fi

if [ -f "$STATE_DIR/retro-pending" ]; then
  OUT="$OUT
> **Recovery:** the previous session ended without running its exit review.
> Its journal is at \`$(cat "$STATE_DIR/retro-pending" 2>/dev/null)\`.
> Offer to run \`/session-end\` over it before starting new work.
"
fi

OUT="$OUT
**Next:** invoke the \`session-start\` skill to turn these facts into a plan.
Confirm the verification loop actually runs *before* writing code."

aw_emit_context "SessionStart" "$OUT"
exit 0

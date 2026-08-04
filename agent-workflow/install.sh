#!/usr/bin/env bash
# Install the agent-workflow bundle into ~/.claude so it applies to every project.
#
# Idempotent: re-running upgrades in place and never duplicates hook entries.
# Your existing settings.json hooks are preserved.
#
#   ./install.sh              install / upgrade
#   ./install.sh --uninstall  remove hooks, skills, and the bundle (state is kept)
#   ./install.sh --dry-run    show what would change

set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLAUDE_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
DEST="$CLAUDE_DIR/agent-workflow"
SKILLS_DIR="$CLAUDE_DIR/skills"
SETTINGS="$CLAUDE_DIR/settings.json"

MODE=install
case "${1:-}" in
--uninstall) MODE=uninstall ;;
--dry-run) MODE=dryrun ;;
esac

command -v python3 >/dev/null 2>&1 || {
  echo "error: python3 is required to merge settings.json safely." >&2
  exit 1
}

HOOK_PATHS='["~/.claude/agent-workflow/hooks/session-bootstrap.sh",
             "~/.claude/agent-workflow/hooks/journal-tool-failure.sh",
             "~/.claude/agent-workflow/hooks/session-exit-nudge.sh",
             "~/.claude/agent-workflow/hooks/session-end-breadcrumb.sh"]'

merge_settings() {
  SETTINGS="$SETTINGS" SNIPPET="$SRC/settings.snippet.json" MODE="$MODE" \
    HOOK_PATHS="$HOOK_PATHS" python3 <<'PY'
import json, os, sys

settings_path = os.environ["SETTINGS"]
mode = os.environ["MODE"]
ours = set(json.loads(os.environ["HOOK_PATHS"]))

try:
    with open(settings_path) as f:
        settings = json.load(f)
except FileNotFoundError:
    settings = {}
except json.JSONDecodeError as e:
    sys.exit(f"error: {settings_path} is not valid JSON ({e}); fix it first.")

hooks = settings.setdefault("hooks", {})

def strip_ours():
    """Remove only our hook entries, leaving everything else untouched."""
    for event in list(hooks):
        kept_groups = []
        for group in hooks.get(event) or []:
            kept = [h for h in (group.get("hooks") or [])
                    if h.get("command") not in ours]
            if kept:
                kept_groups.append({**group, "hooks": kept})
        if kept_groups:
            hooks[event] = kept_groups
        else:
            hooks.pop(event, None)

strip_ours()

if mode != "uninstall":
    with open(os.environ["SNIPPET"]) as f:
        snippet = json.load(f)
    for event, groups in snippet["hooks"].items():
        hooks.setdefault(event, []).extend(groups)

if not hooks:
    settings.pop("hooks", None)
settings.setdefault("$schema",
                    "https://json.schemastore.org/claude-code-settings.json")

if mode == "dryrun":
    print(json.dumps(settings, indent=2))
else:
    if os.path.exists(settings_path):
        with open(settings_path) as f:
            backup = f.read()
        with open(settings_path + ".agent-workflow.bak", "w") as f:
            f.write(backup)
    os.makedirs(os.path.dirname(settings_path), exist_ok=True)
    with open(settings_path, "w") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")
    print(f"settings.json updated ({mode})")
PY
}

if [ "$MODE" = "uninstall" ]; then
  merge_settings
  rm -rf "$DEST"
  for s in session-start session-end session-note; do rm -rf "${SKILLS_DIR:?}/$s"; done
  echo "uninstalled. session state preserved in ~/.claude/agent-workflow-state"
  exit 0
fi

if [ "$MODE" = "dryrun" ]; then
  echo "would install bundle -> $DEST"
  echo "would install skills -> $SKILLS_DIR/{session-start,session-end,session-note}"
  echo "resulting settings.json:"
  merge_settings
  exit 0
fi

mkdir -p "$DEST" "$SKILLS_DIR"
cp -R "$SRC/hooks" "$SRC/settings.snippet.json" "$DEST/"
chmod +x "$DEST"/hooks/*.sh 2>/dev/null || true

for s in session-start session-end session-note; do
  rm -rf "${SKILLS_DIR:?}/$s"
  cp -R "$SRC/skills/$s" "$SKILLS_DIR/$s"
done

merge_settings

echo
echo "installed:"
echo "  bundle  $DEST"
echo "  skills  session-start, session-end, session-note"
echo "  state   ~/.claude/agent-workflow-state  (never written into your repos)"
echo
echo "retrospective trigger: \${AW_RETRO_MODE:-nudge}  (nudge | block | off)"

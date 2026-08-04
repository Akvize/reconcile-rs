#!/bin/bash
# ---------------------------------------------------------------------------
# Paste this into the Setup script field of your cloud environment at
# claude.ai/code (cloud icon above the message box -> hover environment ->
# settings gear -> Setup script).
#
# It installs the agent-workflow hooks into ~/.claude inside the session VM, so
# EVERY cloud session in that environment gets them natively — web, terminal
# `claude --cloud`, mobile, Desktop, Claude Tag and routines — regardless of
# which repository is attached. Nothing has to be committed to your repos.
#
# Why this works: a cloud session's Claude Code reads ~/.claude/settings.json as
# its user-settings layer exactly as a local one does. What does NOT carry over
# is your laptop's ~/.claude, because it is never uploaded — a transport gap,
# not a policy block. The setup script runs as root before Claude Code launches,
# so anything it writes into the VM's home directory is read normally.
#
# Skills do not need this: skills enabled on claude.ai load in every session
# already. This script is only for the hooks — automatic journalling, the
# baseline snapshot, the one-shot exit nudge, and the breadcrumb.
#
# Constraints this script is written around (see docs/en/cloud-environments):
#   - must exit zero, or the session fails to start
#   - must finish within ~5 minutes
#   - result is snapshotted and reused, so it runs once, not per session
# ---------------------------------------------------------------------------

set -u

# Public repo holding the bundle. Change to your own once you relocate it.
BUNDLE_REPO="${AW_BUNDLE_REPO:-https://github.com/adriendellagaspera/dotfiles.git}"
BUNDLE_PATH="${AW_BUNDLE_PATH:-claude/agent-workflow}"

TMP="$(mktemp -d)"
if git clone --depth 1 --quiet "$BUNDLE_REPO" "$TMP/src" 2>/dev/null &&
  [ -d "$TMP/src/$BUNDLE_PATH" ]; then
  ( cd "$TMP/src/$BUNDLE_PATH" && ./install.sh ) || true
  echo "agent-workflow installed from $BUNDLE_REPO"
else
  echo "agent-workflow: could not fetch $BUNDLE_REPO/$BUNDLE_PATH — skipping" >&2
fi
rm -rf "$TMP"

# Never fail the session over optional tooling.
exit 0

#!/usr/bin/env bash
# SessionStart hook for Claude Code on the web.
#
# Installs the one tool AGENTS.md §3's list needs that the web container does not ship. Everything
# else on that list is `cargo` plus a stable toolchain, both already present; `cargo deny check` is
# the last line and the only one that needs a subcommand of its own.
#
# The point is not convenience. §3 says the list *is* what "done" means, so a session that cannot
# run its last line cannot report the gate green — it can only report fourteen of fifteen and hope.
# Installing the tool is cheaper than remembering the exception.
#
# Not pinned, on purpose: CI runs `EmbarkStudios/cargo-deny-action@v2` (`.github/workflows/main.yml`),
# which resolves its own cargo-deny inside the v2 line rather than at a fixed version. Pinning here
# would introduce a local/CI version skew that nobody is watching, which is the failure this hook
# exists to remove. `--locked` still builds from the crate's own lockfile, so the build is
# reproducible for whatever version is current.
#
# Web only. A local checkout gets its toolchain from CONTRIBUTING.md's Dev Container; installing into
# a contributor's machine from a hook they did not ask for is not this script's business.
set -euo pipefail

[ "${CLAUDE_CODE_REMOTE:-}" = "true" ] || exit 0

if command -v cargo-deny >/dev/null 2>&1; then
    echo "session-start: $(cargo deny --version) already installed"
    exit 0
fi

echo "session-start: installing cargo-deny (AGENTS.md §3's last gate line) ..."
cargo install cargo-deny --locked
echo "session-start: $(cargo deny --version) ready"

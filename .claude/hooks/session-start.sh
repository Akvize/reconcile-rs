#!/usr/bin/env bash
# SessionStart hook for Claude Code on the web.
#
# Three things a web container starts without, that CONTRIBUTING.md's Dev Container starts with:
# the git hooks linked (AGENTS.md §2), cargo-deny installed (AGENTS.md §3's last gate line), and
# cargo-nextest installed (the pre-push tier, AGENTS.md §3's table). Missing any of these turns
# into the same failure from two directions: pre-commit/pre-push never firing on `git commit`/
# `git push`, so an agent has to remember to run their equivalent by hand every time instead of
# getting it for free -- or an agent that reaches a gate line unable to run it at all, reporting it
# green having actually run N-1 of N and hoped (cargo-nextest missing silently falls through to
# `git push --no-verify`-shaped trouble: the pre-push hook itself fails outright rather than
# skipping the check, but only after the agent has already burned time treating `cargo test` as
# equivalent). Doing all three here is cheaper than either an agent remembering the setup step or a
# docs line asking it to.
#
# Neither tool is pinned, on purpose: CI resolves cargo-deny via `EmbarkStudios/cargo-deny-action@v2`
# (`.github/workflows/main.yml`), which floats inside the v2 line rather than a fixed version, and
# cargo-nextest has no CI-side pin either. Pinning here would introduce a local/CI version skew
# that nobody is watching. `--locked` still builds from the crate's own lockfile, so the build is
# reproducible for whatever version is current.
#
# Web only. A local checkout gets both from CONTRIBUTING.md's Dev Container; touching a
# contributor's own machine from a hook they did not ask for is not this script's business.
set -euo pipefail

[ "${CLAUDE_CODE_REMOTE:-}" = "true" ] || exit 0

GIT_ROOT=$(git rev-parse --show-toplevel)
for hook in pre-commit pre-push; do
    link="$GIT_ROOT/.git/hooks/$hook"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "../../$hook" ]; then
        continue
    fi
    ln -sf "../../$hook" "$link"
    echo "session-start: linked $hook"
done

if command -v cargo-deny >/dev/null 2>&1; then
    echo "session-start: $(cargo deny --version) already installed"
else
    echo "session-start: installing cargo-deny (AGENTS.md §3's last gate line) ..."
    cargo install cargo-deny --locked
    echo "session-start: $(cargo deny --version) ready"
fi

if command -v cargo-nextest >/dev/null 2>&1; then
    echo "session-start: $(cargo nextest --version | head -1) already installed"
else
    echo "session-start: installing cargo-nextest (the pre-push tier, AGENTS.md §3) ..."
    cargo install cargo-nextest --locked
    echo "session-start: $(cargo nextest --version | head -1) ready"
fi

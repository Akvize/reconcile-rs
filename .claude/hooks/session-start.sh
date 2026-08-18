#!/usr/bin/env bash
# SessionStart hook for Claude Code on the web.
#
# Two things a web container starts without, that CONTRIBUTING.md's Dev Container starts with:
# the git hooks linked (AGENTS.md §2) and cargo-deny installed (AGENTS.md §3's last gate line).
# Missing either turns into the same failure from two directions: pre-commit/pre-push never firing
# on `git commit`/`git push`, so an agent has to remember to run their equivalent by hand every
# time instead of getting it for free -- or an agent that reaches §3's last line unable to run it
# at all, reporting the gate green having actually run fourteen of fifteen and hoped. Doing both
# here is cheaper than either an agent remembering the setup step or a docs line asking it to.
#
# cargo-deny is not pinned, on purpose: CI runs `EmbarkStudios/cargo-deny-action@v2`
# (`.github/workflows/main.yml`), which resolves its own cargo-deny inside the v2 line rather than
# at a fixed version. Pinning here would introduce a local/CI version skew that nobody is
# watching. `--locked` still builds from the crate's own lockfile, so the build is reproducible
# for whatever version is current.
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
    exit 0
fi

echo "session-start: installing cargo-deny (AGENTS.md §3's last gate line) ..."
cargo install cargo-deny --locked
echo "session-start: $(cargo deny --version) ready"

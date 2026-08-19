#!/usr/bin/env bash
# SessionStart hook for Claude Code on the web.
#
# Five things a web container starts without, that CONTRIBUTING.md's Dev Container starts with:
# the git hooks linked (AGENTS.md §2), cargo-deny installed (AGENTS.md §3's last gate line),
# cargo-nextest installed (the pre-push tier, AGENTS.md §3's table), cargo-mutants installed
# (the `repo-gates` job's `check-mutant-count.sh`, CI-only -- not in the pre-commit/pre-push
# tiers, but its counts are version-sensitive, so a local run without it either can't check the
# gate at all or, worse, reports numbers that don't match CI's pinned version and sends an agent
# chasing a phantom drift), and gitleaks installed (the pre-commit tier's secret scan). Missing
# any of these turns into the same failure from two directions:
# pre-commit/pre-push never firing on `git commit`/`git push`, so an agent has to remember to run
# their equivalent by hand every time instead of getting it for free -- or an agent that reaches a
# gate line unable to run it at all, reporting it green having actually run N-1 of N and hoped
# (cargo-nextest missing silently falls through to `git push --no-verify`-shaped trouble: the
# pre-push hook itself fails outright rather than skipping the check, but only after the agent has
# already burned time treating `cargo test` as equivalent). Doing all four here is cheaper than
# either an agent remembering the setup step or a docs line asking it to.
#
# cargo-deny and cargo-nextest are deliberately unpinned: CI resolves cargo-deny via
# `EmbarkStudios/cargo-deny-action@v2` (`.github/workflows/main.yml`), which floats inside the v2
# line rather than a fixed version, and cargo-nextest has no CI-side pin either -- pinning here
# would introduce a local/CI version skew that nobody is watching. cargo-mutants is the opposite:
# CI pins it exactly (`taiki-e/install-action`, `cargo-mutants@27.1.0` in `main.yml`), and
# `check-mutant-count.sh`'s numbers are cargo-mutants-version-dependent, so matching that pin here
# is what makes a local re-run of the gate mean anything. `--locked` still builds from each
# crate's own lockfile, so every install here is reproducible for the version it targets.
# gitleaks is pinned for a third reason: it has no cargo crate, and the agent proxy this session
# runs behind does not resolve GitHub's releases-API "latest" redirect (only a versioned release
# asset URL), so there is no floating target to track even if we wanted one. `Dockerfile.dev`
# pins the identical version for the same reason.
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

# Pinned to match main.yml's `taiki-e/install-action` `cargo-mutants@27.1.0` exactly -- see the
# header comment above for why this one tool is pinned when cargo-deny/cargo-nextest are not.
MUTANTS_VERSION=27.1.0
if command -v cargo-mutants >/dev/null 2>&1 && [ "$(cargo mutants --version | awk '{print $2}')" = "$MUTANTS_VERSION" ]; then
    echo "session-start: cargo-mutants $MUTANTS_VERSION already installed"
else
    echo "session-start: installing cargo-mutants $MUTANTS_VERSION (repo-gates' check-mutant-count.sh) ..."
    cargo install cargo-mutants --locked --version "$MUTANTS_VERSION"
    echo "session-start: $(cargo mutants --version) ready"
fi

GITLEAKS_VERSION=8.21.2
if command -v gitleaks >/dev/null 2>&1 && [ "$(gitleaks version)" = "$GITLEAKS_VERSION" ]; then
    echo "session-start: gitleaks $GITLEAKS_VERSION already installed"
else
    echo "session-start: installing gitleaks $GITLEAKS_VERSION (pre-commit tier's secret scan) ..."
    TMP=$(mktemp -d)
    curl -sSL "https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz" \
        | tar xz -C "$TMP" gitleaks
    mkdir -p "$HOME/.local/bin"
    install -m 0755 "$TMP/gitleaks" "$HOME/.local/bin/gitleaks"
    rm -rf "$TMP"
    echo "session-start: $(gitleaks version) ready"
fi

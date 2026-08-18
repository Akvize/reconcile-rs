#!/usr/bin/env bash
# Runs AGENTS.md §3's list, narrowed to what the current change can actually affect: the
# always-tier commands (cheap, no compile -- same three as `main.yml`'s `repo-gates` job)
# unconditionally, then the `rust`/`deps`-tier commands only if the diff against $BASE_REF touches
# that category (`./scripts/lib-changed-paths.sh`, the same categories `main.yml`'s `changes` job
# and `./pre-push` use).
#
# This is what CLAUDE.md points agents at instead of "run every command in AGENTS.md §3 by hand":
# deciding relevance by eye is exactly the mistake AGENTS.md §10 exists to rule out -- a rule a
# human (or an agent) must remember and apply belongs in a script instead.
#
# Override the base with $1; defaults to origin/main, the same convention
# ./scripts/check-mutation-gate.sh uses. Diffs against the working tree, not just HEAD (see
# lib-changed-paths.sh's changed_paths), so an uncommitted edit is never invisible to this.
set -Eeuo pipefail

BASE_REF="${1:-origin/main}"

GIT_ROOT=$(git rev-parse --show-toplevel)
cd "$GIT_ROOT"
source ./scripts/lib-changed-paths.sh

export RUSTFLAGS=-Dwarnings RUSTDOCFLAGS=-Dwarnings

# Never skip on an unresolvable base -- an unfetched origin/main is a local environment problem,
# not evidence the change cannot affect anything.
if git rev-parse --verify -q "$BASE_REF" >/dev/null; then
    RUST=0
    affects_rust "$BASE_REF" && RUST=1
    DEPS=0
    affects_deps "$BASE_REF" && DEPS=1
else
    echo "run-affected-checks: '$BASE_REF' does not resolve -- running everything" >&2
    RUST=1
    DEPS=1
fi

run() {
    echo "+ $*"
    "$@"
}
skip() {
    echo "run-affected-checks: skipping $1 -- no $2-affecting change against ${BASE_REF}"
}

run ./scripts/check-doc-budget.sh
run ./scripts/check-domain-purity.sh
run ./scripts/check-doc-structure.sh

if [ "$RUST" -eq 1 ]; then
    run cargo fmt --check
    run cargo clippy --workspace --features internal-testing --all-targets
    run cargo clippy --workspace --all-features --all-targets
    run cargo build --workspace
    run cargo nextest run --workspace --features internal-testing --retries 4 --flaky-result fail
    run cargo nextest run --workspace --all-features --retries 4 --flaky-result fail
    run cargo test --doc --workspace --features internal-testing
    run cargo bench --no-run --features internal-testing
    run cargo doc --workspace
    run cargo doc --workspace --all-features
    run cargo package --workspace --allow-dirty
    run ./scripts/check-public-api.sh
else
    skip "fmt / clippy / build / nextest / doctest / bench / doc / package / public-api" rust
fi

if [ "$DEPS" -eq 1 ]; then
    run cargo deny check
else
    skip "cargo deny check" deps
fi

#!/usr/bin/env bash
# Does a change's *tests* actually detect faults in the code the change touched?
#
# Coverage cannot answer this. Meta's ACH study found 49% of fault-detecting
# generated tests added zero line coverage (arXiv:2501.12862) — a coverage-delta
# gate would have discarded half of the tests that mattered. Mutation does answer
# it: inject a plausible fault into the changed lines, and require the suite to
# fail.
#
# Scope is the diff, not the workspace: a full sweep is ~1400 mutants and hours.
# `--in-diff` on a typical PR is 5-20 mutants, 2-8 minutes.
#
# HERMETICITY IS A PRECONDITION, NOT A NICETY. cargo-mutants assumes the suite is
# deterministic; tests/proptest_*.rs draws a fresh random seed per run unless
# PROPTEST_RNG_SEED is set, which makes the same mutant MISSED on one run and
# caught on the next. Pin it, or this gate reports noise.
set -Eeuo pipefail

: "${PROPTEST_RNG_SEED:=20260817}"
export PROPTEST_RNG_SEED

BASE_REF="${1:-origin/main}"

GIT_ROOT=$(git rev-parse --show-toplevel)
cd "$GIT_ROOT"
export CARGO_TARGET_DIR="${GIT_ROOT}/target"

DIFF=$(mktemp); trap 'rm -f "$DIFF"' EXIT
git diff "${BASE_REF}..." -- '*.rs' >"$DIFF"

if [ ! -s "$DIFF" ]; then
    echo "check-mutation-gate: no Rust changes against ${BASE_REF}, nothing to check"
    exit 0
fi

echo "check-mutation-gate: mutating lines changed against ${BASE_REF}"
echo "                     PROPTEST_RNG_SEED=${PROPTEST_RNG_SEED}"

# --workspace is load-bearing: without it cargo-mutants scopes to the invoking package
# only (the root `reconcile` crate), so a diff touching rsos/rbsr/lww-register/gossip
# would silently match zero mutants there instead of gating them. Verified empirically —
# `cargo mutants --in-diff` on a diff to lww-register/src/clock.rs reports "No mutants to
# filter" without --workspace, and finds the mutant with it.
cargo mutants --workspace --no-shuffle -vV --in-diff "$DIFF" --timeout 300

# cargo-mutants exits non-zero when mutants survive, so reaching here means the
# changed lines are covered by fault-detecting tests.
echo "check-mutation-gate: no surviving mutants in the diff"

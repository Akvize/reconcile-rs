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
# Optional "i/n" (0-based, matching mutants.yml's `nightly` job's own convention) to run only one
# shard of the in-diff mutant set -- see the --shard block below for why pr-diff needs this.
SHARD="${2:-}"

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
echo "                     PROPTEST_RNG_SEED=${PROPTEST_RNG_SEED}${SHARD:+, shard=$SHARD}"

# cargo-mutants tests mutants one at a time by default. This gate ran --jobs 3 for a
# while (parallel build+test copies), capped and derived from nproc the same way this
# comment used to describe -- but #438 tracked down a *second* class of concurrency bug
# beyond the copy_target race below: at --jobs > 1, a mutant that is reliably CAUGHT in
# isolation intermittently reports MISSED under load, with no source change between runs
# and no consistent single culprit mutant (four occurrences across two PRs, at least two
# distinct mutants, all in plain synchronous code with no wall-clock dependency -- so not
# the timeout-sensitive-proptest class #425 already documented). Isolating each with
# `--jobs 1` (same flags otherwise, including --copy-target=false below) caught every one
# of them, every time; --jobs 1 has not produced a false MISSED in any of this
# investigation's runs. No upstream fix exists yet (tracked in #438) and root-causing the
# exact mechanism would need reproducing a resource-contention race that costs 15-20
# minutes per attempt -- so until #438 lands one, this gate trades speed for the one
# property that actually matters for an automated gate: a MISSED here must mean a real
# gap, not "re-run and see." Fixed at 1, not derived from nproc -- a bigger box does not
# make the race safer, only faster to hit.
JOBS=1

# --copy-target=false overrides .cargo/mutants.toml's `copy_target = true` (there to
# reuse the warm target/ dir for a fast *sequential* run). Verified empirically: with
# --jobs > 1, each job's startup copy of the shared target/ races the other jobs'
# concurrent build activity in it (rustc still touching target/debug/{deps,incremental}
# after "the build" returns) and reliably crashes with "Worker thread failed: ... IO
# error ... No such file or directory" -- reproduced on every run, --jobs 2 and 3 both.
# Moot now that JOBS is fixed at 1 (no concurrent copy to race), but kept as the safe
# default in case --jobs is ever raised again.
#
# --workspace is load-bearing: without it cargo-mutants scopes to the invoking package
# only (the root `reconcile` crate), so a diff touching rsos/rbsr/lww-register/gossip
# would silently match zero mutants there instead of gating them. Verified empirically —
# `cargo mutants --in-diff` on a diff to lww-register/src/clock.rs reports "No mutants to
# filter" without --workspace, and finds the mutant there.
#
# cargo-mutants' own exit code is non-zero for *any* mutant that isn't caught or unviable --
# that includes TIMEOUT, not just MISSED. .cargo/mutants.toml already documents the intended
# policy ("a mutant that breaks convergence can hang rather than fail... timeouts count as
# caught") but that comment only describes what a human reading the nightly sweep should
# conclude; nothing enforced it here. #427's fingerprint_tree_map split surfaced the gap: a
# handful of `i += 1`-style loop counters mutated to `i *= 1` (starting from `i == 0`) hang
# forever by construction -- no test, however thorough, can turn a genuine infinite loop into
# a finite assertion, so treating a TIMEOUT exactly like a MISSED here would make this gate
# permanently unpassable for that code shape. Score on `missed` specifically instead of the
# raw exit code, so a hang still counts as a detected fault (matching the mutants.toml
# comment) while an actual survivor still fails the gate.
#
# SHARD_ARGS: a large mechanical split (#427/#452) can put 200+ mutants in one diff -- git diff
# shows moved code as changed regardless of whether any logic in it actually did (AGENTS.md §10's
# "a rule enforced by eye" problem, here applied to cargo-mutants' own scope). At --jobs 1 (the
# only setting #438 has verified doesn't produce false MISSED under load) that serialized into a
# 2+ hour run, twice cut off mid-run by the CI runner with zero mutants missed either time --
# wasted compute, not a real gate failure. Splitting the *same* mutant set across parallel shards
# (mutants.yml's `pr-diff` matrix) keeps each shard's wall-clock low without raising --jobs (so
# #438's risk stays closed) and without paying for more total CPU-time than one successful serial
# run would have -- unlike the timeout-minutes bumps this replaced, which paid for repeated
# failed attempts at the same serial run. Empty on a normal PR (5-20 mutants): one shard gets
# everything, sharding is a no-op.
SHARD_ARGS=()
if [ -n "$SHARD" ]; then
    SHARD_ARGS=(--shard "$SHARD" --sharding round-robin)
fi

set +e
cargo mutants --workspace --no-shuffle -vV --in-diff "$DIFF" --timeout 300 --jobs "$JOBS" --copy-target=false "${SHARD_ARGS[@]}"
mutants_status=$?
set -e

if [ ! -f mutants.out/outcomes.json ]; then
    echo "check-mutation-gate: cargo-mutants produced no mutants.out/outcomes.json (exit $mutants_status)" >&2
    exit "${mutants_status:-1}"
fi

missed=$(jq '.missed' mutants.out/outcomes.json)
if [ "$missed" -gt 0 ]; then
    echo "check-mutation-gate: $missed mutant(s) survived (missed) in the diff" >&2
    exit 1
fi

if [ "$mutants_status" -ne 0 ]; then
    echo "check-mutation-gate: cargo-mutants exited $mutants_status with 0 missed (e.g. a timeout) --" \
        "treating as pass, per the policy above."
fi

echo "check-mutation-gate: no surviving mutants in the diff"

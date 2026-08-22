Every command in AGENTS.md §3's list is owned by a gate — `pre-commit`, `pre-push`, or CI-only,
triggered by `git push` via `main.yml`/`mutants.yml` (AGENTS.md §3's tier table). None of them are
yours to hand-run to verify, preview, or double-check a change before committing or pushing: the
gate's result on the materialized tree (the index, then the pushed commit) is the authoritative
one, not a local rerun on a possibly-different working tree.

This applies hardest to CI-only checks (`cargo doc`, `cargo package`, `cargo deny check`,
`./scripts/check-public-api.sh`, `cargo semver-checks`, coverage, `test-mac-hmac`'s variant,
`./scripts/check-mutant-count.sh`): there is no local hook for these, so "just run it first" means
installing extra toolchains (`nightly`, `cargo-public-api`, `cargo-llvm-cov`, ...) and reproducing
minutes of CI by hand for a result the push you're about to make will get anyway. Don't. Push,
then read the actual job's output (or the failure this session is told about) — that failure, not
a local replay, is what tells you whether the change is right.

The one legitimate local use of a CI-only check's tool is *authoring* a change it requires, not
verifying one: running `./scripts/check-public-api.sh --bless` to regenerate a stale snapshot, or
similarly regenerating a lockfile, is producing the fix itself. Reading that tool's plain (non
`--bless`) output first to preview the diff is the verification pass this rule is about — skip it,
bless, and let CI confirm.

`.claude/rules/tests.md` states the same rule for `pre-push`'s `--all-features` nextest run and
`mutants.yml`'s mutation gate specifically, including why hand-running the mutation gate against an
arbitrary base ref would score against the wrong diff even if you did.

---
description: Writing or modifying tests
globs: ["tests/**/*.rs", "**/tests/**/*.rs", "**/src/**/tests.rs"]
---

A test that passes is not evidence the test is worth keeping. The gate is whether
it *detects a fault*, which is what `./scripts/check-mutation-gate.sh` measures.

Both checks are already gated (AGENTS.md §3's tier table) — `pre-push` runs
`--all-features` nextest, and `mutants.yml`'s `pr-diff` job runs the mutation gate on
every PR, required via `mutants-success`. Never hand-run either yourself: a check a
gate already owns the result of is not yours to replay, and hand-running the mutation
gate against an arbitrary base ref would score against the wrong diff anyway. Push and
let the gate report; if it fails, that failure — not a local rerun — is what tells you
whether a test detects a fault.

Assertion rules, in order of preference:
- Assert a *property* (round-trip, idempotence, associativity, ordering), not a
  literal the implementation happens to produce today.
- Never add a snapshot/golden assertion you have not read and justified line by
  line. An accepted-but-unreviewed snapshot encodes current behaviour as correct.
- Never write a test whose only assertion is that a call did not panic.

Determinism is mandatory: proptest draws a fresh seed per run unless
`PROPTEST_RNG_SEED` is set, and non-deterministic tests make mutation results
meaningless. Do not add a test that binds a fixed port.

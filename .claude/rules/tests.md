---
description: Writing or modifying tests
globs: ["tests/**/*.rs", "**/tests/**/*.rs", "**/src/**/tests.rs"]
---

A test that passes is not evidence the test is worth keeping. The gate is whether
it *detects a fault*, which is what `./scripts/check-mutation-gate.sh` measures.

Before proposing new tests as done:
1. `cargo nextest run --workspace --all-features --retries 4 --flaky-result fail`
   — a test that only passes on retry is a defect, not a pass.
2. `./scripts/check-mutation-gate.sh` — the changed lines must have no surviving
   mutants. If a mutant survives, the test asserts the code ran, not that it is
   correct.

Assertion rules, in order of preference:
- Assert a *property* (round-trip, idempotence, associativity, ordering), not a
  literal the implementation happens to produce today.
- Never add a snapshot/golden assertion you have not read and justified line by
  line. An accepted-but-unreviewed snapshot encodes current behaviour as correct.
- Never write a test whose only assertion is that a call did not panic.

Determinism is mandatory: proptest draws a fresh seed per run unless
`PROPTEST_RNG_SEED` is set, and non-deterministic tests make mutation results
meaningless. Do not add a test that binds a fixed port.

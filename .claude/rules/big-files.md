---
description: Reading or editing src/replica.rs
globs: ["src/replica.rs"]
---

1494 lines of production code (its inline `#[cfg(test)]` module moved to
`src/replica/tests.rs` — issue #402 phase 1). `src/replicated_map.rs` went through the same
phase-1 move and then a phase-2 split by concern (issue #405) — it's now 238 lines plus seven
sibling files under `src/replicated_map/` and no longer needs this rule. `replica.rs` still
does: reading it in full costs more context than most whole sessions need, and its own
phase-2 split is tracked separately (its reconciliation-round loop resists the same clean
per-concern separation `replicated_map.rs` had).

- Never `Read` this file without an offset/limit range.
- Orient first: `rg -n '^\s*(pub )?(fn|impl|struct|enum|mod) ' src/replica.rs`
  gives the shape in ~90 lines instead of 1494.
- To find a symbol's definition, `rg -n 'fn <name>'` then read ±40 lines.
- If the task genuinely needs whole-file comprehension, say so and stop: that is a
  signal the file should be split, not that the context should be spent.

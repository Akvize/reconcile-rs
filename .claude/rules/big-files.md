---
description: Reading or editing src/replicated_map.rs or src/replica.rs
globs: ["src/replicated_map.rs", "src/replica.rs"]
---

These two files are 1596 and 1494 lines of production code (their inline `#[cfg(test)]`
modules moved to `src/replicated_map/tests.rs` / `src/replica/tests.rs` —
issue #402 phase 1). Still large: together they're ~13% of the repo, and reading either in
full costs more context than most whole sessions need.

- Never `Read` one of these files without an offset/limit range.
- Orient first: `rg -n '^\s*(pub )?(fn|impl|struct|enum|mod) ' src/replica.rs`
  gives the shape in ~90 lines instead of 1494.
- To find a symbol's definition, `rg -n 'fn <name>'` then read ±40 lines.
- If the task genuinely needs whole-file comprehension, say so and stop: that is a
  signal the file should be split, not that the context should be spent.

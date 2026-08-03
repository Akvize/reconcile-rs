# Documentation

Start at the [project README](../README.md) for what the crate is and how to use it. These four
answer different questions, and each one owns its answer: if two disagree, the one that owns the
question wins.

| | Question | Kind |
|---|---|---|
| [`CONTRACT.md`](./CONTRACT.md) | What do we promise, what do we ask of you, what may we change? | **Normative.** Wins over everything else here |
| [`ARCHITECTURE.md`](./ARCHITECTURE.md) | How is it built? Ports, invariants, migration, and the decision ledger (D1–D12) | Design |
| [`PROGRESS.md`](./PROGRESS.md) | Where does it stand? Findings, maturity, roadmap | Living, changes as work lands |
| [`SOTA.md`](./SOTA.md) | Where does it sit in the field? Competitors and literature | Durable, moves slowly |
| [`GLOSSARY.md`](./GLOSSARY.md) | What does that word mean here? | Project vocabulary; `SOTA.md` §3 covers the literature |

Two rules keep them from drifting: nothing is stated in two places, and `PROGRESS.md` is the only
one that carries status.

Root-level docs stay at the root because GitHub, crates.io or Cargo give them a specific place:
[`README.md`](../README.md), [`CHANGELOG.md`](../CHANGELOG.md),
[`CONTRIBUTING.md`](../CONTRIBUTING.md), and the licence files.

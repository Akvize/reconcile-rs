# AGENTS.md

An embedded, eventually-consistent replicated map. Reads are local; replicas converge over UDP by
range-based set reconciliation. Design docs live in [`docs/`](docs/).

Keep this file short. It is prepended to every session, so every line here is a line not spent on
the task. State only what you cannot infer from the code.

## Before you change anything

| Touching | Read first |
|---|---|
| a public signature, a wire byte, a disk byte | [`docs/CONTRACT.md`](docs/CONTRACT.md) — it is normative, and it lists what callers are entitled to rely on |
| anything in the reconciliation path | [`docs/ARCHITECTURE.md` §5](docs/ARCHITECTURE.md#5-invariants) — nine invariants, each load-bearing. Breaking one usually shows up as silent non-convergence, not a failing test |
| a settled question (pluggable merge, larger-than-RAM, exposing the tree, a strategy knob) | [`docs/ARCHITECTURE.md` §7](docs/ARCHITECTURE.md#7-decision-ledger) — D1–D12, with what would overturn each. Don't relitigate; if you think one is wrong, say why against its stated reasoning |

## The gate

CI runs more than `cargo test`. Run all of it before pushing:

```sh
cargo fmt --check
cargo clippy --all --all-features    # CI denies warnings
cargo test --all --all-features
cargo doc --all --all-features       # under -D warnings
cargo bench --no-run
cargo publish --allow-dirty --dry-run
```

Two traps:

- **`cargo doc` catches what build and clippy do not** — a broken intra-doc link, or a link from a
  public item to a private one, fails CI and nothing else.
- **An env `RUSTFLAGS`/`RUSTDOCFLAGS` overrides `.cargo/config.toml`, it does not merge.** If you set
  either, re-add `--cfg reconcile_internal_testing` or the test-only seam stops compiling.

`mac-blake3` and `mac-hmac` are mutually exclusive and `--all-features` picks blake3, so the HMAC
path only gets exercised by `cargo test --no-default-features --features mac-hmac`.

## Comments

Say what the code cannot. Everything else is noise that goes stale.

- **Point, don't duplicate.** If `docs/CONTRACT.md` or `docs/ARCHITECTURE.md` says it, link the
  section. Don't paraphrase it, and don't re-derive its reasoning.
- **Don't restate the signature.** `/// Returns the number of peers.` on `fn peer_count() -> usize`
  earns nothing.
- **Do write the non-obvious**: an invariant the compiler can't hold, a footgun, why the obvious
  approach was rejected. One or two sentences, at the definition site.
- Doc examples are compiled by `cargo test --doc`. Prefer one that runs over a paragraph.

## Docs

- **One statement, one home.** If two files say it, they will disagree later.
- [`docs/PROGRESS.md`](docs/PROGRESS.md) is the only file carrying status. Nothing else says
  "currently", "not yet" or "planned".
- `docs/CONTRACT.md` promises · `docs/ARCHITECTURE.md` designs · `docs/PROGRESS.md` tracks ·
  `docs/SOTA.md` positions · `docs/GLOSSARY.md` defines. Put a change in exactly one.
- Prefer a diagram or a table to a paragraph. Mermaid renders on GitHub.

## Traps that are not in the code

- **`./pre-commit` is a repo script, not the Python framework.** There is no
  `.pre-commit-config.yaml`; `pre-commit run --all-files` does nothing. `.devcontainer/init.sh`
  links it into `.git/hooks/`. It runs `fmt` and `clippy --all` only, so passing it is weaker than
  the gate above.
- **shellcheck and markdownlint are editor aids, not gates.** Both ship in the dev image, neither
  runs in CI, and the existing docs fail their defaults heavily. Don't "fix" docs to satisfy them.
  Do keep new shell scripts shellcheck-clean.
- **Anything new at the top level ships to crates.io unless `Cargo.toml`'s `exclude` says
  otherwise**, and publishing is irreversible. Check with
  `cargo package --list --allow-dirty | grep <new-path>`.

## Conventions

- **Commits**: Conventional Commits with an optional scope, as in the log — `feat(clock):`,
  `fix(timeout_wheel):`, `refactor(...)`, `perf(...)`, `chore:`, `ci:`. Work lands through a pull
  request; `main` is not committed to directly.
- `#![forbid(unsafe_code)]`. MSRV 1.85; raising it is a minor version bump.
- Ports are `Clock`, `Transport`, `Codec`, `Persistence`, `Discovery`. Which are consumer-wireable,
  and what an implementation owes, is [`docs/CONTRACT.md` §5](docs/CONTRACT.md#5-extending-it).
- Error *kinds* are load-bearing: `Persistence::load` returning `io::ErrorKind::InvalidData` is
  classified corrupt, anything else transient. Getting it wrong makes a caller retry forever.
- Wire and on-disk breaks are batched into one release (D10). Don't ship one on its own.
- New behaviour needs a test that would fail without it. Convergence bugs need one that drives two
  nodes over `InMemoryNetwork` — deterministic, no sockets.

## Commits and PRs

Explain why, not what; the diff shows what. Lead with the problem. Say what you verified and what
you deliberately left out. If you found a defect while doing something else, say so — that is the
part a reviewer cannot reconstruct.

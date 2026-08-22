# Contributing Guide

Two workflows for a reproducible Rust dev environment — pick the one that fits your tooling. Both
build the same image (`.devcontainer/Dockerfile.dev`) and land you in a shell as user `dev`.

```bash
git clone https://github.com/akvize/reconcile-rs.git && cd reconcile-rs
```

## 🔧 IDE Workflow (Dev Containers)

**Prerequisites:** Docker, and one of VS Code (**Remote – Containers** extension), GitHub
Codespaces, a JetBrains IDE with the Dev Containers plugin, or the Dev Containers CLI
(`npm install -g @devcontainers/cli`).

```bash
make dc-up
```

Runs `devcontainer up`: builds the image, creates/starts the container as user `dev`, runs
`.devcontainer/init.sh create` once (postCreate) and `.devcontainer/init.sh start` on every start
(postStart). Then open the workspace: VS Code → **Remote-Containers: Reopen in Container**;
JetBrains → **Attach to Dev Container**; CLI users land in a shell automatically. Your IDE picks up
the installed LSPs (`rust-analyzer`, `dockerfile-language-server-nodejs`, `taplo`, `marksman`)
without further setup.

To rebuild after a `Dockerfile.dev` change: `make dc-rebuild`.

## 🐳 CLI Workflow (Raw Docker)

**Prerequisites:** Docker Engine, `make`.

```bash
make dev
```

Builds the image, mounts the repo at `/workspace`, runs the same init steps as the IDE workflow, and
drops you into `bash` as user `dev`. Attach your own editor (Neovim, Emacs, …) to the running
container for LSP support — the servers above are installed under `/usr/local/bin`.

To rebuild: `make build`, or `docker build --no-cache -f .devcontainer/Dockerfile.dev -t
reconcile-rs-dev-container .` for a clean build.

## Verify (either workflow)

```bash
whoami                        # dev
git config --global user.name
git config --global user.email
ls -l .git/hooks/pre-commit .git/hooks/pre-push   # both hooks linked
rustc --version
cargo deny --version        # AGENTS.md §3's last line; the only one needing a subcommand
cargo nextest --version     # AGENTS.md §3's test lines run under this
ast-grep --version          # structural search/rewrite, not part of any gate
gitleaks version             # ./pre-commit's secret scan
command -v rust-analyzer dockerfile-language-server-nodejs taplo marksman
```

An image built before `cargo-deny`/`cargo-nextest`/`ast-grep`/`gitleaks` were added to
`Dockerfile.dev` passes every check above except those — rebuild (`make dc-rebuild`, or `make
build`) rather than installing them by hand, so the image and this list stay the same artifact.

## Git hooks

Linked automatically by `init.sh`; link manually with:

```bash
ln -sf ../../pre-commit .git/hooks/pre-commit
ln -sf ../../pre-push .git/hooks/pre-push
```

[`./pre-commit`](./pre-commit) runs before every commit, [`./pre-push`](./pre-push) before every
push. They run tiered subsets of the checks in AGENTS.md §3 rather than all of them, so that
committing costs no compile at all — see AGENTS.md §3 for the tier table and each tier's budget.

## Why the gate looks like this

AGENTS.md §3 states the rules; this section is the evidence behind them, kept here so that file
stays a rulebook.

### `--all-targets` is load-bearing

Without it, clippy lints only lib and bin targets — **tests, benches and examples are never linted
at all**. Not "linted elsewhere": a `clippy::*` lint in `tests/` is invisible to the entire
pipeline, because the jobs that *compile* test code (`cargo test`, `cargo llvm-cov`) run rustc, not
clippy. Measured on this workspace with one planted `clippy::clone_on_copy` in `tests/`:

| command | outcome |
|---|---|
| `cargo clippy --workspace --all-features` | exit 0 — undetected |
| `cargo clippy --workspace --all-features --all-targets` | exit 101 — caught |
| `cargo test --workspace --all-features --no-run` | exit 0 — undetected |

`--all-targets` pulls in the benches, which use the `reconcile_internal_testing` seams (`just_insert`
and friends), so it only works alongside `--cfg reconcile_internal_testing` (AGENTS.md §6). That
pairing is why `./pre-push` sets the `--cfg` for both commands rather than just the one.

### Why §3's list starts with an `export`

CI sets `RUSTFLAGS`/`RUSTDOCFLAGS=-Dwarnings` on the whole job. A contributor who copies the
commands out of that list without it gets warnings where CI gets errors — a green local run followed
by a red pipeline. rustc lints such as `unused_parens` behave exactly that way.

Neither hook exports it, though. A hook run carrying a different fingerprint from every by-hand
`cargo` command would evict its artifacts and rebuild the tree every time, in both directions —
measured here as a 20 s push tier turning into 1 min 54 s. `clippy … -- --deny warnings` already
denies rustc lints on the workspace crates, which is the part that mattered.

### Where each tier stops

Measured on this workspace, four cores, warm `target/`.

- **Tier 1 (commit), 0.4 s.** No check invokes rustc, so committing never waits on a build.
  `cargo clippy --all-targets` used to live here; it drags the 128-crate dev-dependency tree
  (criterion, proptest and friends — against 47 without dev-dependencies) into the path of every
  commit, which is why it moved to tier 2.
- **Tier 2 (push), ~20 s.** Dominated by the test binaries — building them when stale, running them
  either way (`tests/service/` alone spends ~5 s on real sockets and timers) — not by clippy,
  which is ~4 s of it. One feature variant only.
- **Tier 3 (CI), minutes.** The second feature variant, `cargo bench --no-run`, `cargo doc` ×2 and
  `cargo package`: together they roughly double the wall clock to re-check what tier 2 already
  covered for the common case. Doc tests are *not* in that list — plain `cargo test` already runs
  them, which is why §3's separate `cargo test --doc` line is belt-and-braces, not extra coverage.

### A gate never runs on a change it cannot affect

`main.yml`'s `changes` job (and `mutants.yml`'s copy of it) already skip the compile-heavy CI jobs
on a diff that cannot touch their category — that is what the `changes` job's own header comment
in `main.yml` explains. Two things did not follow that logic until now: `./pre-push` ran its tier-2
pair unconditionally regardless of what a push touched, and CLAUDE.md told an agent to run
AGENTS.md §3's full list "not a subset" — overriding, on every agent-driven change, exactly the
category-based skip CI itself already applies to the same commands.

`./scripts/lib-changed-paths.sh` is the fix: one predicate (`affects_rust`/`affects_deps`), sourced
by `./pre-push` and by `./scripts/run-affected-checks.sh` (an optional way to get tier-3 confidence
locally before a push, narrowed the same way). Its categories are hand-copied from `main.yml`'s
`rust`/`deps` filters (bash `case` arms mirroring `dorny/paths-filter`'s globs one for one) rather
than shared as data, for the same reason `main.yml` and AGENTS.md §3's command list are themselves
kept in sync by hand: a YAML `filters:` block and a bash predicate cannot both read one file
without a third tool to parse YAML inside pre-commit's 0.4 s budget.

Both callers fail open, not closed: an unresolvable `origin/main` (not fetched, no such remote)
runs everything rather than silently skipping on a base it cannot compute — the one failure mode a
structural-relevance mechanism must never have, since a false "unaffected" verdict is
indistinguishable from a bug that shipped without the gate that would have caught it.

A filtered `./pre-push` is only as good as `./pre-push` actually running, and on Claude Code's web
sessions it never had: `.git/hooks/pre-commit`/`pre-push` are per-checkout symlinks (AGENTS.md §2),
and a fresh container starts without them, same as it starts without `cargo-deny` (below). An agent
that doesn't know to link them gets neither hook, silently -- `git commit`/`git push` just succeed,
having gated nothing. `.claude/hooks/session-start.sh` now links both at session start, the same
fix in the same place as the `cargo-deny` install it already did: a setup step a docs line asks an
agent to remember is a setup step that eventually gets skipped.

That still left the mandate itself: once the hooks are linked, `git commit`/`git push` already gate
tiers 1–2 and `main.yml` gates tier 3 on push, so telling an agent to also run
`./scripts/run-affected-checks.sh` before declaring work done was the same anti-pattern one layer
up, just no longer path-blind. AGENTS.md §3 now says so directly — nothing on the list needs a
manual run — and the script drops out of the recommended workflow. It stays available for the
actual exception, documented above: tier-3 confidence without a CI round-trip.

### `cargo package` can fail on a stale sibling, and only locally

Reproducible: pull a commit that adds a **public item to a workspace sibling**, run §3's list in a
warm tree, and `cargo package` fails on that item while every other command passes.

```
error[E0432]: unresolved import `lww_register::clock::assert_conformance`
error: failed to verify package tarball
```

`cargo package`'s verify step builds each packaged crate against its packaged siblings, and it can
link a sibling rlib compiled **before** the new item existed. Two things make it confusing:

| | |
|---|---|
| `cargo build` / `cargo test` pass | they resolve the sibling by path, from current source |
| deleting `target/package` does not help | the stale rlib is in `target/debug/deps`, not there |

Confirm and work around it with a throwaway target directory, which forces a clean sibling:

```bash
CARGO_TARGET_DIR=$(mktemp -d) cargo package --workspace --allow-dirty
```

If that passes, the tree is fine and the warm `target/` was the whole story. CI never sees this —
it starts from a cold target — so a local-only `cargo package` failure on a symbol a sibling just
gained is this, not the change under test.

### Why nextest, and why `--flaky-result fail`

nextest runs each test in its own process rather than as a thread in one binary, which is what
makes `--retries` meaningful: a retry re-runs the actual test process, not just the assertion, so a
test relying on FD/socket/thread-pool state left over by an earlier failure in the same process
can't quietly pass on retry the way it might under `cargo test`. `--flaky-result fail` matters
because the default (`pass`) is silent: a test that only passes on retry 3/4 exits 0 either way, so
without this flag a flaky test never shows up in CI at all — it just costs time. `cargo test --doc`
stays a separate §3 line: nextest does not run doctests.

### The mutation gate: why coverage isn't enough

`scripts/check-mutation-gate.sh` (config: `.cargo/mutants.toml`; CI: `.github/workflows/mutants.yml`)
answers a different question than coverage does: not "did the suite execute this line" but "would
the suite catch a plausible bug here." Meta's ACH study found **49%** of fault-detecting generated
tests added zero line coverage (arXiv:2501.12862) — a coverage-delta gate would have discarded half
the tests that mattered.

Hermeticity is a precondition, not a nicety: `tests/proptest_*.rs` draws a fresh random seed per run
unless `PROPTEST_RNG_SEED` is set, which would make the same mutant `MISSED` on one run and caught on
the next. The gate pins it (`20260817`); nothing else in CI does, deliberately — pinning it
everywhere would trade away what property tests are for, exploring new inputs on every run.

Two lanes, not one: `pr-diff` gates every PR on the mutants its own diff introduces
(`--in-diff`, verified to need `--workspace` alongside it — without that flag, cargo-mutants scopes
to the root `reconcile` package and silently skips rsos/rbsr/lww-register/gossip entirely). `nightly`
sweeps the full ~1400-mutant workspace, sharded 8 ways, and is reported as a trend
(`continue-on-error`), not a gate — a whole-repo mutation score is a number to watch, not a wall.

## The one gate no script runs

Anything touching the load-bearing invariants ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5), the
wire/on-disk format, the protocol, crypto, or GC gets an **adversarial review before merge** — no
exceptions. AGENTS.md §10 sends a by-eye rule to CI instead; this one stays by eye because it judges
whether a change is *correct*, which no command decides.

### SOTA-alignment drift

A rustdoc citing `SOTA.md §N.M` splits into a checkable half and an unavailable one. That the
section still exists is `check-doc-structure.sh`'s part 5 — a `git grep`-shaped fact, gated the
same way a Markdown link is. That the rustdoc's *characterization* still matches what the cited
section currently says is not: it is the same kind of judgment call as the paragraph above, and a
regex heuristic over free prose has already been measured to fail here (`check-doc-issue-claims.sh`'s
header: a looser matcher on a *simpler* claim — an issue's open/closed state — produced three
false positives out of three). It runs instead as a scheduled, non-blocking review outside CI:
real drift becomes a `C-bug` issue with the citing file/line and the mismatch; no drift, no output.

## Code coverage

See [`README.md`](./README.md) "Testing and coverage" for the `cargo-llvm-cov` commands and the
project-coverage gate in `codecov.yml`. It does not contradict "why coverage isn't enough" above:
that section is about trusting coverage to answer "would the suite catch a fault," which it
doesn't; the gate only answers "is this code exercised at all," a floor rather than a
fault-detection proxy — the two coexist because they measure different things. Two extras for
local iteration: `--hide-instantiations --text` for a detailed missed-lines report, and the
`report` sub-command to reuse a previous run's results instead of re-running the tests.

## Code quality trend (complexity, duplication)

`code-quality` in `main.yml` runs `./scripts/report-code-quality.sh` on every Rust-affecting PR and
posts its output to the job summary — cognitive complexity per function
([mozilla/rust-code-analysis](https://github.com/mozilla/rust-code-analysis)) and cross-file
duplication ([jscpd](https://github.com/kucherenko/jscpd)). The numbers themselves are still not a
gate: unlike every check in AGENTS.md §3, "should this function be simpler" has no single right
threshold for a script to enforce, so they stay a trend to watch, not a build-failing one. The job
itself is in `ci-success`'s `needs:` (main.yml) as of #507, though: its own pass/fail (an
install failure, an unreadable source file) was 100% stable across every measured push, so gating
on that costs nothing and closes the gap where a broken job silently never blocked a merge. Run it
locally with `cargo install rust-code-analysis-cli --locked` and `npm install -g jscpd` on `PATH` —
neither ships in `Dockerfile.dev`, since nothing else here needs them outside this one report.

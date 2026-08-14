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
command -v rust-analyzer dockerfile-language-server-nodejs taplo marksman
```

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

`--all-targets` pulls in the benches, which use the `internal-testing` seams (`just_insert` and
friends), so it only works alongside `--features internal-testing`. That pairing is why `./pre-push`
carries both flags rather than just the one.

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
  either way (`tests/service.rs` alone spends ~5 s on real sockets and timers) — not by clippy,
  which is ~4 s of it. One feature variant only.
- **Tier 3 (CI), minutes.** The second feature variant, `cargo bench --no-run`, `cargo doc` ×2 and
  `cargo package`: together they roughly double the wall clock to re-check what tier 2 already
  covered for the common case. Doc tests are *not* in that list — plain `cargo test` already runs
  them, which is why §3's separate `cargo test --doc` line is belt-and-braces, not extra coverage.

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

See [`README.md`](./README.md) "Testing and coverage" for the `cargo-llvm-cov` commands. Two extras
for local iteration: `--hide-instantiations --text` for a detailed missed-lines report, and the
`report` sub-command to reuse a previous run's results instead of re-running the tests.

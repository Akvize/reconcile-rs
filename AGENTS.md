# AGENTS.md

Source of truth for any human or AI agent working here, across tools (Claude Code, Codex, Cursor,
...). Tool-specific files (`CLAUDE.md`) import this file and add nothing that contradicts it.

This file states rules, not rationale. For rationale and worked examples, follow the links —
duplicating them here is exactly the rot this file is meant to avoid (§10).

## 1. Map

Five-crate Cargo workspace, dependency order `rsos → rbsr → lww-register`, `gossip` (independent
sibling) `→ reconcile` (facade). See [`ARCHITECTURE.md`](./ARCHITECTURE.md) §2 for the module table
and diagram. Edition 2021, no MSRV pin.

Read first, don't duplicate: [`README.md`](./README.md) (usage/API/security/deployment),
[`ARCHITECTURE.md`](./ARCHITECTURE.md) (module map, ports & adapters, invariants),
[`PROGRESS.md`](./PROGRESS.md) (live correctness/security/publish status),
[`SOTA.md`](./SOTA.md) (durable positioning/glossary/bibliography).

## 2. Environment

Plain `cargo` + a Rust toolchain is enough. [`CONTRIBUTING.md`](./CONTRIBUTING.md) documents a Dev
Container / Docker setup. Link both git hooks once (§3.2):

```bash
ln -sf ../../pre-commit .git/hooks/pre-commit
ln -sf ../../pre-push .git/hooks/pre-push
```

## 3. Build, lint, test — run all before declaring work done

In CI's order (`.github/workflows/main.yml`):

```bash
export RUSTFLAGS=-Dwarnings RUSTDOCFLAGS=-Dwarnings          # what CI sets; without it a lint
                                                             # is a warning locally and an error
                                                             # in CI — run the list as CI runs it
cargo fmt --check
./scripts/check-domain-purity.sh                             # hexagonal boundary, §9
cargo clippy --workspace --features internal-testing --all-targets
cargo clippy --workspace --all-features --all-targets        # --all-targets is load-bearing, §3.1
cargo build --workspace
cargo test --workspace --features internal-testing
cargo test --workspace --all-features
cargo test --doc --workspace --features internal-testing
cargo bench --no-run --features internal-testing              # benches must compile
cargo doc --workspace                                         # both matter: an intra-doc link to a
cargo doc --workspace --all-features                          # feature-gated item dangles in only one
cargo package --workspace --allow-dirty                       # release packaging, §11
```

`--workspace`, never `--all`. This list is what CI runs and what "done" means; the two git hooks run
tiered *subsets* of it and deliberately do not reproduce it (§3.2). `main.yml` and this list are kept
in sync by hand — change one, change both.

### 3.1 Why `--all-targets`, and why the `export`

Both flags exist because a green local run used to be able to precede a red CI run.

**`--all-targets`.** Without it, clippy lints only lib and bin targets — **tests, benches and
examples are never linted at all**. Not "linted elsewhere": a `clippy::*` lint in `tests/` is
invisible to the entire pipeline, because the jobs that *compile* test code (`cargo test`,
`cargo llvm-cov`) run rustc, not clippy. Measured on this workspace with one planted
`clippy::clone_on_copy` in `tests/`:

| command | outcome |
|---|---|
| `cargo clippy --workspace --all-features` | exit 0 — undetected |
| `cargo clippy --workspace --all-features --all-targets` | exit 101 — caught |
| `cargo test --workspace --all-features --no-run` | exit 0 — undetected |

`--all-targets` pulls in the benches, which use the `internal-testing` seams (`just_insert` and
friends), so it only works alongside `--features internal-testing` — that pairing is why
`./pre-push` carries both flags rather than just the one.

**The `export`.** CI sets `RUSTFLAGS`/`RUSTDOCFLAGS=-Dwarnings` on the whole job. A contributor who
copies the commands out of this list without it gets warnings where CI gets errors, which is a green
local run followed by a red pipeline — rustc lints such as `unused_parens` behave exactly that way.

### 3.2 Three tiers: commit, push, CI

CI runs the §3 list on every push, so the hooks do not have to reproduce it — they have to catch the
cheap failures early without making the inner loop unpleasant. Each tier gets a time budget, and a
check belongs in the earliest tier it fits:

| tier | what runs | cost |
|---|---|---|
| [`./pre-commit`](./pre-commit) | `cargo fmt --check`, `./scripts/check-domain-purity.sh` | 0.3 s |
| [`./pre-push`](./pre-push) | `cargo clippy --workspace --features internal-testing --all-targets -- --deny warnings`, `cargo test --workspace --features internal-testing` | ~20 s |
| [`main.yml`](./.github/workflows/main.yml) | the whole §3 list | minutes |

Measured on this workspace, four cores, warm `target/`. The push tier is dominated by the test
binaries — building them when they are stale, and running them either way (`tests/service.rs` alone
spends ~5 s on real sockets and timers) — not by clippy, which is ~4 s of it.

What follows from the split, in the order it tends to surprise people:

- **A commit may be lint-dirty; a push should not be.** That is the intended trade — `git commit` is
  a save point, `git push` is a publication. Neither tier-1 check invokes rustc, so committing never
  waits on a build.
- **Tier 2 runs one feature variant, not both.** The second variant, `cargo bench --no-run`,
  `cargo doc` ×2 and `cargo package` stay in CI: they roughly double the wall clock to re-check what
  tier 2 has already covered for the common case. Doc tests are *not* in that list — plain
  `cargo test` already runs them, which is why the §3 list's separate `cargo test --doc` line is
  belt-and-braces rather than extra coverage.
- **Neither hook exports `RUSTFLAGS`.** §3.1's export is for a human running the list by hand. In a
  hook it would give every hook run a different fingerprint from every `cargo` command run by hand,
  so each would evict the other's artifacts and both would rebuild a 128-crate tree (47 without dev-
  dependencies) every time. `clippy … -- --deny warnings` already denies rustc lints on the
  workspace crates, which is the part §3.1 was about.
- **Both hooks check a materialized tree, not the working directory** — `pre-commit` the index,
  `pre-push` the commit being pushed. What is recorded or published is what has to be green,
  whatever is half-finished on disk.
- **`git push --no-verify` skips tier 2 on purpose.** Pushing a work-in-progress branch for someone
  to look at is legitimate, and CI remains the authority either way.

Adding a check to a hook means checking it against that tier's budget first. If it does not fit, it
belongs in CI only — that is not a lesser outcome, it is the design.

## 4. Type safety and domain modeling

Strong-type every wire/domain entity: a newtype per concept (never a bare `u64`/`[u8; N]`/`String`),
type-owned validation (parsing and "is this acceptable" live on the type, not the caller), and
construction of an invalid instance structurally impossible. A parameter that appears only in a
constructed result, never in a comparison or computation, belongs to the caller, not the callee.
Worked examples and the reasoning behind them: [`ARCHITECTURE.md`](./ARCHITECTURE.md) §4
(`Timestamp`/`Hlc`/`AdmittedTime`) and `gossip/src/auth.rs`/`replay.rs` (`Payload`, `Seq`, `Stamp`).

## 5. Code style

- `#![forbid(unsafe_code)]` on every crate root — no `unsafe`, ever, compile error not a lint. Carry
  it onto any new crate root.
- `cargo fmt` and `cargo clippy --deny warnings` (§3) are gated, not suggestions.
- §4's strong-typing convention is not yet mechanically gated — hold the line in review.
- `internal-testing`-gated `crate::testing` is test-only, never reachable from non-test code.

## 6. Feature flags

`mac-blake3` (default) vs `mac-hmac` — exactly one wins, build fails if neither. `zeroize`,
`encryption`, `metrics`, `metrics-prometheus`, `dns-hickory` are opt-in. `internal-testing` exposes
test-only seams (`reconcile::testing`, `rbsr::RangeAggregate::for_testing`) — not public API.

`mac-blake3`/`mac-hmac`/`zeroize`/`encryption`/`dns-hickory` are declared on `gossip` (owns
`auth.rs`/`discovery.rs`) and re-exposed from `reconcile` as unification entries
(`mac-blake3 = ["gossip/mac-blake3"]`); `metrics`/`metrics-prometheus` stay on `reconcile`
(`observability.rs`/`prometheus.rs`); `internal-testing` is declared on both `reconcile` and `rbsr`,
forwarded the same way. `reconcile` depends on `gossip` with `default-features = false`, so its own
`default = ["mac-blake3"]` is the single place the MAC backend gets chosen.

Touching feature-gated code: run **both** clippy/test variants (§3) — CI gates them separately
because feature interactions hide bugs.

## 7. Testing

New behavior needs a test: `tests/*.rs` for public-API-crossing changes, `#[cfg(test)]` for internal
invariants. `tests/proptest_fingerprint_tree_map.rs` / `tests/fuzz_packets.rs` are property/fuzz
oracles for the tree and the wire format — extend these over narrow examples when touching parsing
or `FingerprintTreeMap` invariants. Codecov coverage is `informational: true` — not a merge gate;
don't rely on it as proof.

## 8. Security

- UDP is **unauthenticated by default** — any host reaching the port can forge an update via LWW.
  Production sets a shared cluster key (README "Security model"). Never weaken this
  default-unauthenticated-but-loud-warning posture without updating that section.
- The cluster key is a single shared secret — no per-peer identity, no forward secrecy.
- Never commit a real key/credential — README's examples are placeholders.
- Per-peer replay protection (`gossip/src/replay.rs`) deliberately outlives peer membership — don't
  "clean up" a decommissioned peer's entry, that reopens replay.

## 9. Structure and boundaries

Crate map and the domain-purity invariant it enforces: [`ARCHITECTURE.md`](./ARCHITECTURE.md) §2.
`./scripts/check-domain-purity.sh` (itself well-commented — read it before changing it) gates it in
`./pre-commit` and CI: a manifest check (`rsos`/`rbsr`/`lww-register` may not depend on
`tokio`/`bincode`/`chrono`/`ipnet`/`mio`/`reqwest`/`hyper`/`socket2`/`async-trait`) plus a source grep
over `lww-register/src/*.rs` (catches an infrastructure type reached through an allowed dependency's
re-export). `gossip` deliberately has **no** dependency on `lww-register` — nothing in
transport/auth/replay/discovery knows what an `Entry`/`Timestamp`/`Key` is; if a change seems to need
that edge, it has landed in the wrong crate. Widening either set means updating the script **and**
`ARCHITECTURE.md` §2 together.

Docs that change with code, same PR: `README.md`, `ARCHITECTURE.md` §1–§3, this file.
[`PROGRESS.md`](./PROGRESS.md): living status, update as findings/phases change. `SOTA.md`: durable
reference, not updated for routine changes.

## 10. Commit, PR, and gating conventions

No enforced commit-message/PR-title format — match recent history (`git log --oneline`). Reference
the tracking issue (`#NNN`) where relevant.

The one enforced convention: a rule a human must remember and enforce by eye belongs in
`.github/workflows/main.yml` instead, plus whichever hook tier it fits in (§3.2; §9 is the model).
Prose-only guidelines decay; failing commands don't.

## 11. Publishing

Only `reconcile` is on crates.io today, and that published `0.2.1` predates the workspace split (it
vendors what are now `rsos`/`rbsr`/`lww-register`/`gossip` directly) — the four sibling crates have
never been published. Publishing all five, in dependency order
(`rsos` → `rbsr`,`lww-register` → `gossip` → `reconcile`), is implemented in
`.github/workflows/tags.yml` (read its comments — they are the source of truth for the mechanics:
tag/manifest version check, publish order, idempotent skip-if-published, the local stale-registry-cache
gotcha) and fires on a `v*` tag; never hand-run `cargo publish`. Current status and the version to cut
next are a live decision — see [`PROGRESS.md`](./PROGRESS.md), not this file.

`gossip` publishes as `reconcile-gossip` (name taken); every dependent renames it back
(`gossip = { package = "reconcile-gossip", ... }`) so source everywhere still says `use gossip::…`.
`lww-register`/`reconcile-gossip` are implementation detail with no stability guarantee — on
crates.io only because cargo has no vendoring. Every intra-workspace dependency carries a `version`
alongside its `path` (required for `cargo package`/`publish` to resolve it).

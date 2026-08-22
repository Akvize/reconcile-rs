# AGENTS.md

Source of truth for any human or AI agent working here, across tools (Claude Code, Codex, Cursor,
...). Tool-specific files (`CLAUDE.md`) import this file and add nothing that contradicts it.

This file states rules, not rationale. For rationale and worked examples, follow the links —
duplicating them here is exactly the rot this file is meant to avoid (§10). That is a budget, not a
preference: `./scripts/check-doc-budget.sh` fails if this file and `CLAUDE.md` exceed 200 lines
together, since `CLAUDE.md` imports this one verbatim and a reader gets the sum. The same script
also caps `SOTA.md` on its own, larger budget — durable reference, not read every session, but not
unbounded either.

## 1. Map

Five-crate Cargo workspace, dependency order `rsos → rbsr → lww-register`, `gossip` (independent
sibling) `→ reconcile` (facade). See [`ARCHITECTURE.md`](./ARCHITECTURE.md) §2 for the module table
and diagram. Edition 2021, MSRV 1.85 (`rust-version`, all five manifests, README "MSRV").

Read first, don't duplicate: [`README.md`](./README.md) (usage/API/security/deployment),
[`ARCHITECTURE.md`](./ARCHITECTURE.md) (module map, ports & adapters, invariants, audit history),
[`SOTA.md`](./SOTA.md) (durable positioning/glossary/bibliography). Live correctness/security/
release status is the `v1.0.0` milestone and issue #206, not a file.

## 2. Environment

Plain `cargo` + a Rust toolchain is enough (§3's public-API gate also needs `nightly` + `cargo-public-api`).
[`CONTRIBUTING.md`](./CONTRIBUTING.md) documents a Dev Container / Docker setup. Link both git hooks once (§3):

```bash
ln -sf ../../pre-commit .git/hooks/pre-commit
ln -sf ../../pre-push .git/hooks/pre-push
```

## 3. Build, lint, test — gated automatically, not hand-run

In CI's order (`.github/workflows/main.yml`):

```bash
export RUSTFLAGS=-Dwarnings RUSTDOCFLAGS=-Dwarnings   # what CI sets; without it a lint warns locally, errors in CI
cargo fmt --check
./scripts/check-doc-budget.sh              # AGENTS.md + CLAUDE.md ≤ 200, SOTA.md §1-§2 prose ≤ 700
./scripts/check-domain-purity.sh                             # hexagonal boundary + §2 graph, §9
./scripts/check-doc-structure.sh                             # doc links/anchors/paths, SOTA §4.2
./scripts/check-test-file-naming.sh                # split #[cfg(test)] modules named tests.rs
./scripts/check-file-size.sh                    # prod/test line-count budgets, warn + hard-fail
RUSTFLAGS="$RUSTFLAGS --cfg reconcile_internal_testing" cargo clippy --workspace --all-targets
RUSTFLAGS="$RUSTFLAGS --cfg reconcile_internal_testing" cargo clippy --workspace --all-features --all-targets
cargo build --workspace
RUSTFLAGS="$RUSTFLAGS --cfg reconcile_internal_testing" cargo check --workspace --all-targets --all-features  # pinned to rust-version, CI-only (§3 table)
RUSTFLAGS="$RUSTFLAGS --cfg reconcile_internal_testing" cargo nextest run --workspace --retries 4 --flaky-result fail
RUSTFLAGS="$RUSTFLAGS --cfg reconcile_internal_testing" cargo nextest run --workspace --all-features --retries 4 --flaky-result fail
RUSTFLAGS="$RUSTFLAGS --cfg reconcile_internal_testing" cargo test --doc --workspace  # nextest doesn't run doctests
RUSTFLAGS="$RUSTFLAGS --cfg reconcile_internal_testing" cargo bench --no-run    # benches must compile
cargo doc --workspace                                         # both matter: an intra-doc link to a
cargo doc --workspace --all-features                          # feature-gated item dangles in only one
cargo package --workspace --allow-dirty                       # release packaging, §11
cargo deny check                                              # advisories/licenses/sources, deny.toml
./scripts/check-public-api.sh                                 # public-API snapshot + 0.x-leak gate, §11
./scripts/check-mutant-count.sh   # repo-gates' 6th check; also CI-only, omitted above: test-mac-hmac's clippy/build/nextest trio with `--no-default-features --features mac-hmac` (+ the same `--cfg`, §6), coverage's `cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info` → Codecov (§7), and `cargo semver-checks --workspace` (§11, trend until a release re-baselines it)
```

`--workspace`, never `--all`. This list is what CI runs and what "done" means — gated automatically
in tiers, never by hand, once the hooks are linked (§2); a check belongs in the earliest tier whose
budget it fits, and if it fits none of them it is CI-only by design:

| tier | what runs | cost |
|---|---|---|
| [`./pre-commit`](./pre-commit) | `gitleaks protect --staged`, `cargo fmt --check`, the `./scripts/check-doc-*.sh`/`check-domain-purity.sh`/`check-test-file-naming.sh`/`check-file-size.sh` gates above | 0.4 s |
| [`./pre-push`](./pre-push) | the two `--cfg reconcile_internal_testing` lines above, `clippy` first | ~20 s, skipped per commit with no Rust-affecting change |
| [`main.yml`](./.github/workflows/main.yml) | everything else | minutes |
| [`mutants.yml`](./.github/workflows/mutants.yml) `pr-diff` | `./scripts/check-mutation-gate.sh` — in-diff mutation coverage (`.claude/rules/tests.md`); required via `mutants-success`, same as `ci-success` | 2–8 min |

`git commit` triggers tier 1, `git push` triggers tier 2 and — via `main.yml`/`mutants.yml` — tier 3: never
hand-run any of this to verify a change (`.claude/rules/gated-checks.md`) — it replays a check a gate already
owns. Both hooks check a materialized tree — the index, then the commit being pushed — so what gets recorded or
published is what has to be green, not whatever is half-finished on disk. `git push --no-verify` skips tier 2 on
purpose; CI stays the authority. `main.yml`/this list are kept in sync by hand — change one, change both.

A gate also never runs on a change nothing in it can affect, at any tier:
[`./scripts/lib-changed-paths.sh`](./scripts/lib-changed-paths.sh) categorizes every changed path as
`rust`/`deps`/neither — the categories `main.yml`'s `changes` job, `mutants.yml`'s copy of it, and
`./pre-push` all read, hand-synced with `main.yml`'s filter for the same reason as above.

Why `--all-targets` is load-bearing, why the export exists, and why each tier stops where it does:
[`CONTRIBUTING.md`](./CONTRIBUTING.md) "Why the gate looks like this" — measured, not asserted.

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
- `crate::testing` is `#[cfg(reconcile_internal_testing)]`-gated, test-only, non-test code never reaches it (§6).

## 6. Feature flags

`mac-blake3` (default) vs `mac-hmac` — exactly one wins, build fails if neither. `zeroize`,
`encryption`, `metrics`, `metrics-prometheus`, `dns-hickory` are opt-in. Test-only seams
(`reconcile::testing`, `rbsr::RangeAggregate::for_testing`) are reached only via `--cfg
reconcile_internal_testing` in RUSTFLAGS (#330: not a Cargo feature, unsettable from a dependent's build).

`mac-blake3`/`mac-hmac`/`zeroize`/`encryption`/`dns-hickory` are declared on `gossip` (owns
`auth.rs`/`discovery.rs`) and re-exposed from `reconcile` as unification entries
(`mac-blake3 = ["gossip/mac-blake3"]`); `metrics`/`metrics-prometheus` stay on `reconcile`
(`observability.rs`/`prometheus.rs`). `reconcile` depends on `gossip` with `default-features = false`,
so its own `default = ["mac-blake3"]` is the single place the MAC backend gets chosen.

Touching feature-gated code: run **both** clippy/test variants (§3) — feature interactions hide
bugs; `--all-features` no longer implies the `--cfg` above (#330).

## 7. Testing

New behavior needs a test: `tests/*.rs` for public-API-crossing changes, `#[cfg(test)]` for internal
invariants. `tests/proptest_fingerprint_tree_map/` / `tests/fuzz_packets.rs` are property/fuzz
oracles for the tree and the wire format — extend these over narrow examples when touching parsing
or `FingerprintTreeMap` invariants. Codecov (`codecov.yml`) warns below 100% project coverage and
gates at 90%; per-PR patch coverage stays informational — README "Testing and coverage".

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
`ARCHITECTURE.md` §2 together; the §2 half is gated — part 3 checks its graph against the manifests.

Docs that change with code, same PR: `README.md`, `ARCHITECTURE.md` §1–§3, this file. `SOTA.md`:
durable reference, not updated for routine changes. Live correctness/security/release status lives
in the `v1.0.0` milestone and issue #206 — a GitHub query, not a file to keep in sync by hand.

**Every fact lives in exactly one place; everywhere else links to it.** A restatement is a second
copy that drifts, and the drifted copy is read as true. Hence **prose is the last resort**, in docs
and in code comments alike: prefer a mermaid diagram (`ARCHITECTURE.md` §2/§3 is the model), a table,
a code block, or a link. State the rule, the shape or the evidence — never narrate it. Scope is every
doc **about** this repo, in it or not — issues, PRs, review comments — not only committed files;
`.github/ISSUE_TEMPLATE/`/`pull_request_template.md` carry the shape so it isn't re-derived per post.
The failure mode that recurs in agent-written threads: narrating the process that produced a finding
instead of stating the finding and what it supersedes. Comments belong to humans: an agent never
posts one, but reads what a human wrote there, challenges it when the code says otherwise, and — with
that human in session — folds the outcome into the body, the title or the code. The change is the
reply.

## 10. Commit, PR, and gating conventions

No enforced commit-message/PR-title format — match recent history (`git log --oneline`). Reference
the tracking issue (`#NNN`) where relevant.

The one enforced convention: a rule a human must remember and enforce by eye belongs in
`.github/workflows/main.yml` instead, plus whichever hook tier it fits in (§3; §9 is the model).
Prose-only guidelines decay; failing commands don't.

## 11. Publishing

As of `v0.3.0` (2026-08-19) all five crates are on crates.io: `reconcile`/`rsos`/`lww-register`/
`reconcile-gossip` at `0.3.0`, `rbsr` at `0.1.0` (own line, see Version lines below). `0.2.1` was
the last release before the workspace split (it vendored the four siblings directly) — see
`MIGRATING.md`. Publishing in dependency order (`rsos` → `rbsr`,`lww-register` → `gossip` →
`reconcile`) is implemented in `.github/workflows/tags.yml` (its comments are the source of truth
for the mechanics: tag/manifest version check, publish order, idempotent skip-if-published, the
stale-registry-cache gotcha) and fires on a `v*` tag; never hand-run `cargo publish`. Version to cut
(#204, #206 §5): `0.3.0` first, `1.0.0` from it — the path taken; #410 found no durable citation.

`gossip` publishes as `reconcile-gossip` (name taken); every dependent renames it back
(`gossip = { package = "reconcile-gossip", ... }`) so source everywhere still says `use gossip::…`.
Every intra-workspace dependency carries a `version` alongside its `path` (`cargo package`/`publish` need
it) — the four siblings are on crates.io only because cargo has no vendoring; depend on `reconcile`, not on them.

**Version lines (#308, 2026-08-11).** `rsos`/`lww-register`/`reconcile-gossip` are in `reconcile`'s
public API → majors coupled → `1.0.0` with it, its semver covering the re-exported items only.
`rbsr` is not → stays `0.x` until #289 settles it; promoting later is additive, demoting is not.
`./scripts/check-public-api.sh` (§3) gates a `rbsr` symbol re-entering the public API, mechanically (#311 rules 2/3; rule 1 is a §3 trend until a release re-baselines it).

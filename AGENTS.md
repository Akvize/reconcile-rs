# AGENTS.md

Source of truth for any human or AI agent working here, across tools (Claude Code, Codex, Cursor,
...). Tool-specific files (`CLAUDE.md`) import this file and add nothing that contradicts it.

This file states rules, not rationale. For rationale and worked examples, follow the links —
duplicating them here is exactly the rot this file is meant to avoid (§10). That is a budget, not a
preference: `./scripts/check-doc-budget.sh` fails if this file and `CLAUDE.md` exceed 200 lines
together, since `CLAUDE.md` imports this one verbatim and a reader gets the sum.

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
Container / Docker setup. Link both git hooks once (§3):

```bash
ln -sf ../../pre-commit .git/hooks/pre-commit
ln -sf ../../pre-push .git/hooks/pre-push
```

## 3. Build, lint, test

In CI's order (`.github/workflows/main.yml`) — most of this already runs via hooks (§2), see below:

```bash
export RUSTFLAGS=-Dwarnings RUSTDOCFLAGS=-Dwarnings          # what CI sets; without it a lint
                                                             # is a warning locally and an error
                                                             # in CI — run the list as CI runs it
cargo fmt --check
./scripts/check-doc-budget.sh                                # AGENTS.md + CLAUDE.md ≤ 200 lines
./scripts/check-domain-purity.sh                             # hexagonal boundary + §2 graph, §9
./scripts/check-doc-structure.sh                             # doc links/anchors/paths, SOTA §4.2
cargo clippy --workspace --features internal-testing --all-targets
cargo clippy --workspace --all-features --all-targets        # --all-targets is load-bearing
cargo build --workspace
cargo test --workspace --features internal-testing
cargo test --workspace --all-features
cargo test --doc --workspace --features internal-testing
cargo bench --no-run --features internal-testing              # benches must compile
cargo doc --workspace                                         # both matter: an intra-doc link to a
cargo doc --workspace --all-features                          # feature-gated item dangles in only one
cargo package --workspace --allow-dirty                       # release packaging, §11
cargo deny check                                              # advisories/licenses/sources, deny.toml
```

`--workspace`, never `--all`. The hooks below auto-run tiered *subsets* of this on every commit/push
(§2) — don't replay their commands by hand. What fits neither tier is CI-only by design; run it
locally only when the change plausibly touches it, not reflexively:

| tier | what runs | cost |
|---|---|---|
| [`./pre-commit`](./pre-commit) | `cargo fmt --check`, the three `./scripts/check-doc-*.sh`/`check-domain-purity.sh` gates above | 0.4 s |
| [`./pre-push`](./pre-push) | the two `internal-testing` lines above, `clippy` first | ~20 s |
| [`main.yml`](./.github/workflows/main.yml) | everything above | minutes |

So a commit may be lint-dirty and a push should not be: `git commit` is a save point, `git push` a
publication, and `git push --no-verify` skips tier 2 on purpose. Both hooks check a materialized
tree — the index, then the commit being pushed — because what is recorded or published is what has
to be green, whatever is half-finished on disk. `main.yml` and this list are kept in sync by hand —
change one, change both.

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
`ARCHITECTURE.md` §2 together; the §2 half is gated — part 3 checks its graph against the manifests.

Docs that change with code, same PR: `README.md`, `ARCHITECTURE.md` §1–§3, this file.
[`PROGRESS.md`](./PROGRESS.md): living status, update as findings/phases change. `SOTA.md`: durable
reference, not updated for routine changes.

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
Every intra-workspace dependency carries a `version` alongside its `path` (required for
`cargo package`/`publish` to resolve it). The four siblings are on crates.io only because cargo has
no vendoring — depend on `reconcile`, not on them.

**Version lines (#308, 2026-08-11).** `rsos`/`lww-register`/`reconcile-gossip` are in `reconcile`'s
public API → majors coupled → `1.0.0` with it, its semver covering the re-exported items only.
`rbsr` is not → stays `0.x` until #289 settles it; promoting later is additive, demoting is not.

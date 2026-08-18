# public-api/

Committed `cargo public-api` snapshots, one per workspace crate (`rsos.txt`, `rbsr.txt`,
`lww-register.txt`, `reconcile-gossip.txt`, `reconcile.txt`) — default features, deterministic
output. Same role as `Cargo.lock`: a stale diff against the live render fails the build, so an
unintended public-API change is a CI failure, not a review miss.

Mechanism, rationale, and the second check these files feed (no `rbsr` symbol may appear in
`reconcile`'s API — AGENTS.md §11, [#308](https://github.com/Akvize/reconcile-rs/issues/308)): see
the header of [`../scripts/check-public-api.sh`](../scripts/check-public-api.sh).

**Never hand-edit.** After a deliberate public-API change, regenerate and commit:

```bash
./scripts/check-public-api.sh --bless
```

The diff in that commit *is* the change to review.

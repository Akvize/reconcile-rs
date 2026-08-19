# Security Policy

## Supported versions

Only the latest published `0.x` release of `reconcile` (and its published dependencies `rsos`,
`rbsr`, `lww-register`, `reconcile-gossip`) receives security fixes. There is no long-term-support
branch before `1.0.0` — see the
[`v1.0.0` milestone](https://github.com/Akvize/reconcile-rs/milestone/2) and
[issue #206](https://github.com/Akvize/reconcile-rs/issues/206) for the release plan.

## Reporting a vulnerability

Please report suspected vulnerabilities privately via GitHub's
[private vulnerability reporting](https://github.com/Akvize/reconcile-rs/security/advisories/new)
(repository Security tab → "Report a vulnerability") rather than a public issue.

We aim to acknowledge a report within 5 business days, and will keep you updated as we investigate
and fix it.

## Scope

In scope: memory-safety bugs, authentication/MAC bypass, an unauthenticated node able to corrupt or
exfiltrate data beyond what is already documented below, panics reachable from untrusted network
input, and any behavior contradicting the README's "Security model" section.

Out of scope — documented design choices, not bugs:

- UDP reconciliation is **unauthenticated by default**; a shared cluster key is opt-in and required
  to close this (README "Security model", AGENTS.md §8).
- The cluster key is a single shared secret: no per-peer identity, no forward secrecy (issues #135,
  #136).
- UDP source addresses are spoofable — a property of the transport, not this crate.

See the README's [Security model](README.md#security-model) section for the full threat model.

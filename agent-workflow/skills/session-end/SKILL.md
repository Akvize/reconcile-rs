---
name: session-end
description: Run the end-of-session review gate before a working session closes — scope drift, verification honesty, convention conformance, doc consistency, blast radius and secret sweep, agent retrospective, repo retrospective, and handoff. Use when wrapping up a session, when the user says "wrap up", "end session", "we're done", "debrief", or invokes /session-end.
---

# Session exit review

A session is not finished when the code works. It is finished when the code
works, the docs still tell the truth, the conventions held, nothing irreversible
happened by accident, and **the next session starts better than this one did**.

Eight gates. Each one ends in exactly one of two states:

- **PASS** — with the evidence that makes it a pass (a command that ran, a file
  that was read, a diff that was inspected).
- **FINDING** — with severity and a concrete next action.

"Looks fine" is not a state. If a gate cannot be evaluated, it is a FINDING that
says so. The value of this review is entirely in its willingness to report badly.

## Gather the evidence first

Do this before reasoning about any gate. Never run this review from memory —
memory is precisely what has degraded over a long session.

```
aw diff        # what THIS session changed, against the bootstrap baseline
aw journal     # friction, decisions, anomalies, assumptions recorded as they happened
aw baseline    # where the session started
aw secrets     # credential-shaped strings in added lines
aw deps        # dependency manifest changes
aw conventions # convention sources, lint configs, doc surface
```

If the journal is empty and the session was substantial, that is itself the first
finding: this session flew blind and the retrospective below will be weaker for it.
Say so rather than compensating with invention.

---

## G0 — Scope drift

Compare what was asked against what shipped. Both directions are failures:

- **Narrowing** — something in scope was quietly dropped, deferred, or stubbed.
  This is the more damaging direction, because it is usually invisible in a diff
  and the user believes it was delivered.
- **Widening** — refactors, renames, reformatting, dependency bumps, or "while I
  was in there" fixes that nobody asked for. These inflate review cost and blast
  radius, and they hide the real change.

Reference the `scope` journal entry from bootstrap. If none exists, reconstruct
the goal from the opening request and say that you are doing so.

Every file in `aw diff` should be traceable to the request. List any that are not.

## G1 — Verification honesty

The failure mode this gate exists for: an agent says "this works" when what it
means is "this compiles", or "tests pass" when it ran a subset, or "fixed" when
it never reproduced the bug in the first place.

Build a ledger. For each substantive claim made during the session:

| claim | evidence | verdict |
|---|---|---|
| … | exact command that ran, and its result | VERIFIED / UNVERIFIED |

Rules, applied strictly:

- Evidence is a command that **actually ran in this session** and its real output.
  Not CI config, not "should pass", not a command you intended to run.
- A test suite that was never executed against the final state of the code
  verifies nothing. If code changed after the last run, re-run it now.
- Reproduction counts: a bug fix without a failing-then-passing observation is
  UNVERIFIED, however obvious the fix looks.
- Passing a subset is a subset. Say which subset.

**Report every UNVERIFIED claim explicitly to the user.** Never let one be
silently upgraded by omission. This gate is the single highest-value item in the
review — an unverified claim presented as fact is worse than no claim at all,
because it spends trust that has to be repaid later.

## G2 — Convention conformance

Not "does it lint" — run the linter, but that is the floor.

1. Run the project's actual format/lint/check commands (from `aw verify`) and
   report real output.
2. Compare the shipped code against the *practised* conventions of the files it
   sits beside: error handling, naming, test placement and naming, module
   boundaries, comment density, public-API surface, commit-message style.
3. Where written guidance and practised convention conflict, name the conflict.
   That conflict is a repo finding (G6), not merely a code finding.

New code that is locally idiomatic but repo-alien is a finding even when every
linter is green.

## G3 — Documentation consistency

Three distinct questions, all of which must be asked:

1. **Stale** — does any doc now describe behaviour that changed this session?
   Check the doc surface from `aw conventions`, and grep the docs for the
   identifiers, flags, commands, endpoints, and config keys that this session
   touched or renamed.
2. **Missing** — did this session add behaviour, configuration, or public API
   that no doc mentions? New public surface with no documentation is a finding.
3. **Contradicted** — did this session *discover* that existing docs were already
   wrong, independent of the change? Fixing that is in scope for the review even
   though it predates the session; at minimum it must be reported.

Also check the doc-adjacent artifacts that rot silently: README examples that no
longer run, changelog entries for user-visible changes, generated API docs,
architecture diagrams whose components were renamed, and any doc that pins a
version this session bumped.

## G4 — Blast radius, reversibility, and secrets

What did this session do that is hard or impossible to undo?

- **Published** — commits pushed, branches created, PRs opened, releases tagged,
  packages published, artifacts uploaded, messages sent to external services.
  Anything that left the machine may be cached, indexed, or already read.
- **Destructive** — files or branches deleted, history rewritten, migrations run,
  data mutated, infrastructure changed.
- **Secrets** — run `aw secrets`. Review every hit by hand; the pattern match is
  a prompt to look, not a verdict. Then check the places secrets leak that are
  not source files: commit messages, PR/issue bodies, test fixtures, logs
  committed as artifacts, `.env` files newly tracked, and CI config.
- **Supply chain** — run `aw deps`. For every added or bumped dependency: is it
  necessary, is the version pinned appropriately, is the licence compatible, and
  was it deliberate or a transitive surprise from a lockfile regeneration?

State plainly what cannot be walked back. If something irreversible happened that
the user did not explicitly authorise, that is a P0 finding and it leads the report.

## G5 — Agent retrospective

Read the journal. For each bump — tool failure, wrong assumption, misleading doc,
convention discovered too late, wasted loop — do the analysis that actually
compounds:

**bump → root cause → prevention lane**

Four lanes, and choosing the right one is the whole point:

1. **Human practice** — the user could have prevented this cheaply. Name the
   practice concretely: "state the acceptance criterion before I start", "tell me
   when a doc is known-stale", "point me at the one file that matters". Vague
   advice ("communicate better") is not a lane.
2. **Instruction change** — a durable rule belongs in `CLAUDE.md`/`AGENTS.md`.
   **Quote the exact text to add.** A proposal that is not copy-pasteable will
   not survive the end of this message.
3. **Tooling** — a hook, skill, script, CI check, lint rule, MCP server, or CLI
   would make the failure impossible rather than merely discouraged. Prefer
   making a class of error unrepresentable over documenting that it is bad.
   Name the specific tool and what it would gate on.
4. **Accept** — the cost of prevention exceeds the cost of recurrence. This is a
   legitimate outcome and saying so is better than manufacturing a fix.

Bias toward lane 3, then lane 2. A rule in a document is only as strong as the
attention budget of whoever reads it next; a check that fails loudly is not.

Be specific about what *actually* happened. A retrospective full of generic
lessons is a sign the journal was empty, and should be labelled as such.

## G6 — Repo retrospective

Structural, undocumented anomalies discovered this session — the things a
newcomer (human or agent) would trip on and that no document warns about.

Rate each on two axes:

**Criticality**
- **C4** — correctness, security, or data-loss risk
- **C3** — actively misleads readers or agents into wrong changes
- **C2** — costs time on every encounter
- **C1** — cosmetic or purely aesthetic

**Remediation cost**
- **R1** — under 30 minutes
- **R2** — a few hours
- **R3** — a day or more
- **R4** — needs design, or a decision that is not yours to make

Routing:

| | R1 | R2 | R3 | R4 |
|---|---|---|---|---|
| **C4** | fix now | fix now | file P0 | file P0 + escalate to user |
| **C3** | fix now | propose | file issue | file issue |
| **C2** | propose | file issue | register | register |
| **C1** | register | register | register | register |

"Register" means: record it in the handoff so the next session inherits the
knowledge rather than rediscovering it. Anything at C4 leads the report regardless
of cost.

Do not fix anything outside the session's scope without asking — G0 exists to
catch exactly that. Propose, and let the user decide.

## G7 — Handoff and decision log

Write the artifact that makes the next session start warm. Keep it short enough
to be read.

```markdown
# Handoff — <date>, <branch>

## Shipped
<what landed, in observable terms>

## Verified / unverified
<the G1 ledger, condensed — carry the unverified items forward loudly>

## Open loops
<TODOs, skipped or ignored tests, stubs, deferred work, known-broken paths>

## Decisions
<decision — why — what was rejected and why>

## Repo knowledge earned
<the C1/C2 registered anomalies, and any convention learned the hard way>

## Start here next time
<the single most useful next action>
```

Install it so the next bootstrap picks it up automatically:

```
aw handoff <file>
```

The decision log matters more than it looks: undocumented decisions get
re-litigated by the next session, which burns context re-deriving a conclusion
that was already reached — and sometimes reaches the opposite one.

---

## Close out

1. **Report to the user**: findings first, ordered by severity, each with its
   next action. Then the gate summary. Lead with anything at P0/C4, anything
   irreversible, and every UNVERIFIED claim.
2. **Propose the compounding deltas** — the `CLAUDE.md`/`AGENTS.md` additions
   from G5, as exact text, applied only with the user's agreement. This is the
   flywheel: each session's friction becomes next session's guardrail, and the
   instructions converge on the repo's real behaviour instead of its imagined one.
3. **Mark it done**: `aw done`

Report the review faithfully. A gate that failed, failed. A check that was skipped,
was skipped — say which and why. An exit review that always passes is not a gate,
it is a ritual, and it will cost the user more than it saves.

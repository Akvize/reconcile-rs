---
name: session-note
description: Record a friction point, decision, assumption, anomaly, unverified claim, or piece of debt into the session journal so the end-of-session retrospective rests on evidence rather than recollection. Use the moment something surprises you mid-session, or when the user says "note that", "remember this", "log that".
---

# Session note

One line, at the moment it happens. The end-of-session review reads these back.

```
aw note <kind> "<one sentence>"
```

## Kinds

| kind | use it when | why the retrospective wants it |
|---|---|---|
| `friction` | something cost time that should not have | the raw material for prevention |
| `decision` | a non-obvious choice was made | stops it being re-litigated next session |
| `anomaly` | the repo does something surprising and undocumented | feeds the repo retrospective |
| `assumption` | you proceeded without confirming something | flags risk carried into the deliverable |
| `unverified` | you claimed or implied something works without proving it | the honesty ledger's input |
| `scope` | the goal or its boundary was set or changed | the drift gate's reference point |
| `debt` | you knowingly left something incomplete | becomes an open loop in the handoff |

## Why bother

A retrospective written from memory at the end of a long session is the least
reliable one available. Early friction has been compacted out of context, and the
gaps get filled with plausible-sounding bumps that never happened. A journal turns
"what went wrong today?" from a memory test into a reading exercise.

The bar is low on purpose: **if it surprised you, note it.** Ten seconds now,
against a retrospective that is either evidence-based or fiction.

Tool failures are journalled automatically by the `PostToolUseFailure` hook — do
not duplicate them. Note the things a hook cannot see: wrong assumptions,
misleading documentation, conventions discovered too late, decisions taken.

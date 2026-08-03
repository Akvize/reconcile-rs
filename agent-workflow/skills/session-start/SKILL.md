---
name: session-start
description: Bootstrap a working session before any code is written — establish how the project proves itself, what conventions bind it, the git baseline, and what the previous session left unfinished. Use at the start of a session, when picking up work in an unfamiliar or half-remembered repo, when the user says "start a session", "bootstrap", "let's begin", or invokes /session-start.
---

# Session bootstrap

The purpose of bootstrap is to **replace guessing with knowing** before the first
edit. Most bad agent sessions are lost in the first five minutes: a test command
is invented, a convention is assumed, a doc is trusted that no longer matches the
code. Everything below is cheap; discovering any of it *after* writing code is not.

This skill is project-agnostic. It never assumes a language, toolchain, or layout —
it probes. If a step finds nothing, say so out loud rather than filling the gap
with a plausible default.

## 1. Read the injected facts

The `SessionStart` hook has already probed the repo and injected a bootstrap block
(verification commands, convention sources, lint configs, doc surface, git
baseline, previous handoff). If it is absent, the hooks are not installed — run
the probes yourself:

```
aw verify        # how this project proves itself
aw conventions   # convention sources, lint configs, doc surface
aw baseline      # recorded git baseline
```

## 2. Prove the verification loop actually runs — before writing code

This is the step people skip and regret. Pick the cheapest command from the
detected set (usually a lint or a fast unit-test target) and **run it now**, on
unmodified code.

You are establishing three things:
- the command exists and works in *this* environment (not just in CI);
- what a passing baseline looks like, so you can tell your breakage from pre-existing breakage;
- how long the loop takes, which decides how often you can afford to run it.

If it fails on untouched code, that is a finding, not an obstacle: record it
(`aw note anomaly "…"`) and tell the user before proceeding. Never silently work
around a broken baseline — you will otherwise spend the session unable to
distinguish your own damage from the pre-existing kind.

If no verification command could be detected at all, **ask the user**. Do not
invent one.

## 3. Internalise the conventions, then state them back

Read the convention sources that exist (`CLAUDE.md`, `AGENTS.md`, `CONTRIBUTING.md`,
lint configs, `.editorconfig`). Then do the thing documents cannot do for you:
**read two or three files adjacent to where you will be working** and note the
conventions that are practised but never written down — error-handling style,
naming, test layout, comment density, module structure.

Written conventions tell you what the project aspires to. Neighbouring code tells
you what it actually does. Where they conflict, the code usually wins, and the
conflict itself is a finding for the exit review.

State back to the user, in three or four lines, the conventions you will follow.
This is cheap and catches misalignment immediately.

If there is no `CLAUDE.md`/`AGENTS.md`, note it (`aw note anomaly "no agent
instruction file"`). The exit review will propose one seeded from what you learn.

## 4. Pick up unfinished business

If a previous handoff was injected, read it and confirm with the user what carries
over. If the bootstrap reported a *pending retrospective* from a session that
ended without one, offer to run `/session-end` over its preserved journal first —
those findings are still valid and still unbanked.

## 5. Fix the intent in writing

Before touching code, write down — to the user, in two or three lines:

- **Goal:** what "done" means, in observable terms.
- **In scope / out of scope:** the boundary, so drift is detectable later.
- **Verification:** the exact command(s) that will demonstrate success.
- **Blast radius:** does this session expect to push, publish, migrate, or delete?

Record it: `aw note scope "<the goal and boundary>"`.

This costs thirty seconds and is what the exit review's scope-drift gate compares
against. Without it, "did we ship what was asked?" cannot be answered by anything
except memory, which is exactly the thing that fails.

## 6. Journal as you go

The exit review is only as good as the evidence collected during the session.
Tool failures are captured automatically. Everything else needs one line from you,
at the moment it happens — not reconstructed at the end:

```
aw note friction   "README documents --flag; the binary rejects it"
aw note decision   "chose X over Y because Z"
aw note anomaly    "tests/ imports from src/internal, which is private by convention"
aw note assumption "assuming the staging DB schema matches prod — unverified"
aw note unverified "refactor compiles and unit-tests pass; no integration test covers this path"
aw note debt       "left the slow path unoptimised; see TODO in parser.rs"
```

Rule of thumb: **if it surprised you, journal it.** Surprises are exactly the
material the retrospective needs, and they are exactly what you will have
forgotten by the end.

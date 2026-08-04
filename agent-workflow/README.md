# agent-workflow

A project-agnostic bootstrap and exit discipline for agent coding sessions.

Two moments decide whether a session was worth it, and neither is the middle.
**Bootstrap** decides whether the agent is working from knowledge or from
guesses. **Exit** decides whether anything learned survives the session, or dies
with the context window.

Nothing here knows what language your project is in. Every fact about a repo is
probed at runtime, so the same bundle behaves correctly in a Rust crate, a
Django service, a Terraform module, or a repo it has never seen.

---

## The design constraint worth knowing up front

You cannot make a retrospective run automatically "when the session ends".

`SessionEnd` hooks fire on a 1.5-second budget and **cannot inject context into
the model** — by the time they run, there is no model turn left to drive. `Stop`
is the only late-firing event that can reach the model, and it fires at the end
of *every* turn, not at the end of the session.

So this bundle does the achievable thing instead, in three parts:

1. **Nudge once.** A `Stop` hook trips a single time per session, and only once
   the session has actually produced work (commits, or more dirty files than it
   started with). One reminder, not a per-turn nag.
2. **Leave a breadcrumb.** A `SessionEnd` hook records that the review never ran
   and preserves the journal.
3. **Recover next time.** The next session's bootstrap sees the breadcrumb and
   offers to run the review over the preserved evidence. Findings from a session
   that ended abruptly are still valid — they just have not been banked yet.

Set `AW_RETRO_MODE=block` if you want a hard gate that refuses to end a turn
until the review runs, or `off` to disable the nudge entirely.

---

## Why the journal is the load-bearing part

A retrospective written from memory at the end of a long session is the least
reliable one available. Early friction has been compacted out of context, and the
gaps get filled with plausible-sounding bumps that never happened.

So the bundle **records during, synthesises at exit**:

- A `PostToolUseFailure` hook journals **every failed tool call** automatically.
  This is free, and it is the highest-signal friction data that exists — each one
  is a wrong assumption, a missing dependency, or a command that does not work
  here.
- The agent adds one line whenever something surprises it: `aw note friction …`,
  `aw note decision …`, `aw note anomaly …`.

At exit, "what went wrong today?" becomes a reading exercise rather than a memory
test.

---

## Install

### Locally (terminal sessions)

```bash
./install.sh              # install or upgrade (idempotent)
./install.sh --dry-run    # show the resulting settings.json without writing
./install.sh --uninstall  # remove hooks, skills and CLI; state is preserved
```

This installs into `~/.claude`, so it applies to **every** project — no per-repo
setup, nothing committed into your repositories. Existing hooks in your
`settings.json` are preserved; a `.agent-workflow.bak` backup is written.

Session state lives in `~/.claude/agent-workflow-state/<repo-key>/`, keyed by git
remote so clones and worktrees of the same project share it. Your repos stay clean.

### Everywhere else (Claude web, mobile, routines)

Cloud sessions never see your laptop's `~/.claude`, so the two halves install
differently:

**Skills — enable them on claude.ai.** Cloud and Cowork sessions load the skills
enabled for your claude.ai account, synced at session start. Add
`session-start`, `session-end` and `session-note` there once and they are
available in every session on every surface, with no repo and no setup script.
This is the bulk of the value, and it costs one upload.

**Hooks — one setup script per cloud environment.** Paste
[`cloud-setup-script.sh`](./cloud-setup-script.sh) into the **Setup script**
field of your environment at claude.ai/code (cloud icon above the message box →
environment → gear). It runs as root before Claude Code launches, installs the
bundle into the VM's `~/.claude`, and is snapshotted, so it runs once rather than
per session. Every session in that environment then gets the automatic
journalling, baseline and exit nudge, whatever repo is attached.

That user-settings layer being read inside the VM is verified, not assumed: a
`Setup` and a `SessionStart` hook written to the VM's `~/.claude/settings.json`
both fired. What does not carry over is your *local* `~/.claude`, because it is
never uploaded — a transport gap rather than a policy block.

The skills work standalone. Each one carries a **Without the `aw` CLI** section
with plain-git equivalents, so the claude.ai-only install is fully usable and the
hooks are a genuine upgrade rather than a prerequisite.

---

## What you get

| | |
|---|---|
| `/session-start` | Bootstrap: prove the verification loop runs, internalise conventions, pick up the previous handoff, fix the intent in writing. |
| `/session-end` | Eight-gate exit review (below). |
| `/session-note` | One-line journalling during the session. |
| `aw` | The CLI the skills drive — `note`, `journal`, `diff`, `secrets`, `deps`, `handoff`, `done`. |

### The eight exit gates

| Gate | Question |
|---|---|
| **G0** Scope drift | Did we ship what was asked — no less (silent narrowing) and no more (unrequested extras)? |
| **G1** Verification honesty | Is every "this works" traceable to a command that actually ran? Unverified claims are named as unverified. |
| **G2** Conventions | Linters pass *and* the code matches what neighbouring files actually practise. |
| **G3** Docs | Nothing stale, nothing missing, nothing newly discovered to be already wrong. |
| **G4** Blast radius | What cannot be walked back — pushed, published, migrated, deleted — plus a secret sweep and dependency delta. |
| **G5** Agent retro | Each bump → root cause → prevention lane: human practice, instruction change, tooling, or accept. |
| **G6** Repo retro | Undocumented structural anomalies, scored criticality × remediation cost, with a routing matrix. |
| **G7** Handoff | Open loops, decisions with rejected alternatives, and the next session's starting point. |

Every gate ends in **PASS with evidence** or a **FINDING with severity and a next
action**. "Looks fine" is not an allowed outcome — a gate that always passes is a
ritual, not a gate.

---

## The part that compounds

G5 does not stop at naming what went wrong. Each bump is routed to a prevention
lane, in preference order:

1. **Tooling** — a hook, CI check, or lint rule that makes the error impossible.
2. **Instruction change** — exact, copy-pasteable text for `CLAUDE.md`/`AGENTS.md`.
3. **Human practice** — a concrete change in how you brief the agent.
4. **Accept** — prevention costs more than recurrence. A legitimate answer.

The bias toward tooling is deliberate: a rule in a document is only as strong as
the attention budget of whoever reads it next, whereas a check that fails loudly
is not.

Over N sessions the project's agent instructions converge on the repo's *actual*
behaviour instead of its imagined one — and that is the whole point. Each
session's friction becomes the next session's guardrail.

---

## Relocating this bundle

It is deliberately self-contained: one directory, no references to the repo that
happens to host it. To move it into your dotfiles:

```bash
cp -R agent-workflow /path/to/dotfiles/claude/agent-workflow
cd /path/to/dotfiles && git add claude/agent-workflow && git commit -m "add agent-workflow"
```

Nothing in it needs editing after the move.

---

## Layout

```
agent-workflow/
  install.sh                       idempotent installer / uninstaller
  settings.snippet.json            the four hook registrations
  bin/aw                           session CLI driven by the skills
  hooks/
    lib.sh                         shared probing + journal library
    session-bootstrap.sh           SessionStart  → inject repo facts
    journal-tool-failure.sh        PostToolUseFailure → auto-journal bumps
    session-exit-nudge.sh          Stop          → one-shot review reminder
    session-end-breadcrumb.sh      SessionEnd    → breadcrumb for recovery
  skills/
    session-start/SKILL.md
    session-end/SKILL.md
    session-note/SKILL.md
```

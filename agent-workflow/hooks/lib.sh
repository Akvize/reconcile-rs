#!/usr/bin/env bash
# agent-workflow shared hook library.
#
# Sourced by every hook. Deliberately free of project-specific knowledge: every
# fact about the repository is probed at runtime so the same bundle works in a
# Rust crate, a Django service, or a Terraform module without edits.
#
# Hooks must never break a session. Nothing here uses `set -e`, every external
# call is guarded, and callers are expected to `exit 0` unconditionally.

set -uo pipefail

AW_HOME="${AW_HOME:-$HOME/.claude/agent-workflow}"
AW_STATE_ROOT="${AW_STATE_ROOT:-$HOME/.claude/agent-workflow-state}"

# --------------------------------------------------------------------------
# JSON helpers (python3 preferred, jq fallback, sed last resort)
# --------------------------------------------------------------------------

aw_json_get() {
  # aw_json_get <json> <key>  -> value on stdout, empty if absent
  local json="${1:-}" key="${2:-}"
  [ -n "$json" ] || return 0
  if command -v python3 >/dev/null 2>&1; then
    AW_KEY="$key" printf '%s' "$json" | AW_KEY="$key" python3 -c '
import json, os, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
v = d.get(os.environ.get("AW_KEY", ""), "")
sys.stdout.write("" if v is None else str(v))
' 2>/dev/null
  elif command -v jq >/dev/null 2>&1; then
    printf '%s' "$json" | jq -r --arg k "$key" '.[$k] // empty' 2>/dev/null
  else
    printf '%s' "$json" |
      sed -n "s/.*\"${key}\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -1
  fi
}

aw_emit_context() {
  # aw_emit_context <hookEventName> <context text>
  local ev="${1:-}" txt="${2:-}"
  [ -n "$txt" ] || return 0
  if command -v python3 >/dev/null 2>&1; then
    AW_EV="$ev" AW_TXT="$txt" python3 -c '
import json, os
print(json.dumps({"hookSpecificOutput": {
    "hookEventName": os.environ["AW_EV"],
    "additionalContext": os.environ["AW_TXT"],
}}))
' 2>/dev/null && return 0
  fi
  if command -v jq >/dev/null 2>&1; then
    jq -n --arg ev "$ev" --arg txt "$txt" \
      '{hookSpecificOutput:{hookEventName:$ev,additionalContext:$txt}}' 2>/dev/null && return 0
  fi
  local esc
  esc=$(printf '%s' "$txt" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk '{printf "%s\\n", $0}')
  printf '{"hookSpecificOutput":{"hookEventName":"%s","additionalContext":"%s"}}\n' "$ev" "$esc"
}

# --------------------------------------------------------------------------
# Repo / session identity
# --------------------------------------------------------------------------

aw_repo_root() {
  local d="${1:-$PWD}"
  git -C "$d" rev-parse --show-toplevel 2>/dev/null || printf '%s' "$d"
}

aw_repo_key() {
  # Stable per-project key. Prefers the git remote so clones and worktrees of
  # the same project share retrospective state; falls back to the path.
  local root remote basis key
  root="$(aw_repo_root "${1:-$PWD}")"
  remote="$(git -C "$root" config --get remote.origin.url 2>/dev/null)"
  basis="${remote:-$root}"
  key="$(printf '%s' "$basis" |
    sed -e 's#\.git$##' -e 's#.*[/:]\([^/]*/[^/]*\)$#\1#' -e 's#[^A-Za-z0-9._-]#_#g')"
  [ -n "$key" ] || key="$(basename "$root" 2>/dev/null || echo project)"
  printf '%s' "$key"
}

aw_status_hash() {
  # Fingerprint of the working tree. `-uall` matters: without it git collapses an
  # untracked directory into a single entry, so a session that creates a whole
  # new tree looks identical to one that did nothing. Hashing rather than
  # counting also survives an add-plus-revert that nets to the same total.
  local root="${1:-$PWD}" st
  st="$(git -C "$root" status --porcelain -uall 2>/dev/null)"
  if command -v sha1sum >/dev/null 2>&1; then
    printf '%s' "$st" | sha1sum | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    printf '%s' "$st" | shasum | cut -d' ' -f1
  else
    printf '%s' "$st" | cksum | tr -d ' '
  fi
}

aw_status_count() {
  git -C "${1:-$PWD}" status --porcelain -uall 2>/dev/null | wc -l | tr -d ' '
}

aw_state_dir() {
  local d="$AW_STATE_ROOT/$(aw_repo_key "${1:-$PWD}")"
  mkdir -p "$d" 2>/dev/null
  printf '%s' "$d"
}

aw_session_dir() {
  # aw_session_dir <session_id> [cwd]
  local sid="${1:-unknown}" d
  d="$(aw_state_dir "${2:-$PWD}")/sessions/$sid"
  mkdir -p "$d" 2>/dev/null
  printf '%s' "$d"
}

# --------------------------------------------------------------------------
# Journal — the evidence substrate for the exit retrospective.
# A retrospective written from memory at the end of a long session is the least
# reliable one: early friction is forgotten and plausible-sounding bumps get
# invented. Recording as we go is what makes the exit review evidence-based.
# --------------------------------------------------------------------------

aw_journal_append() {
  # aw_journal_append <session_dir> <kind> <message>
  local sdir="${1:-}" kind="${2:-note}" msg="${3:-}"
  [ -n "$sdir" ] || return 0
  mkdir -p "$sdir" 2>/dev/null
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
  if command -v python3 >/dev/null 2>&1; then
    AW_TS="$ts" AW_KIND="$kind" AW_MSG="$msg" python3 -c '
import json, os, sys
sys.stdout.write(json.dumps({
    "ts": os.environ["AW_TS"],
    "kind": os.environ["AW_KIND"],
    "message": os.environ["AW_MSG"],
}) + "\n")
' >>"$sdir/journal.jsonl" 2>/dev/null
  else
    local esc
    esc=$(printf '%s' "$msg" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | tr '\n' ' ')
    printf '{"ts":"%s","kind":"%s","message":"%s"}\n' "$ts" "$kind" "$esc" \
      >>"$sdir/journal.jsonl" 2>/dev/null
  fi
}

# --------------------------------------------------------------------------
# Repository probes — all discovery, no assumptions
# --------------------------------------------------------------------------

aw_find_convention_files() {
  # Files that encode how this project expects code to be written.
  local root="${1:-$PWD}" f
  for f in CLAUDE.md AGENTS.md .cursorrules .windsurfrules CONTRIBUTING.md \
    CONTRIBUTING.rst STYLE.md STYLEGUIDE.md CODING_STANDARDS.md \
    ARCHITECTURE.md DECISIONS.md .editorconfig; do
    [ -e "$root/$f" ] && printf '%s\n' "$f"
  done
  for f in "$root"/.claude/rules/*.md "$root"/.github/copilot-instructions.md \
    "$root"/docs/adr "$root"/doc/adr; do
    [ -e "$f" ] && printf '%s\n' "${f#"$root"/}"
  done
  return 0
}

aw_find_lint_configs() {
  local root="${1:-$PWD}" f
  for f in .pre-commit-config.yaml .pre-commit-config.yml pre-commit \
    rustfmt.toml .rustfmt.toml clippy.toml .clippy.toml \
    .eslintrc .eslintrc.json .eslintrc.js eslint.config.js .prettierrc \
    .prettierrc.json biome.json ruff.toml .ruff.toml setup.cfg tox.ini \
    .flake8 mypy.ini .golangci.yml .golangci.yaml .rubocop.yml \
    .checkstyle.xml .swiftlint.yml phpcs.xml .stylelintrc; do
    [ -e "$root/$f" ] && printf '%s\n' "$f"
  done
  return 0
}

aw_detect_verification() {
  # The single highest-value thing to establish before writing code: how this
  # project actually proves itself. CI is listed first because it is ground
  # truth — whatever CI runs is what "passing" means here.
  local root="${1:-$PWD}"

  if [ -d "$root/.github/workflows" ]; then
    local ci
    ci="$(grep -rhE '^\s+run:\s*\S' "$root/.github/workflows" 2>/dev/null |
      sed -e 's/^\s*run:\s*//' -e 's/^[[:space:]]*//' |
      grep -vE '^(\||>)' | sort -u | head -12)"
    [ -n "$ci" ] && { printf 'from CI (.github/workflows) — ground truth:\n'; printf '%s\n' "$ci" | sed 's/^/  $ /'; }
  fi

  if [ -f "$root/Makefile" ] || [ -f "$root/makefile" ]; then
    local mk
    mk="$(grep -hE '^[a-zA-Z0-9_.-]+:' "$root/Makefile" "$root/makefile" 2>/dev/null |
      cut -d: -f1 | sort -u | grep -iE 'test|lint|check|fmt|format|build|ci|verify|audit' | head -10)"
    [ -n "$mk" ] && { printf 'make targets:\n'; printf '%s\n' "$mk" | sed 's/^/  $ make /'; }
  fi

  if [ -f "$root/package.json" ] && command -v python3 >/dev/null 2>&1; then
    local ns
    ns="$(python3 -c '
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
for k in list((d.get("scripts") or {}).keys())[:12]:
    print("  $ npm run " + k)
' "$root/package.json" 2>/dev/null)"
    [ -n "$ns" ] && { printf 'npm scripts:\n'; printf '%s\n' "$ns"; }
  fi

  for jf in justfile Justfile .justfile; do
    if [ -f "$root/$jf" ]; then
      local jr
      jr="$(grep -hE '^[a-zA-Z0-9_-]+.*:' "$root/$jf" 2>/dev/null | cut -d: -f1 | sort -u | head -10)"
      [ -n "$jr" ] && { printf 'just recipes:\n'; printf '%s\n' "$jr" | sed 's/^/  $ just /'; }
      break
    fi
  done

  # Ecosystem defaults, only when nothing more specific was declared.
  [ -f "$root/Cargo.toml" ] && printf 'rust defaults:\n  $ cargo test --all\n  $ cargo clippy --all -- --deny warnings\n  $ cargo fmt --check\n'
  [ -f "$root/go.mod" ] && printf 'go defaults:\n  $ go test ./...\n  $ go vet ./...\n'
  [ -f "$root/pyproject.toml" ] && printf 'python: pyproject.toml present — check [tool.*] for the runner (pytest/ruff/mypy/tox)\n'
  { [ -f "$root/.pre-commit-config.yaml" ] || [ -f "$root/.pre-commit-config.yml" ]; } &&
    printf 'pre-commit:\n  $ pre-commit run --all-files\n'
  [ -x "$root/pre-commit" ] && printf 'repo-local hook script:\n  $ ./pre-commit\n'
  return 0
}

aw_doc_surface() {
  # Docs whose accuracy is a deliverable, not a nicety.
  local root="${1:-$PWD}" f
  for f in README.md README.rst docs doc CHANGELOG.md ARCHITECTURE.md \
    PROGRESS.md ROADMAP.md SOTA.md API.md; do
    [ -e "$root/$f" ] && printf '%s\n' "$f"
  done
  return 0
}

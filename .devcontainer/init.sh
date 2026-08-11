#!/usr/bin/env bash
set -euo pipefail

export PATH="/usr/local/cargo/bin:/usr/local/rustup/bin:$PATH"

# This script runs in two modes:
#  - create: heavy setup (cargo update/fetch)
#  - start : lightweight link hook & version check
mode=${1:-start}

echo "🚀 Running init tasks (mode=$mode)..."

if [[ "$mode" == "create" ]]; then
  echo "📦 Running cargo update && fetch"
  cargo update && cargo fetch
  echo "✅ Cargo dependencies fetched"
  exit 0
fi

if [[ "$mode" == "start" ]]; then
  if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    workspace_root="$git_root"
  else
    workspace_root="/workspace"
  fi

  # Both tiers of the gate, see AGENTS.md §3.2
  for hook in pre-commit pre-push; do
    hook_path="$workspace_root/.git/hooks/$hook"
    if [[ -d "$workspace_root/.git" ]] && [[ ! -L "$hook_path" ]]; then
      echo "🔗 Linking $hook hook"
      ln -sf "$workspace_root/$hook" "$hook_path"
    fi
  done

  if [[ -n "${GIT_AUTHOR_NAME:-}" ]]; then
    git config --global user.name "$GIT_AUTHOR_NAME"
  fi
  if [[ -n "${GIT_AUTHOR_EMAIL:-}" ]]; then
    git config --global user.email "$GIT_AUTHOR_EMAIL"
  fi

  echo "🔧 rustc: $(rustc --version)"
  echo "✅ Init start tasks complete"
  exit 0
fi
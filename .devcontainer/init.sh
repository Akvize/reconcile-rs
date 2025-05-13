#!/usr/bin/env bash
set -euo pipefail

export PATH="/usr/local/cargo/bin:/usr/local/rustup/bin:$PATH"

# This script runs in two modes:
#  - create: heavy setup (cargo update/fetch)
#  - start : lightweight link hook & version check
mode=${1:-start}

echo "🚀 Running init tasks (mode=$mode)..."

if [[ "$mode" == "create" ]]; then
  # Heavy, one-time operations after container creation
  echo "📦 Running cargo update && fetch"
  cargo update && cargo fetch
  echo "✅ Cargo dependencies fetched"
  exit 0
fi

if [[ "$mode" == "start" ]]; then
  # Lightweight operations on each container start
  # Determine repo root
  if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    workspace_root="$git_root"
  else
    workspace_root="/workspace"
  fi

  # Link pre-commit hook if missing
  hook_path="$workspace_root/.git/hooks/pre-commit"
  if [[ -d "$workspace_root/.git" ]] && [[ ! -L "$hook_path" ]]; then
    echo "🔗 Linking pre-commit hook"
    ln -sf "$workspace_root/pre-commit" "$hook_path"
  fi

  # Add your Git config here
  if [[ -n "${GIT_AUTHOR_NAME:-}" ]]; then
    git config --global user.name "$GIT_AUTHOR_NAME"
  fi
  if [[ -n "${GIT_AUTHOR_EMAIL:-}" ]]; then
    git config --global user.email "$GIT_AUTHOR_EMAIL"
  fi

  # Version check
  echo "🔧 rustc: $(rustc --version)"
  echo "✅ Init start tasks complete"
  exit 0
fi
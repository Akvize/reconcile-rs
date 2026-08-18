#!/usr/bin/env bash
# Path categories mirroring `main.yml`'s `changes` job (dorny/paths-filter) and `mutants.yml`'s own
# copy of the same `rust` filter -- kept in sync by hand across all three, the same convention
# AGENTS.md §3 already uses for `main.yml` vs its command list. Change one, change all three.
#
# Sourced, not run: `./pre-push` and `./scripts/run-affected-checks.sh` both need the same "does
# this diff touch category X" predicate, and a rule a script enforces belongs in exactly one place
# (AGENTS.md §10) rather than copied into each caller by hand.

# changed_paths <base-ref> [<target>]
#   <target> a commit-ish: the historical diff between the two, merge-base relative
#     (`base...target`, matching how a GitHub PR's own diff is computed) -- pre-push, checking one
#     pushed commit's tree in isolation.
#   <target> omitted: <base-ref> against the current working tree -- staged, unstaged and
#     committed-since-<base-ref> changes alike -- plus untracked files. Deliberately a direct diff
#     against <base-ref>, not merge-base-relative: `git diff <ref>...` with the second side omitted
#     resolves to `<ref>...HEAD`, a commit-to-commit diff that is silently empty whenever <ref> and
#     HEAD already coincide -- exactly the case with nothing committed yet -- which would make this
#     invisible to precisely the uncommitted change it exists to catch. A direct diff can include
#     unrelated churn if <base-ref> has moved since the branch point; that only makes this run a
#     command that turns out unaffected, never the reverse.
changed_paths() {
    local base="$1" target="${2:-}"
    if [ -z "$target" ]; then
        git diff --name-only "$base"
        git ls-files --others --exclude-standard
    else
        git diff --name-only "${base}...${target}"
    fi | sort -u
}

# One case arm per line of `main.yml`'s `rust`/`deps` filters (comment there points back here).
_is_rust_path() {
    case "$1" in
        *.rs | */Cargo.toml | Cargo.toml | Cargo.lock | rust-toolchain* | .github/workflows/main.yml) return 0 ;;
        *) return 1 ;;
    esac
}

_is_deps_path() {
    case "$1" in
        */Cargo.toml | Cargo.toml | Cargo.lock | deny.toml | .github/workflows/main.yml) return 0 ;;
        *) return 1 ;;
    esac
}

_any_path_matches() {
    local matcher="$1" base="$2" target="${3:-}" path
    while IFS= read -r path; do
        [ -z "$path" ] && continue
        "$matcher" "$path" && return 0
    done < <(changed_paths "$base" "$target")
    return 1
}

affects_rust() { _any_path_matches _is_rust_path "$1" "${2:-}"; }
affects_deps() { _any_path_matches _is_deps_path "$1" "${2:-}"; }

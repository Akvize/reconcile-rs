#!/usr/bin/env bash
# Structural correctness of the repository's Markdown, in three parts: link targets resolve, anchor
# links resolve, and SOTA.md's bibliography entries carry what §4.2 says they must.
#
# Why this is gated rather than reviewed: a link that resolves nowhere renders as a link. Nothing
# about `[`Payload`](../gossip/src/auth.rs)` looks wrong in a diff -- ARCHITECTURE.md sits at the
# repository root, so `../` escapes the repository and the link 404s on GitHub. That one, and a port
# table pointing at `reconcile/src/clock.rs` when the facade crate *is* the root (`src/`), both
# survived review and were found by probing. AGENTS.md §10: a rule enforced by eye belongs in a
# failing command instead.
#
# ---------------------------------------------------------------------------------------------
# The precision rules below are the whole design, and they were derived by measurement, not taste.
# On this tree a naive matcher flagged 30 paths, a refined one 6, and the correct one 2 -- the two
# real bugs. Every exclusion here bought a false-positive class, and a checker that reports any of
# them is a checker someone disables:
#
#   bare filenames used as prose   `auth.rs`                    -> require a `/`
#   backticked URLs                `github.com/amparore/aelmdb` -> require an extension, skip hosts
#   arithmetic that looks like one `1/b`                        -> require an extension
#   generated artifacts            `target/criterion/…`         -> skip target/
#   docs in subdirectories         examples/README.md's `k8s/…` -> resolve from the doc's OWN dir
#
# That last one is the subtle one: `examples/README.md` refers to `k8s/main.rs`, which exists at
# `examples/k8s/main.rs`. Resolving from the repository root instead condemns a correct document.
# ---------------------------------------------------------------------------------------------
set -Eeuo pipefail

# Resolve the repo root from the script's own location, not `git rev-parse`: the pre-commit hook
# runs this against a bare `git checkout-index` copy that has no `.git` (same reason as
# check-doc-budget.sh).
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

status=0
fail() { echo "check-doc-structure: $*" >&2; status=1; }

mapfile -t DOCS < <(
    ls -1 ./*.md 2>/dev/null
    ls -1 ./*/README.md 2>/dev/null
    ls -1 ./.github/*.md 2>/dev/null
)

# GitHub's heading-slug rule, as applied to a heading's text: lowercase, drop Markdown emphasis and
# link syntax, drop remaining punctuation, spaces to hyphens. Explicit `<a id="…">` anchors count
# too -- SOTA.md's glossary uses them (`g91`…`g96`) precisely because its headings are numbered.
#
# Cached per file, and matched by string containment rather than by piping into `grep -q`. Both
# choices are bug fixes, not style: `grep -q` exits at the first match, the SIGPIPE kills the `sed`
# upstream of it, and `set -o pipefail` then reports 141 for a pipeline that *succeeded*. A trailing
# `grep` that matches nothing (a document with no `<a id=>`) fails the function the same way. The
# cache also keeps this inside the 0.4 s pre-commit budget: one pass per document instead of one per
# link, which on this tree is 14 passes instead of 182.
declare -A ANCHOR_CACHE
anchors_for() {
    local f=$1 headings ids
    [[ -v ANCHOR_CACHE[$f] ]] && return 0
    headings=$(
        sed -nE 's/^#{1,6}[[:space:]]+(.*)$/\1/p' "$f" |
            sed -E 's/[`*_]//g; s/\[([^]]*)\]\([^)]*\)/\1/g' |
            sed -E 's/[^[:alnum:][:space:]_-]//g' |
            tr '[:upper:]' '[:lower:]' |
            sed -E 's/[[:space:]]+/-/g; s/^-+|-+$//g'
    ) || true
    ids=$(grep -oE '<a[[:space:]]+id="[^"]+"' "$f" | sed -E 's/.*id="([^"]+)".*/\1/') || true
    ANCHOR_CACHE[$f]=$'\n'"$headings"$'\n'"$ids"$'\n'
}

# ---- 1 & 2: Markdown link targets and anchors -------------------------------------------------
links_checked=0
for doc in "${DOCS[@]}"; do
    dir=$(dirname "$doc")
    while IFS= read -r target; do
        case "$target" in http://*|https://*|mailto:*|"") continue ;; esac
        file_part=${target%%#*}
        frag=""
        [[ "$target" == *"#"* ]] && frag=${target#*#}

        if [ -n "$file_part" ]; then
            resolved="$dir/$file_part"
            if [ ! -e "$resolved" ]; then
                fail "$doc: link target does not exist: $target"
                continue
            fi
        else
            resolved="$doc"
        fi

        if [ -n "$frag" ] && [[ "$resolved" == *.md ]]; then
            links_checked=$((links_checked + 1))
            anchors_for "$resolved"
            [[ ${ANCHOR_CACHE[$resolved]} == *$'\n'"$frag"$'\n'* ]] ||
                fail "$doc: anchor does not resolve: $target"
        fi
    done < <(grep -oE '\]\([^)[:space:]]+\)' "$doc" | sed -E 's/^\]\(//; s/\)$//')
done

# ---- 3: backticked path-shaped references ------------------------------------------------------
for doc in "${DOCS[@]}"; do
    dir=$(dirname "$doc")
    while IFS= read -r ref; do
        # Require a separator and a file extension; both exclusions are load-bearing (header).
        [[ "$ref" == */* ]] || continue
        [[ "$ref" =~ \.[a-z]+$ ]] || continue
        case "$ref" in
            *.com/*|*.org/*|*.io/*|*.net/*|*.dev/*) continue ;;  # backticked URLs
            target/*) continue ;;                                # build output, absent by design
            *'*'*|*'{'*) continue ;;                             # globs and placeholders
        esac
        [ -e "$dir/$ref" ] || [ -e "$ref" ] || fail "$doc: path does not exist: $ref"
    done < <(grep -oE '`[A-Za-z0-9_][A-Za-z0-9_./*{}-]*`' "$doc" | tr -d '`')
done

# ---- 4: SOTA.md bibliography entry format (§4.2) -----------------------------------------------
# §4.2 grandfathers entries written before the rule, so this cannot demand `Bears on:` everywhere.
# What it can demand is that an entry which *opts in* is complete: a version-pinned identifier and a
# forward pointer. That makes the format self-enforcing as entries migrate, with no flag day -- and
# it is exactly the shape §10 asks for, since "remember to pin the version" is otherwise a rule a
# human applies by eye.
if [ -f SOTA.md ]; then
    entries=0
    while IFS=$'\t' read -r lineno block; do
        entries=$((entries + 1))
        if ! grep -qE 'arXiv:[0-9]{4}\.[0-9]{4,5}v[0-9]+|doi:10\.' <<<"$block"; then
            fail "SOTA.md:$lineno: entry has 'Bears on:' but no version-pinned arXiv id or doi: (§4.2 rule 1)"
        fi
        if ! grep -qF '→' <<<"$block"; then
            fail "SOTA.md:$lineno: entry has 'Bears on:' but no '→' forward pointer (§4.2 rule 3)"
        fi
    done < <(awk '
        /^- / { if (buf != "" && buf ~ /Bears on:/) printf "%d\t%s\n", start, buf; buf = $0; start = NR; next }
        /^[[:space:]]+/ { if (buf != "") buf = buf " " $0; next }
        { if (buf != "" && buf ~ /Bears on:/) printf "%d\t%s\n", start, buf; buf = "" }
        END { if (buf != "" && buf ~ /Bears on:/) printf "%d\t%s\n", start, buf }
    ' SOTA.md)
    echo "check-doc-structure: ${#DOCS[@]} docs, $links_checked anchor links, $entries §4.2 entries"
fi

exit "$status"

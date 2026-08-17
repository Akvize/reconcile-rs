#!/usr/bin/env bash
# Shared by check-closed-issue-rustdoc-refs.sh (the after-the-fact audit) and
# check-pr-closing-issue-rustdoc-refs.sh (the before-merge race guard) so the two checks cannot
# disagree on what counts as "a rustdoc issue reference" -- the same failure mode
# check-pr-closes-issues.sh's own header warns against for its closing-keyword regex.
#
# Matches exactly the two forms already in use across this tree (verified against every rustdoc
# `#[0-9]+` occurrence before this script existed): a backtick citation (`` `#288` ``) and a
# rustdoc link reference, definition or use (`[#355]`, `[#355]: https://...`). Both require `#`
# immediately followed by digits with no intervening space, so a Rust attribute (`#[derive(...)]`,
# `#[test]`) never matches on either count: the digit adjacency rules it out on its own, and
# attributes never appear on a `///`/`//!` line to begin with, which is the line-level filter below.
#
# `check-doc-issue-claims.sh`'s header is the measured reason this does not try to read free prose:
# a looser "state word near a number" matcher produced three false positives out of three on this
# tree. Backtick/bracket delimiters are a deliberate citation, never incidental.
#
# Prints `file:line:number` triples to stdout, one per hit. A line citing more than one issue
# produces more than one triple. Duplicates across lines for the same issue are expected -- callers
# that only care about *distinct* numbers dedupe on their own end, this stays a straight scan.
rustdoc_issue_refs() {
    local file line rest hit num
    while IFS= read -r file; do
        while IFS=: read -r line rest; do
            while IFS= read -r hit; do
                [ -n "$hit" ] || continue
                num=$(tr -d '`[]#' <<<"$hit")
                echo "$file:$line:$num"
            done < <(grep -oE '`#[0-9]+`|\[#[0-9]+\]' <<<"$rest")
        done < <(grep -nE '^[[:space:]]*//[!/]' -- "$file")
    done < <(git ls-files -- '*.rs')
}

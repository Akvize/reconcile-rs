#!/usr/bin/env bash
# Forbids `<name>_tests.rs` as a split-`#[cfg(test)]`-module filename under any crate's `src/`.
#
# The convention (.claude/rules/big-files.md, .claude/rules/tests.md's globs) is `<dir>/tests.rs`
# via `mod tests;` -- e.g. `src/replica/tests.rs` for the module split out of `src/replica.rs`.
# #402 phase 1 split two files this way and named them differently:
# `src/replica/tests.rs` but `src/replicated_map/replicated_map_tests.rs`. The second matched
# none of tests.md's globs, silently exempting a 1199-line test file from the testing rule --
# found by review, not by any gate. Fixing that file's name is not enough on its own (AGENTS.md
# §10): gate the naming so the same drift can't recur under a different module's name.
set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR/.."

status=0
while IFS= read -r -d '' f; do
    echo "check-test-file-naming: $f -- split test modules are named 'tests.rs' (mod tests;)," >&2
    echo "  not '<name>_tests.rs'. Rename the file and its 'mod' declaration to match." >&2
    status=1
done < <(find . -path './target' -prune -o -path '*/src/*_tests.rs' -print0)

if [ "$status" -eq 0 ]; then
    echo "check-test-file-naming: no '<name>_tests.rs' files under src/"
fi

exit "$status"

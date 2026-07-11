#!/usr/bin/env bash
# Run the full nudge test-suite. Exits non-zero if any test file reports a
# failure. Individual files skip themselves when their prerequisites are absent.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

rc=0
for t in "$HERE"/test_*.sh; do
    printf '\n### %s\n' "$(basename "$t")"
    bash "$t" || rc=1
done

printf '\n=== suite %s ===\n' "$([ "$rc" -eq 0 ] && echo PASSED || echo FAILED)"
exit "$rc"

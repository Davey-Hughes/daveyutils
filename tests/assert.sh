#!/usr/bin/env bash
# Generic test assertions shared across the suite.

PASS=0
FAIL=0

# check <description> <expected> <actual>
check() {
    if [ "$2" = "$3" ]; then
        printf '  ok  : %s\n' "$1"
        PASS=$((PASS + 1))
    else
        printf '  FAIL: %s\n        expected [%s]\n        actual   [%s]\n' "$1" "$2" "$3"
        FAIL=$((FAIL + 1))
    fi
}

# Print the tally and return non-zero if anything failed.
finish() {
    printf '\n== %d passed, %d failed ==\n' "$PASS" "$FAIL"
    [ "$FAIL" -eq 0 ]
}

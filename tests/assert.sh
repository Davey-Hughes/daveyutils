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

# Print the tally and EXIT non-zero if anything failed.
#
# This terminates rather than returns so the status cannot be dropped: a caller
# that wrote `finish; exit 0` (as test_video_pcm_to_flac.sh's bash<4.3 skip
# branch did) would otherwise report a file with FAILing checks as exit 0, and
# run.sh's `bash "$t" || rc=1` would call the whole suite PASSED. Every test file
# ends with finish, so exiting here costs no expressiveness.
finish() {
    printf '\n== %d passed, %d failed ==\n' "$PASS" "$FAIL"
    if [ "$FAIL" -eq 0 ]; then exit 0; else exit 1; fi
}

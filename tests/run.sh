#!/usr/bin/env bash
# Run the full nudge test-suite. Exits non-zero if any test file reports a
# failure. Individual files skip themselves when their prerequisites are absent.
#
# The files run CONCURRENTLY. Each is self-contained -- every one makes its own
# `mktemp -d` working and stub directories and removes them on EXIT, sources
# nothing but the read-only assert.sh, and stubs PATH inside a subshell -- so no
# two share a path or an ordering assumption. Serially the suite was ~3s of
# mostly waiting on forks, which is the shape that overlaps well.
#
# Output is buffered per file and printed in the sequential order afterwards,
# not streamed: interleaved live output from seven scripts is unreadable, and a
# stable order keeps a CI log diffable against the run before it. The cost is
# that nothing prints until the suite is done, which at this size is ~1s.
#
# Spawned unbounded, deliberately: this is seven short, fork-heavy scripts, and
# a job-slot mechanism in plain bash would be more machinery than the thing it
# schedules. Revisit if tests/ grows an order of magnitude.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

OUT=$(mktemp -d "${TMPDIR:-/tmp}/nudge-suite.XXXXXX")
trap 'rm -rf "$OUT"' EXIT

for t in "$HERE"/test_*.sh; do
    n=$(basename "$t")
    # The status goes to a file rather than through `wait`: the reporting loop
    # below runs in file order, not completion order, so it cannot use `$!`.
    { bash "$t" >"$OUT/$n.out" 2>&1; printf '%s' "$?" >"$OUT/$n.rc"; } &
done
wait

rc=0
for t in "$HERE"/test_*.sh; do
    n=$(basename "$t")
    printf '\n### %s\n' "$n"
    cat "$OUT/$n.out"
    # A missing or empty .rc means the subshell died without writing one (killed,
    # or the disk filled). Treating that as a pass would turn a lost test into a
    # green suite, so it counts as a failure.
    { [ -s "$OUT/$n.rc" ] && [ "$(cat "$OUT/$n.rc")" -eq 0 ]; } || rc=1
done

printf '\n=== suite %s ===\n' "$([ "$rc" -eq 0 ] && echo PASSED || echo FAILED)"
exit "$rc"

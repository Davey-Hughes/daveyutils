#!/usr/bin/env bash
# Meta-test: a nudge SCHEDULING REGRESSION must FAIL test_jobs_e2e.sh, never
# silently skip it.
#
# test_jobs_e2e.sh decided skippability by running nudge and grepping its stdout
# for "Job ID: N". ANY regression in at_pipe, at_schedule_epoch,
# finalize_schedule's success message, the -q $AT_QUEUE handling, or the
# -p/-m/-i/-n parse branches makes that grep empty -- whereupon it printed
# "SKIP: environment can't queue an 'at' job (no id returned)" and exited 0.
# run.sh saw 0 and printed "=== suite PASSED ===", so all ~25 of its checks
# (--list-plain, list_jobs, F5, --preview-job, F1, F4, --edit, F3, --cancel)
# vanished without a single FAIL line -- CI green while nudge could not schedule
# at all. Highest risk on macOS, precisely the platform that file exists to cover.
#
# We stage a copy of the tree whose nudge cannot invoke `at` (at_pipe sabotaged)
# while the environment's REAL `at` still works perfectly, then assert the e2e
# file reports a failure rather than excusing itself.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"

for b in at atq atrm; do
    if ! command -v "$b" >/dev/null 2>&1; then
        echo "  SKIP: '$b' not installed"
        exit 0
    fi
done

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/e2e-skip.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT
mkdir -p "$WORKDIR/tests" "$WORKDIR/scripts"
cp "$HERE/assert.sh" "$HERE/lib.sh" "$HERE/test_jobs_e2e.sh" "$WORKDIR/tests/"

# Sabotage every `at` invocation inside at_pipe -- and only those, so nothing is
# ever queued and no at job can leak. From the outside this is indistinguishable
# from any other "nudge can't schedule" regression.
sed 's/| at -q /| at_sabotaged_regression -q /g' "$HERE/../scripts/nudge" \
    > "$WORKDIR/scripts/nudge"
chmod +x "$WORKDIR/scripts/nudge"

# Guards: if the sabotage stopped applying (nudge refactored), this test proves
# nothing -- so fail loudly here rather than silently passing.
check "sabotage patched all 3 at_pipe call sites" "3" \
    "$(grep -c 'at_sabotaged_regression -q ' "$WORKDIR/scripts/nudge" | tr -d ' ')"
check "sabotaged nudge returns no job id" "yes" \
    "$("$WORKDIR/scripts/nudge" -p 'x:0.0' -m '23:59' -i probe -n 2>/dev/null \
        | grep -q 'Job ID:' && echo no || echo yes)"
check "the real 'at' still works here (so this is a nudge fault)" "yes" \
    "$(pid=$(echo true | at -q w now + 2 hours 2>&1 | grep -oE 'job [0-9]+' | grep -oE '[0-9]+'); \
       [ -n "$pid" ] && { atrm "$pid" 2>/dev/null; echo yes; } || echo no)"

out=$(bash "$WORKDIR/tests/test_jobs_e2e.sh" 2>&1)
rc=$?

check "a regressed nudge FAILS the e2e file (not exit 0)" "yes" \
    "$([ "$rc" -ne 0 ] && echo yes || echo no)"
check "a regressed nudge is not excused as an unusable environment" "yes" \
    "$(printf '%s' "$out" | grep -q "can't queue an 'at' job" && echo no || echo yes)"

# --- the GENUINE skip must survive --------------------------------------------
# An `at` that exists but refuses to queue (at.deny, no atd permission, a
# container without a spool) is a real environment limit, not a nudge bug: the
# file must still skip cleanly rather than failing. Guards against over-fixing.
mkdir -p "$WORKDIR/fakebin"
cat > "$WORKDIR/fakebin/at" <<'FAKE'
#!/usr/bin/env bash
echo "You do not have permission to use at." >&2
exit 1
FAKE
chmod +x "$WORKDIR/fakebin/at"
# Restore the PRISTINE nudge: the point here is an unusable `at`, not a bad nudge.
cp "$HERE/../scripts/nudge" "$WORKDIR/scripts/nudge"

skip_out=$(PATH="$WORKDIR/fakebin:$PATH" bash "$WORKDIR/tests/test_jobs_e2e.sh" 2>&1)
skip_rc=$?
check "an unusable 'at' still skips cleanly (exit 0)" "0" "$skip_rc"
check "an unusable 'at' reports the skip" "yes" \
    "$(printf '%s' "$skip_out" | grep -q "SKIP: environment can't queue an 'at' job" \
        && echo yes || echo no)"

finish

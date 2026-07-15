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
# Every real `at` job this file stages. Each id is assigned BEFORE its inline
# atrm and kept out of any command substitution so this trap can still reap it:
# the id must outlive the subshell that created it, or a failing atrm (or a
# SIGINT landing in the window) orphans a real job in the user's queue. Same
# contract test_jobs_e2e.sh's remember_id/purge keeps -- see test_e2e_at_hygiene.sh.
PROBE_ID=""   # the "real at still works here" probe
QS_ID=""      # the queue-scoped sabotage's queue-'w' job
trap 'rm -rf "$WORKDIR"; for _id in $PROBE_ID $QS_ID; do atrm "$_id" 2>/dev/null; done' EXIT
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
PROBE_ID=$(echo true | at -q w now + 2 hours 2>&1 | grep -oE 'job [0-9]+' | grep -oE '[0-9]+')
check "the real 'at' still works here (so this is a nudge fault)" "yes" \
    "$([ -n "$PROBE_ID" ] && echo yes || echo no)"
# The EXIT trap backs this up: if it fails here the job is still reaped.
atrm "$PROBE_ID" 2>/dev/null

out=$(bash "$WORKDIR/tests/test_jobs_e2e.sh" 2>&1)
rc=$?

check "a regressed nudge FAILS the e2e file (not exit 0)" "yes" \
    "$([ "$rc" -ne 0 ] && echo yes || echo no)"
check "a regressed nudge is not excused as an unusable environment" "yes" \
    "$(printf '%s' "$out" | grep -q "can't queue an 'at' job" && echo no || echo yes)"

# --- a QUEUE-SCOPED regression must fail too -----------------------------------
# The whole-file skip is covered above, but F1/F4 staged their "foreign" jobs by
# grepping NUDGE's stdout for an id and skipped themselves when it came back
# empty. A regression that breaks only the OTHER queues (v/u) leaves the file's
# own queue 'w' working, so every w-based check still passes: the file printed
# "SKIP: F1 -- couldn't stage a job in queue 'v'" plus the same for F4, ran none
# of those 3 checks, and still exited 0 -- suite PASSED with a broken nudge.
# The sabotage above cannot catch this: it breaks EVERY queue, so the main ID
# empties first and the file fails for the other reason.
#
# Here `at` works for queue w and fails for anything else -- from the outside,
# a nudge that can only schedule on one queue.
awk '
  NR==1 {
      print
      print "at_queue_scoped_regression() { if [ \"$2\" = w ]; then command at \"$@\"; else return 1; fi; }"
      next
  }
  { gsub(/\| at -q /, "| at_queue_scoped_regression -q "); print }
' "$HERE/../scripts/nudge" > "$WORKDIR/scripts/nudge"
chmod +x "$WORKDIR/scripts/nudge"

# Guards: the sabotage must be BOTH applied and genuinely queue-scoped, else this
# proves nothing (or silently degenerates into the total sabotage above).
check "queue-scoped sabotage patched all 3 at_pipe call sites" "3" \
    "$(grep -c 'at_queue_scoped_regression -q ' "$WORKDIR/scripts/nudge" | tr -d ' ')"
QS_ID=$(NUDGE_AT_QUEUE=w "$WORKDIR/scripts/nudge" -p 'x:0.0' -m '23:58' -i probe -n 2>/dev/null \
    | grep -oE 'Job ID: [0-9]+' | grep -oE '[0-9]+')
check "queue-scoped sabotage: queue 'w' still schedules" "yes" \
    "$([ -n "$QS_ID" ] && echo yes || echo no)"
# The EXIT trap backs this up: if it fails here the job is still reaped.
atrm "$QS_ID" 2>/dev/null
check "queue-scoped sabotage: queue 'v' does not" "yes" \
    "$(NUDGE_AT_QUEUE=v "$WORKDIR/scripts/nudge" -p 'x:0.0' -m '23:58' -i probe -n 2>/dev/null \
        | grep -q 'Job ID:' && echo no || echo yes)"

qs_out=$(bash "$WORKDIR/tests/test_jobs_e2e.sh" 2>&1)
qs_rc=$?
check "a queue-scoped regression FAILS the e2e file (not exit 0)" "yes" \
    "$([ "$qs_rc" -ne 0 ] && echo yes || echo no)"
check "F1 is not silently skipped when 'at' can queue in 'v'" "yes" \
    "$(printf '%s' "$qs_out" | grep -q "SKIP: F1" && echo no || echo yes)"
check "F4 is not silently skipped when 'at' can queue in 'u'" "yes" \
    "$(printf '%s' "$qs_out" | grep -q "SKIP: F4" && echo no || echo yes)"

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

# --- an `at` that QUEUES but whose id won't parse is a fault, not a skip -------
# The probe only knows an id it can grep out of at's "job N at ..." line. If that
# message ever changes shape, the grep comes back empty -- which the skip branch
# read as "this environment can't queue", printed the SKIP and exited 0. But at
# exiting 0 means it ACCEPTED the job: a REAL job is now sitting in the user's
# queue and nothing can reap it (remember_id "" is a no-op). Report that instead
# of excusing it. The exit status is what separates the two cases -- a refusing
# `at` writes to stderr too, so "raw output non-empty" would also fire on the
# genuine skip above and break it.
cat > "$WORKDIR/fakebin/at" <<'FAKE'
#!/usr/bin/env bash
cat >/dev/null            # swallow the job body; queue nothing (no real leak)
echo "warning: commands will be executed using /bin/sh" >&2
echo "job number 4242 at Wed Jul 15 23:59:00 2026" >&2   # not "job 4242 at ..."
exit 0                    # ... but claim success: at accepted the job
FAKE
chmod +x "$WORKDIR/fakebin/at"

unparsed_out=$(PATH="$WORKDIR/fakebin:$PATH" bash "$WORKDIR/tests/test_jobs_e2e.sh" 2>&1)
unparsed_rc=$?
check "an unparsable 'at' id FAILS the e2e file (not exit 0)" "yes" \
    "$([ "$unparsed_rc" -ne 0 ] && echo yes || echo no)"
check "an unparsable 'at' id is not excused as an unusable environment" "yes" \
    "$(printf '%s' "$unparsed_out" | grep -q "SKIP: environment can't queue" && echo no || echo yes)"
check "an unparsable 'at' id is reported as such" "yes" \
    "$(printf '%s' "$unparsed_out" | grep -q 'no parsable id' && echo yes || echo no)"

finish

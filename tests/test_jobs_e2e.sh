#!/usr/bin/env bash
# End-to-end tests for the job-management CLI (--list/--preview-job/--edit/
# --cancel) against a REAL `at` spool. These are the only tests that exercise the
# actual `atq` output format, so they're what verifies the parsing on macOS/BSD
# (where atq's columns differ from GNU) -- the unit tests in test_jobs.sh use
# synthetic dumps and can't catch a real-format mismatch.
#
# The `at` DAEMON need not be running: we only schedule, inspect and remove jobs,
# never wait for one to fire. Skipped cleanly if at/atq/atrm are missing or the
# environment won't let us queue a job at all.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

for b in at atq atrm; do
    if ! command -v "$b" >/dev/null 2>&1; then
        echo "  SKIP: '$b' not installed"
        exit 0
    fi
done

# Isolate every test job in a throwaway queue so we never touch the user's real
# nudge queue ('n'). NUDGE_AT_QUEUE steers the "$NUDGE" subprocesses and our own
# cleanup; AT_QUEUE steers the helper functions we call directly (lib.sh already
# froze that global to 'n' when it sourced the script, before this export).
export NUDGE_AT_QUEUE=w
AT_QUEUE=w
# Extra throwaway queues stage "foreign" jobs (F1 preview guard, F4 cross-queue
# note). We purge ONLY the ids this test creates -- NOT a blanket sweep of
# queues w/v/u -- because a real `at -q w|v|u` job belonging to the user (not
# this test) must never be deleted just for sharing a queue letter.
CREATED_IDS=""
remember_id() { [ -n "$1" ] && CREATED_IDS="$CREATED_IDS $1"; }
purge() {
    local id
    for id in $CREATED_IDS; do atrm "$id" 2>/dev/null; done
    CREATED_IDS=""
}
trap 'purge; rm -f "$PRELUDE"' EXIT

# Decide skippability by probing `at` DIRECTLY -- never through nudge. Grepping
# nudge's own output for "Job ID: N" could not tell an unusable `at` from a
# REGRESSED nudge: any break in at_pipe, at_schedule_epoch, finalize_schedule's
# success message, -q handling or the -p/-m/-i/-n parse branches emptied the grep
# and silently disabled this entire file, exit 0, suite still "PASSED".
#
# probe_queue <queue> -- can `at` ITSELF queue a job in <queue> here?
#   0 = yes, and PROBE_ID parsed (so we could reap it again)
#   1 = `at` refused: nothing was queued, a real environment limit -> skippable
#   2 = `at` ACCEPTED but printed no id we can parse -> NOT skippable, and a
#       failure has already been recorded here (see below)
#
# The payload is `true` and we remove it immediately, so it's harmless even where
# BSD `at` drops the relative offset and schedules it for ~now. remember_id backs
# up the atrm in case the latter fails, so the EXIT trap still reaps it -- but
# only once the id parsed: remember_id "" is a no-op. So an `at` that queues a
# job whose id we can't grep out leaves a REAL job in the user's queue that
# nothing can reap; skipping there would exit 0 and abandon it. at's exit status
# is what separates that from a refusal (a refusing `at` writes to stderr too, so
# "raw output non-empty" would misfire on the genuine, skippable case).
PROBE_ID=""
PROBE_RAW=""
probe_queue() {
    local rc
    PROBE_RAW=$(echo true | at -q "$1" now + 2 hours 2>&1)
    rc=$?
    PROBE_ID=$(printf '%s\n' "$PROBE_RAW" | grep -oE 'job [0-9]+' | grep -oE '[0-9]+')
    remember_id "$PROBE_ID"
    if [ -n "$PROBE_ID" ]; then
        atrm "$PROBE_ID" 2>/dev/null
        return 0
    fi
    [ "$rc" -ne 0 ] && return 1
    check "probe: 'at' accepted a job in queue '$1' but printed no parsable id" \
        "parsed" "unparsed -- at said: $PROBE_RAW"
    return 2
}

probe_queue "$AT_QUEUE"
case $? in
    0) ;;
    2)  echo "  (that job is now orphaned in queue '$AT_QUEUE' and nothing here can"
        echo "   reap it -- a fault to fix, not an environment to excuse; aborting)"
        finish ;;   # terminates non-zero: probe_queue already recorded the failure
    *)  echo "  SKIP: environment can't queue an 'at' job"
        exit 0 ;;
esac

# Schedule one job; echo its numeric id (empty if scheduling failed).
schedule() { "$NUDGE" "$@" 2>/dev/null | grep -oE 'Job ID: [0-9]+' | grep -oE '[0-9]+'; }

FAKE_PANE="e2e:0.0"
ID=$(schedule -p "$FAKE_PANE" -m '23:59' -i 'msg one' -i "it's two" -n)
remember_id "$ID"

# `at` demonstrably works here, so an empty ID is a REAL nudge regression rather
# than an unusable environment: report it as a failure instead of excusing it.
check "schedule: nudge queued a job and reported its id" "yes" \
    "$([ -n "$ID" ] && echo yes || echo no)"
if [ -z "$ID" ]; then
    echo "  (every check below needs that job; aborting this file)"
    finish   # terminates non-zero: the check above already recorded the failure
fi

# --- --list-plain: real atq -> our row parser (the macOS-format check) ---------
plain=$("$NUDGE" --list-plain)
check "list: our job id present"   "yes" "$(printf '%s\n' "$plain" | awk '{print $1}' | grep -qx "$ID" && echo yes || echo no)"
check "list: pane recovered (not '?')" "yes" "$(printf '%s' "$plain" | grep -q "$FAKE_PANE" && echo yes || echo no)"
check "list: message count shown"  "yes" "$(printf '%s' "$plain" | grep -q '2 msg' && echo yes || echo no)"

# --- list_jobs table also parses (fzf-independent path, called directly) --------
table=$(list_jobs)
check "table: job id present"      "yes" "$(printf '%s' "$table" | grep -q "$ID" && echo yes || echo no)"
check "table: pane recovered"      "yes" "$(printf '%s' "$table" | grep -q "$FAKE_PANE" && echo yes || echo no)"
check "table: fire time not blank" "yes" "$(printf '%s' "$table" | grep -qE '[0-9]{2}:[0-9]{2}' && echo yes || echo no)"

# --- F5: piped / non-TTY --list must use the plain table, not the fzf dashboard.
# Dispatching on `command -v fzf` alone made a piped/cron `--list` print only
# fzf's "inappropriate ioctl" error; it now also requires stdin+stdout to be TTYs.
piped=$("$NUDGE" --list </dev/null | cat)
check "F5: piped --list shows the job id (plain table)" "yes" \
    "$(printf '%s\n' "$piped" | awk '{print $1}' | grep -qx "$ID" && echo yes || echo no)"

# --- --preview-job: recovers pane + both messages (round-trips the apostrophe) --
prev=$("$NUDGE" --preview-job "$ID")
check "preview: pane"        "yes" "$(printf '%s' "$prev" | grep -q "Pane:.*$FAKE_PANE" && echo yes || echo no)"
check "preview: msg 1"       "yes" "$(printf '%s' "$prev" | grep -qF 'msg one' && echo yes || echo no)"
check "preview: msg 2 apostrophe" "yes" "$(printf '%s' "$prev" | grep -qF "it's two" && echo yes || echo no)"
check "preview: notify option" "yes" "$(printf '%s' "$prev" | grep -q 'notify' && echo yes || echo no)"

# --- F1: --preview-job must validate queue membership before the at -c eval -----
# A nudge-shaped job in ANOTHER queue must NOT be rendered (its at -c body would
# otherwise reach job_detail's eval unguarded); previewing it shows "not found".
#
# Staging feasibility is decided by probing `at -q v` DIRECTLY, for the same
# reason the whole-file skip is: an empty F1ID from grepping NUDGE's stdout could
# not tell "this environment has no usable queue 'v'" from "nudge can no longer
# schedule onto another queue". A regression scoped to the non-'w' queues left
# every w-based check passing and quietly dropped F1/F4 -- 23 passed, 0 failed,
# exit 0. Once `at` is known to work for the queue, an empty F1ID is a REAL
# regression, so report it via check and let the file fail.
probe_queue v; f1_probe=$?
if [ "$f1_probe" -eq 0 ]; then
    F1ID=$(NUDGE_AT_QUEUE=v "$NUDGE" -p 'foreign:1.1' -m '23:57' -i 'secret leak' 2>/dev/null \
        | grep -oE 'Job ID: [0-9]+' | grep -oE '[0-9]+')
    remember_id "$F1ID"
    check "F1: nudge staged a job in queue 'v' (at itself can)" "yes" \
        "$([ -n "$F1ID" ] && echo yes || echo no)"
    if [ -n "$F1ID" ]; then
        f1prev=$("$NUDGE" --preview-job "$F1ID")
        check "F1: foreign-queue job not previewed" "yes" \
            "$(printf '%s' "$f1prev" | grep -q 'not found or not a nudge job' && echo yes || echo no)"
        check "F1: foreign pane not leaked into preview" "yes" \
            "$(printf '%s' "$f1prev" | grep -q 'foreign:1.1' && echo no || echo yes)"
        atrm "$F1ID" 2>/dev/null
    fi
elif [ "$f1_probe" -eq 1 ]; then
    echo "  SKIP: F1 -- 'at' itself cannot queue in queue 'v' here"
fi   # probe 2: probe_queue already recorded the failure; nothing to excuse

# --- F4: nudge jobs OUTSIDE our queue (e.g. an older nudge on the default queue)
# are invisible to the table and un-cancellable; list_jobs now notes them. Call
# list_jobs directly (like the table test above) to isolate the note from F5.
# Same direct-probe rule as F1: once `at -q u` is known to work, an empty F4ID is
# a nudge regression, not an environment limit.
probe_queue u; f4_probe=$?
if [ "$f4_probe" -eq 0 ]; then
    F4ID=$(NUDGE_AT_QUEUE=u "$NUDGE" -p 'legacy:2.2' -m '23:56' -i 'old job' 2>/dev/null \
        | grep -oE 'Job ID: [0-9]+' | grep -oE '[0-9]+')
    remember_id "$F4ID"
    check "F4: nudge staged a job in queue 'u' (at itself can)" "yes" \
        "$([ -n "$F4ID" ] && echo yes || echo no)"
    if [ -n "$F4ID" ]; then
        f4list=$(list_jobs)
        check "F4: --list notes an out-of-queue nudge job by id" "yes" \
            "$(printf '%s' "$f4list" | grep -q "outside queue '$AT_QUEUE'" \
                && printf '%s' "$f4list" | grep -qw "$F4ID" && echo yes || echo no)"
        atrm "$F4ID" 2>/dev/null
    fi
elif [ "$f4_probe" -eq 1 ]; then
    echo "  SKIP: F4 -- 'at' itself cannot queue in queue 'u' here"
fi   # probe 2: probe_queue already recorded the failure; nothing to excuse

# --- --edit (non-interactive): overlay a flag, keep the rest, swap the id -------
NEW=$("$NUDGE" --edit "$ID" -i 'edited msg' 2>/dev/null | grep -oE 'new job #[0-9]+' | grep -oE '[0-9]+')
remember_id "$NEW"
check "edit: produced a new id"  "yes" "$([ -n "$NEW" ] && [ "$NEW" != "$ID" ] && echo yes || echo no)"
check "edit: old id gone"        "yes" "$(atq -q "$NUDGE_AT_QUEUE" | awk '{print $1}' | grep -qx "$ID" && echo no || echo yes)"
prev2=$("$NUDGE" --preview-job "${NEW:-0}")
check "edit: new message applied" "yes" "$(printf '%s' "$prev2" | grep -qF 'edited msg' && echo yes || echo no)"
check "edit: original pane kept"  "yes" "$(printf '%s' "$prev2" | grep -q "$FAKE_PANE" && echo yes || echo no)"

# --- F3: `at` rejecting the time must fail loudly, not print a false "Done!" ----
# at_pipe's exit status IS at's; finalize_schedule ignored it and fell through to
# "Done!" on a garbled time (exit 0, nothing queued). It must now fail non-zero.
f3before=$(atq -q "$NUDGE_AT_QUEUE" | wc -l | tr -d ' ')
f3out=$("$NUDGE" -p "$FAKE_PANE" -m 'garbagetime' -i x 2>/dev/null)
f3rc=$?
f3after=$(atq -q "$NUDGE_AT_QUEUE" | wc -l | tr -d ' ')
check "F3: garbage -m exits non-zero"    "yes" "$([ "$f3rc" -ne 0 ] && echo yes || echo no)"
check "F3: no false 'Done!' on stdout"   "yes" "$(printf '%s' "$f3out" | grep -q 'Done!' && echo no || echo yes)"
check "F3: nothing queued by a bad time" "$f3before" "$f3after"

# --- guards: --cancel/--edit must not swallow a following flag as the id --------
before=$(atq -q "$NUDGE_AT_QUEUE" | wc -l | tr -d ' ')
"$NUDGE" --cancel -m 15:00 >/dev/null 2>&1
rc_flag=$?
"$NUDGE" --cancel >/dev/null 2>&1
rc_none=$?
after=$(atq -q "$NUDGE_AT_QUEUE" | wc -l | tr -d ' ')
check "guard: --cancel -m errors (non-zero)" "yes" "$([ "$rc_flag" -ne 0 ] && echo yes || echo no)"
check "guard: --cancel no-id errors"         "yes" "$([ "$rc_none" -ne 0 ] && echo yes || echo no)"
check "guard: no jobs removed by bad cancel"  "$before" "$after"

# --- --cancel: removes exactly the target job ----------------------------------
"$NUDGE" --cancel "${NEW:-0}" >/dev/null 2>&1
check "cancel: job removed" "yes" "$(atq -q "$NUDGE_AT_QUEUE" | awk '{print $1}' | grep -qx "${NEW:-0}" && echo no || echo yes)"

finish

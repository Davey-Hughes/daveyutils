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
# note); purged alongside the main one so nothing leaks between runs.
TEST_QUEUES="w v u"
purge() {
    local qq
    for qq in $TEST_QUEUES; do
        atrm $(atq -q "$qq" 2>/dev/null | awk '{print $1}') 2>/dev/null
    done
}
trap 'purge; rm -f "$PRELUDE"' EXIT
purge

# Schedule one job; echo its numeric id (empty if scheduling failed).
schedule() { "$NUDGE" "$@" 2>/dev/null | grep -oE 'Job ID: [0-9]+' | grep -oE '[0-9]+'; }

FAKE_PANE="e2e:0.0"
ID=$(schedule -p "$FAKE_PANE" -m '23:59' -i 'msg one' -i "it's two" -n)

if [ -z "$ID" ]; then
    echo "  SKIP: environment can't queue an 'at' job (no id returned)"
    exit 0
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
F1ID=$(NUDGE_AT_QUEUE=v "$NUDGE" -p 'foreign:1.1' -m '23:57' -i 'secret leak' 2>/dev/null \
    | grep -oE 'Job ID: [0-9]+' | grep -oE '[0-9]+')
if [ -n "$F1ID" ]; then
    f1prev=$("$NUDGE" --preview-job "$F1ID")
    check "F1: foreign-queue job not previewed" "yes" \
        "$(printf '%s' "$f1prev" | grep -q 'not found or not a nudge job' && echo yes || echo no)"
    check "F1: foreign pane not leaked into preview" "yes" \
        "$(printf '%s' "$f1prev" | grep -q 'foreign:1.1' && echo no || echo yes)"
    atrm "$F1ID" 2>/dev/null
else
    echo "  SKIP: F1 -- couldn't stage a job in queue 'v'"
fi

# --- F4: nudge jobs OUTSIDE our queue (e.g. an older nudge on the default queue)
# are invisible to the table and un-cancellable; list_jobs now notes them. Call
# list_jobs directly (like the table test above) to isolate the note from F5.
F4ID=$(NUDGE_AT_QUEUE=u "$NUDGE" -p 'legacy:2.2' -m '23:56' -i 'old job' 2>/dev/null \
    | grep -oE 'Job ID: [0-9]+' | grep -oE '[0-9]+')
if [ -n "$F4ID" ]; then
    f4list=$(list_jobs)
    check "F4: --list notes an out-of-queue nudge job by id" "yes" \
        "$(printf '%s' "$f4list" | grep -q "outside queue '$AT_QUEUE'" \
            && printf '%s' "$f4list" | grep -qw "$F4ID" && echo yes || echo no)"
    atrm "$F4ID" 2>/dev/null
else
    echo "  SKIP: F4 -- couldn't stage a job in queue 'u'"
fi

# --- --edit (non-interactive): overlay a flag, keep the rest, swap the id -------
NEW=$("$NUDGE" --edit "$ID" -i 'edited msg' 2>/dev/null | grep -oE 'new job #[0-9]+' | grep -oE '[0-9]+')
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

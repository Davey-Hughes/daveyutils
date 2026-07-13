#!/usr/bin/env bash
# Unit tests for the job-management parsing helpers (no at/atq/tmux needed).
# These reverse what at_pipe/build_next_cmd produce, so we build a realistic
# `at -c` dump the same way at_pipe does and assert the parser recovers it.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# Wrap an inner nudge command (as build_next_cmd emits) the way at_pipe does,
# then bury it under a few env lines the way `at -c` prints them. The parser
# must recover the job from the LAST non-empty line regardless of the preamble.
mk_at_c() {
    local cmd="$1" escaped q
    escaped=${cmd//"'"/"'\''"}      # same '->'\'' escaping at_pipe applies
    q="'"
    printf 'SHELL=/bin/sh\numask 22\ncd /home/u || exit\n'
    printf 'export DISPLAY=:0; export DBUS_SESSION_BUS_ADDRESS=unix:/run/x; bash -c %s%s%s\n' \
        "$q" "$escaped" "$q"
}

# Build a representative job: two messages (one with an apostrophe + spaces to
# stress the printf %q / single-quote-escaping round-trip), notify + retries on.
SCRIPT_PATH=/usr/local/bin/nudge
TARGET_PANE="bot:0.1"
SEND_DELAY=0.75
NOTIFY=true
VERIFY=false
MESSAGES=("please continue" "it's done")
CMD=$(build_next_cmd 2)
RAW=$(mk_at_c "$CMD")

# --- job_inner_cmd: reconstructs the inner command, trimmed to the flags ------
inner=$(job_inner_cmd "$RAW")
check "inner: starts at --execute-nudge" "yes" \
    "$(printf '%s' "$inner" | grep -q '^--execute-nudge' && echo yes || echo no)"
check "inner: keeps -n flag"             "yes" \
    "$(printf '%s' "$inner" | grep -q -- ' -n' && echo yes || echo no)"
check "inner: drops the leading script path" "no" \
    "$(printf '%s' "$inner" | grep -q '/usr/local/bin/nudge' && echo yes || echo no)"
check "inner: non-nudge dump rejected"   "1" \
    "$(job_inner_cmd 'echo hello world' >/dev/null 2>&1; echo $?)"

# --- job_summary: pane<TAB>count for the table --------------------------------
check "summary: pane"  "bot:0.1" "$(job_summary "$RAW" | cut -f1)"
check "summary: count" "2"       "$(job_summary "$RAW" | cut -f2)"
check "summary: junk rejected" "1" "$(job_summary 'not a nudge job' >/dev/null 2>&1; echo $?)"

# --- job_detail: human block for the fzf preview / cancel confirmation --------
detail=$(job_detail "$RAW")
check "detail: pane line"        "yes" "$(printf '%s' "$detail" | grep -q 'Pane:.*bot:0.1' && echo yes || echo no)"
check "detail: options line"     "yes" "$(printf '%s' "$detail" | grep -qE 'Options:.*auto-retry\(2\).*notify' && echo yes || echo no)"
check "detail: msg 1"            "yes" "$(printf '%s' "$detail" | grep -qF '1. please continue' && echo yes || echo no)"
# The apostrophe survives the printf %q -> '\'' escaping -> reversal round-trip.
check "detail: msg 2 apostrophe" "yes" "$(printf '%s' "$detail" | grep -qF "2. it's done" && echo yes || echo no)"

# --- atq_time_str: format the ctime-style date atq prints (GNU + BSD alike) ----
check "atq_time: ctime -> HH:MM Mon DD" "14:30 Jul 12" \
    "$(atq_time_str '3	Sat Jul 12 14:30:00 2026 n davey')"
check "atq_time: BSD line (no user col)" "09:05 Dec 1" \
    "$(atq_time_str '17	Mon Dec  1 09:05:00 2025 n')"

# --- load_job: seed the scheduling globals from an at -c dump (for --edit) -----
# Run in a command-substitution subshell so load_job's global mutations don't
# leak into the parent (which would corrupt later cases), echoing the state.
state=$(load_job "$RAW"
    printf '%s|%s|%s|%s|%s|%s|%s' \
        "$TARGET_PANE" "${#MESSAGES[@]}" "${MESSAGES[1]:-}" \
        "$NOTIFY" "$VERIFY" "$AUTO_RETRY" "$RETRY_LIMIT")
IFS='|' read -r lpane lcount lmsg2 lnotify lverify lretry_on lretry <<< "$state"
check "load: pane"                    "bot:0.1"   "$lpane"
check "load: message count"           "2"         "$lcount"
check "load: message 2 round-trips"   "it's done" "$lmsg2"
check "load: notify on"               "true"      "$lnotify"
check "load: verify off"              "false"     "$lverify"
check "load: auto-retry on (had -r)"  "true"      "$lretry_on"
check "load: retry count"             "2"         "$lretry"
check "load: junk rejected" "1" "$(load_job 'not a nudge job' >/dev/null 2>&1; echo $?)"

# --- atq_ctime / ctime_to_epoch: recover a job's fire time (for --edit) --------
check "atq_ctime: pull date out of atq line" "Mon Aug 16 10:56:00 2027" \
    "$(atq_ctime '28	Mon Aug 16 10:56:00 2027 z davey')"
# Round-trip: ctime string -> epoch -> reformatted matches (GNU coreutils host).
ep=$(ctime_to_epoch 'Mon Aug 16 10:56:00 2027')
check "ctime_to_epoch: round-trips HH:MM" "10:56"      "$(format_epoch "${ep:-0}" '%H:%M')"
check "ctime_to_epoch: round-trips date"  "2027-08-16" "$(format_epoch "${ep:-0}" '%Y-%m-%d')"

# --- edit_has_flags: did the user pass any editable flag with --edit? ----------
# SET_* default to false (from the Defaults block). Each is set true only by its
# flag's parse branch, so this decides interactive vs non-interactive editing.
check "edit_has_flags: none -> 1"   "1" "$(edit_has_flags; echo $?)"
check "edit_has_flags: pane -> 0"   "0" "$(SET_PANE=true; edit_has_flags; echo $?)"
check "edit_has_flags: time -> 0"   "0" "$(SET_TIME=true; edit_has_flags; echo $?)"
check "edit_has_flags: retries -> 0" "0" "$(SET_RETRIES=true; edit_has_flags; echo $?)"

# --- apply_edit_overrides: overlay explicit flags onto the loaded job state ----
# Precondition: scheduling globals hold the LOADED job; EDIT_* hold the flag
# values; SET_* mark which flags were explicit. Only marked fields change.
r=$(TARGET_PANE=loaded:0 MESSAGES=(a b) NOTIFY=true VERIFY=false AUTO_RETRY=true RETRY_LIMIT=3
    SET_PANE=true EDIT_PANE=new:9
    apply_edit_overrides
    printf '%s|%s|%s|%s|%s|%s' "$TARGET_PANE" "${#MESSAGES[@]}" "$NOTIFY" "$VERIFY" "$AUTO_RETRY" "$RETRY_LIMIT")
check "override: pane only (rest kept)" "new:9|2|true|false|true|3" "$r"

r=$(TARGET_PANE=loaded:0 MESSAGES=(a b c) NOTIFY=true VERIFY=false AUTO_RETRY=false RETRY_LIMIT=2
    SET_MESSAGES=true EDIT_MESSAGES=(only)
    SET_NOTIFY=true EDIT_NOTIFY=false
    apply_edit_overrides
    printf '%s|%s|%s' "${#MESSAGES[@]}" "${MESSAGES[0]}" "$NOTIFY")
check "override: messages replaced + notify off" "1|only|false" "$r"

# An explicit -r re-enables auto-retry with the new count (mirrors the flag).
r=$(AUTO_RETRY=false RETRY_LIMIT=2
    SET_RETRIES=true EDIT_RETRIES=5
    apply_edit_overrides
    printf '%s|%s' "$AUTO_RETRY" "$RETRY_LIMIT")
check "override: -r implies auto-retry on" "true|5" "$r"

# --no-auto-retry (SET_AUTO_RETRY with EDIT_AUTO_RETRY=false) turns it off.
r=$(AUTO_RETRY=true RETRY_LIMIT=4
    SET_AUTO_RETRY=true EDIT_AUTO_RETRY=false
    apply_edit_overrides
    printf '%s' "$AUTO_RETRY")
check "override: --no-auto-retry disables" "false" "$r"

finish

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

# --- F10: ctime parsing must not depend on the user's locale -------------------
# atq always emits C-locale English names (at never calls setlocale), but BSD
# strptime honours LC_TIME -- a macOS session in de_DE.UTF-8 rejects "Sun"/"Mar"
# unless ctime_to_epoch forces the C locale. GNU date parses English names in
# any locale, so only the BSD path can fail. Self-skips when no German locale is
# installed (typical Linux CI); macOS ships de_DE.UTF-8, so CI covers the BSD
# path there. 2026-03-15 IS a Sunday, and both "Sun" and "Mar" differ in German
# ("So" / "Mär"), so a locale-sensitive parse cannot accidentally pass.
de=$(locale -a 2>/dev/null | grep -iE '^de_DE\.utf-?8$' | head -n 1)
if [ -n "$de" ]; then
    lep=$(LC_ALL="$de" bash -c 'source "'"$PRELUDE"'"
        ctime_to_epoch "Sun Mar 15 10:56:00 2026"')
    check "F10: C-locale atq date parsed under de_DE" "10:56 2026-03-15" \
        "$(format_epoch "${lep:-0}" '%H:%M %Y-%m-%d')"
else
    echo "  skip: F10 locale test (no de_DE.UTF-8 installed)"
fi

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

# --- F2: a payload ending in a dangling value-flag must not spin forever -------
# job_summary/job_detail/load_job walk args with `shift 2`; in bash `shift 2`
# with $#==1 is a FAILING no-op, so a nudge job in our queue whose payload ends
# in a bare -p/-i/-w/-r loops at 100% CPU (hanging --list / preview / --edit).
# `shift 2 || return 1` degrades it to a clean rejection (job_row shows "?").
run_guarded() {
    # Run "$@" under a short timeout if the host has one (stock macOS lacks
    # timeout(1)); post-fix there is no hang, so run directly when it's absent.
    if command -v timeout >/dev/null 2>&1; then timeout 5 "$@"
    elif command -v gtimeout >/dev/null 2>&1; then gtimeout 5 "$@"
    else "$@"; fi
}
BAD=$(mk_at_c '--execute-nudge -p bot:0.1 -i hi -p')
# Feed the dump via an env var (not $1): sourcing $PRELUDE also loads the arg
# parser, which would otherwise consume the dump and exit before the function
# runs. With no positional args the parser stays idle. Pre-fix these HANG (empty
# capture once timeout kills them); post-fix each returns non-zero cleanly.
for fn in job_summary job_detail load_job; do
    rc=$(run_guarded env BAD_DUMP="$BAD" bash -c \
        'source "'"$PRELUDE"'"; '"$fn"' "$BAD_DUMP" >/dev/null 2>&1; echo $?')
    check "F2: $fn rejects dangling flag (no hang)" "yes" \
        "$([ -n "$rc" ] && [ "$rc" != 0 ] && echo yes || echo no)"
done

# --- F8: --edit must preserve the ORIGINAL job's desktop-env export prefix -----
# Rescheduling goes through at_pipe, which re-captures DISPLAY/WAYLAND/DBUS from
# the EDITING shell -- edit over SSH and the replacement gets blank exports, so
# notify-send dies silently. load_job snapshots the prefix; at_pipe emits it
# verbatim instead of the live-captured line (empty prefix -> live capture).
mk_at_c_env() {
    # Like mk_at_c but with a KNOWN single-statement export prefix (arg 2) -- the
    # real Linux at_pipe shape: `export ...; bash -c '...'`.
    local cmd="$1" pfx="$2" escaped q
    escaped=${cmd//"'"/"'\''"}
    q="'"
    printf 'SHELL=/bin/sh\numask 22\ncd /home/u || exit\n'
    printf '%s bash -c %s%s%s\n' "$pfx" "$q" "$escaped" "$q"
}
KNOWN_ENV='export DISPLAY=:7 WAYLAND_DISPLAY=wayland-1 DBUS_SESSION_BUS_ADDRESS=unix:/run/user/1000/bus;'
f8pfx=$(load_job "$(mk_at_c_env "$CMD" "$KNOWN_ENV")"; printf '%s' "$PRESERVED_ENV_EXPORTS")
check "F8: load_job captures the export prefix" "$KNOWN_ENV" "$f8pfx"
# A dump with NO export prefix (Darwin-style / pre-export jobs) leaves it empty.
f8none=$(load_job "$(printf 'SHELL=/bin/sh\ncd /home/u || exit\nbash -c %s%s%s\n' "'" '--execute-nudge -p x -i hi' "'")"
    printf '%s' "$PRESERVED_ENV_EXPORTS")
check "F8: no-export dump -> empty prefix (live capture)" "" "$f8none"

# at_pipe emits the preserved prefix verbatim and IGNORES the live shell env
# (`at` stubbed to echo back what it would queue -- no daemon needed).
f8out=$(
    at() { cat; }
    AT_QUEUE=w; IS_DARWIN=false
    PRESERVED_ENV_EXPORTS='export DISPLAY=:4242 WAYLAND_DISPLAY= DBUS_SESSION_BUS_ADDRESS=unix:/x;'
    DISPLAY=':9999'
    at_pipe 'echo hi' -t 202601010000)
check "F8: at_pipe emits the preserved DISPLAY" "yes" \
    "$(printf '%s' "$f8out" | grep -q 'DISPLAY=:4242' && echo yes || echo no)"
check "F8: at_pipe drops the live editing DISPLAY" "yes" \
    "$(printf '%s' "$f8out" | grep -q 'DISPLAY=:9999' && echo no || echo yes)"
f8live=$(
    at() { cat; }
    AT_QUEUE=w; IS_DARWIN=false; PRESERVED_ENV_EXPORTS=''
    DISPLAY=':9999'
    at_pipe 'echo hi' -t 202601010000)
check "F8: empty prefix -> live capture" "yes" \
    "$(printf '%s' "$f8live" | grep -q 'DISPLAY=:9999' && echo yes || echo no)"

finish

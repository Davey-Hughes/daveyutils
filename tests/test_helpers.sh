#!/usr/bin/env bash
# Unit tests for the pure helper functions in `nudge` (no tmux/at needed).
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ESCB=$(printf '\033')

# --- env_bool ----------------------------------------------------------------
check "env_bool 1 -> true"      "true"  "$(env_bool 1)"
check "env_bool true -> true"   "true"  "$(env_bool true)"
check "env_bool YES -> true"    "true"  "$(env_bool YES)"
check "env_bool On -> true"     "true"  "$(env_bool On)"
check "env_bool 0 -> false"     "false" "$(env_bool 0)"
check "env_bool empty -> false" "false" "$(env_bool '')"
check "env_bool junk -> false"  "false" "$(env_bool banana)"

# --- options_summary ---------------------------------------------------------
check "summary: none" "none" \
    "$(AUTO_RETRY=false VERIFY=false NOTIFY=false options_summary)"
check "summary: auto-retry only" "auto-retry(2)" \
    "$(AUTO_RETRY=true RETRY_LIMIT=2 VERIFY=false NOTIFY=false options_summary)"
check "summary: verify + notify" "verify, notify" \
    "$(AUTO_RETRY=false VERIFY=true NOTIFY=true options_summary)"
check "summary: all three" "auto-retry(-1), verify, notify" \
    "$(AUTO_RETRY=true RETRY_LIMIT=-1 VERIFY=true NOTIFY=true options_summary)"

# --- pane_after_marker + detect_reset_epoch (the retry false-positive fix) ----
# Resumed session: stale banner sits ABOVE our injected marker -> excluded.
paneA=$(printf '%s\n' \
    '● Earlier work in progress' \
    '⏸ session limit reached · resets 3:00am' \
    '> please continue' \
    '● Sure, continuing the task now...')
recentA=$(pane_after_marker "$paneA" "please continue")
check "A: stale banner excluded" "yes" \
    "$(printf '%s' "$recentA" | grep -q '3:00am' && echo no || echo yes)"
check "A: no retry (empty detect)" "" "$(detect_reset_epoch "$recentA" 2>/dev/null)"

# Still limited: a fresh banner appears BELOW the marker -> detected.
paneB=$(printf '%s\n' \
    '⏸ session limit reached · resets 3:00am' \
    '> please continue' \
    '⏸ session limit reached · resets 5:00am')
recentB=$(pane_after_marker "$paneB" "please continue")
check "B: old banner excluded"  "yes" "$(printf '%s' "$recentB" | grep -q '3:00am' && echo no || echo yes)"
check "B: new banner included"  "yes" "$(printf '%s' "$recentB" | grep -q '5:00am' && echo yes || echo no)"
check "B: detected epoch is 5:00am reset (05:03 w/ pad)" "05:03" \
    "$(format_epoch "$(detect_reset_epoch "$recentB" 2>/dev/null)" '%H:%M')"

# Marker scrolled off (continue worked and pushed it away) -> no retry.
recentC=$(pane_after_marker "$(printf '%s\n' '● lots' '● of' '● output')" "please continue")
check "C: marker missing -> empty" "" "$recentC"

# ANSI-wrapped marker still matches after stripping.
paneD=$(printf '%s\n' \
    "${ESCB}[2m>${ESCB}[0m ${ESCB}[1mplease continue${ESCB}[0m" \
    '⏸ session limit reached · resets 5:00am')
recentD=$(pane_after_marker "$paneD" "please continue")
check "D: ANSI marker matched" "yes" "$(printf '%s' "$recentD" | grep -q '5:00am' && echo yes || echo no)"

# --- has_limit_banner (used by --verify) -------------------------------------
check "banner: Claude present"      "0" "$(has_limit_banner '⏸ session limit reached · resets 3:00am'; echo $?)"
check "banner: current-session var" "0" "$(has_limit_banner 'Your current session limit resets 11:00pm'; echo $?)"
check "banner: Agy quota present"   "0" "$(has_limit_banner 'quota reached — Resets in 1h30m'; echo $?)"
check "banner: clean pane absent"   "1" "$(has_limit_banner '● Running tests... all green'; echo $?)"

# Agy relative-duration path: "Resets in 2m" -> now + 120s + 180s pad = now+300.
now=$(date +%s)
agy=$(detect_reset_epoch "$(printf 'quota reached\nResets in 2m\n')" 2>/dev/null)
diff=$(( ${agy:-0} - now ))
check "Agy: relative reset ~ now+300s" "yes" \
    "$([ "$diff" -ge 290 ] && [ "$diff" -le 330 ] && echo yes || echo no)"

# --- normalize_clock: canonicalise a captured clock token for both date flavours
check "normclock: 3:00am -> 3:00AM"   "3:00AM"  "$(normalize_clock '3:00am')"
check "normclock: lowercase pm"       "11:59PM" "$(normalize_clock '11:59pm')"
check "normclock: no minutes -> :00"  "3:00PM"  "$(normalize_clock '3pm')"
check "normclock: space before ap"    "4:30PM"  "$(normalize_clock '4:30 PM')"
check "normclock: trailing timezone ignored" "9:00AM" "$(normalize_clock '9:00am (America/New_York)')"
check "normclock: no am/pm -> empty"  ""        "$(normalize_clock '14:30')"

# --- detect_reset_epoch: loosened clock parsing (minutes optional / lowercase) -
# "resets 3pm" (no minutes) must parse to 15:03 (with the 3m pad).
check "detect: bare-hour '3pm' -> 15:03" "15:03" \
    "$(format_epoch "$(detect_reset_epoch 'session limit reached, resets 3pm' 2>/dev/null)" '%H:%M')"
# Spaced, lowercase meridiem still parses.
check "detect: '4:30 pm' -> 16:33" "16:33" \
    "$(format_epoch "$(detect_reset_epoch 'current session resets at 4:30 pm' 2>/dev/null)" '%H:%M')"

# --- extensible patterns: NUDGE_CLOCK_PATTERN / NUDGE_DURATION_PATTERN ----------
# A foreign CLI's banner is undetected by default...
check "custom: unknown banner ignored by default" "" \
    "$(detect_reset_epoch 'FooCLI throttled until 2:00am' 2>/dev/null)"
# ...but detected once its clock-style pattern is registered (parsed as a time).
check "custom: NUDGE_CLOCK_PATTERN detects (02:03 w/ pad)" "02:03" \
    "$(NUDGE_CLOCK_PATTERN='FooCLI throttled' \
        format_epoch "$(NUDGE_CLOCK_PATTERN='FooCLI throttled' detect_reset_epoch 'FooCLI throttled until 2:00am' 2>/dev/null)" '%H:%M')"
# Duration-style extension: "cooldown 3m" via NUDGE_DURATION_PATTERN -> ~now+300s.
now=$(date +%s)
cust=$(NUDGE_DURATION_PATTERN='cooldown' detect_reset_epoch "$(printf 'cooldown active\nResets in 2m\n')" 2>/dev/null)
diff=$(( ${cust:-0} - now ))
check "custom: NUDGE_DURATION_PATTERN detects ~now+300s" "yes" \
    "$([ "$diff" -ge 290 ] && [ "$diff" -le 330 ] && echo yes || echo no)"
# has_limit_banner honours the same extension.
check "custom: has_limit_banner sees clock extension" "0" \
    "$(NUDGE_CLOCK_PATTERN='FooCLI throttled' has_limit_banner 'FooCLI throttled until 2:00am'; echo $?)"

# Duration countdown extraction is case-insensitive ("resets in" lowercase).
now=$(date +%s)
low=$(detect_reset_epoch "$(printf 'quota reached\nresets in 2m\n')" 2>/dev/null)
diff=$(( ${low:-0} - now ))
check "duration: lowercase 'resets in' parsed ~now+300s" "yes" \
    "$([ "$diff" -ge 290 ] && [ "$diff" -le 330 ] && echo yes || echo no)"

# --- build_next_cmd flag passthrough -----------------------------------------
cmd_v=$(SCRIPT_PATH=/x TARGET_PANE="s:0.0" SEND_DELAY=0.75 NOTIFY=false VERIFY=true bash -c '
    source "'"$PRELUDE"'"; SCRIPT_PATH=/x; TARGET_PANE="s:0.0"; SEND_DELAY=0.75
    NOTIFY=false; VERIFY=true; MESSAGES=("hi"); build_next_cmd 2')
check "build_next_cmd: -v when VERIFY on"  "yes" "$(printf '%s' "$cmd_v" | grep -q -- ' -v' && echo yes || echo no)"
check "build_next_cmd: -r embeds count"    "yes" "$(printf '%s' "$cmd_v" | grep -q -- ' -r 2' && echo yes || echo no)"

cmd_nov=$(bash -c 'source "'"$PRELUDE"'"; SCRIPT_PATH=/x; TARGET_PANE="s:0.0"; SEND_DELAY=0.75
    NOTIFY=true; VERIFY=false; MESSAGES=("hi"); build_next_cmd ""')
check "build_next_cmd: no -v when VERIFY off" "no"  "$(printf '%s' "$cmd_nov" | grep -q -- ' -v' && echo yes || echo no)"
check "build_next_cmd: -n when NOTIFY on"     "yes" "$(printf '%s' "$cmd_nov" | grep -q -- ' -n' && echo yes || echo no)"
check "build_next_cmd: no -r when limit empty" "no" "$(printf '%s' "$cmd_nov" | grep -q -- ' -r' && echo yes || echo no)"

# A script path containing a space must survive the bash -c round-trip: the
# emitted command, re-tokenised the way the 'at' job's bash -c does, must yield
# the full path back as $1 (else the job runs "/opt/my" with arg "dir/nudge").
first_arg=$(bash -c 'source "'"$PRELUDE"'"; SCRIPT_PATH="/opt/my dir/nudge"
    TARGET_PANE="s:0.0"; SEND_DELAY=0.75; NOTIFY=false; VERIFY=false; MESSAGES=("hi")
    cmd=$(build_next_cmd ""); eval "set -- $cmd"; printf "%s" "$1"')
check "build_next_cmd: spaced script path round-trips" "/opt/my dir/nudge" "$first_arg"

# --- prompt_options (driven via process substitution so mutations persist) ---
# toggle auto-retry + verify on; blank count keeps default 2
r=$(AUTO_RETRY=false VERIFY=false NOTIFY=false RETRY_LIMIT=2
    prompt_options < <(printf 'av\n\n') >/dev/null 2>&1
    printf '%s %s %s %s' "$AUTO_RETRY" "$VERIFY" "$NOTIFY" "$RETRY_LIMIT")
check "prompt: toggle a+v, keep count" "true true false 2" "$r"

# toggle notify only (auto-retry stays off -> no count prompt)
r=$(AUTO_RETRY=false VERIFY=false NOTIFY=false RETRY_LIMIT=2
    prompt_options < <(printf 'n\n') >/dev/null 2>&1
    printf '%s %s %s %s' "$AUTO_RETRY" "$VERIFY" "$NOTIFY" "$RETRY_LIMIT")
check "prompt: toggle notify only" "false false true 2" "$r"

# toggling auto-retry that was ON turns it OFF
r=$(AUTO_RETRY=true VERIFY=false NOTIFY=false RETRY_LIMIT=2
    prompt_options < <(printf 'a\n') >/dev/null 2>&1
    printf '%s %s %s %s' "$AUTO_RETRY" "$VERIFY" "$NOTIFY" "$RETRY_LIMIT")
check "prompt: toggle auto-retry off" "false false false 2" "$r"

# enable auto-retry and set a custom count
r=$(AUTO_RETRY=false VERIFY=false NOTIFY=false RETRY_LIMIT=2
    prompt_options < <(printf 'a\n5\n') >/dev/null 2>&1
    printf '%s %s %s %s' "$AUTO_RETRY" "$VERIFY" "$NOTIFY" "$RETRY_LIMIT")
check "prompt: auto-retry with count 5" "true false false 5" "$r"

# blank line keeps everything as-is
r=$(AUTO_RETRY=false VERIFY=true NOTIFY=false RETRY_LIMIT=2
    prompt_options < <(printf '\n') >/dev/null 2>&1
    printf '%s %s %s %s' "$AUTO_RETRY" "$VERIFY" "$NOTIFY" "$RETRY_LIMIT")
check "prompt: blank keeps state" "false true false 2" "$r"

# unknown letters ignored, non-integer count rejected (keeps prior)
r=$(AUTO_RETRY=false VERIFY=false NOTIFY=false RETRY_LIMIT=2
    prompt_options < <(printf 'axz\nnope\n') >/dev/null 2>&1
    printf '%s %s %s %s' "$AUTO_RETRY" "$VERIFY" "$NOTIFY" "$RETRY_LIMIT")
check "prompt: junk ignored, bad count kept" "true false false 2" "$r"

# --- is_relative_timespec (BSD `at` silently drops relative "+" offsets) ------
# Relative offsets contain a '+'; absolute / named times do not.
check "reltime: 'now + 45 min' relative"  "0" "$(is_relative_timespec 'now + 45 min'; echo $?)"
check "reltime: 'now + 1 hour' relative"  "0" "$(is_relative_timespec 'now + 1 hour'; echo $?)"
check "reltime: 'now +30minutes' relative" "0" "$(is_relative_timespec 'now +30minutes'; echo $?)"
check "reltime: '14:30' absolute"         "1" "$(is_relative_timespec '14:30'; echo $?)"
check "reltime: '1159pm' absolute"        "1" "$(is_relative_timespec '1159pm'; echo $?)"
check "reltime: 'noon' absolute"          "1" "$(is_relative_timespec 'noon'; echo $?)"
check "reltime: 'tomorrow 9am' absolute"  "1" "$(is_relative_timespec 'tomorrow 9am'; echo $?)"
check "reltime: empty absolute"           "1" "$(is_relative_timespec ''; echo $?)"

# --- atrun_hint (macOS-only schedule-time reminder; suppressible) -------------
# Forced IS_DARWIN so both branches are tested regardless of the host OS.
check "atrun_hint: macOS mentions atrun" "yes" \
    "$(IS_DARWIN=true NUDGE_NO_ATRUN_HINT= atrun_hint 2>&1 | grep -qi 'atrun' && echo yes || echo no)"
check "atrun_hint: macOS points at --help" "yes" \
    "$(IS_DARWIN=true NUDGE_NO_ATRUN_HINT= atrun_hint 2>&1 | grep -q -- '--help' && echo yes || echo no)"
check "atrun_hint: prints to stderr not stdout" "" \
    "$(IS_DARWIN=true NUDGE_NO_ATRUN_HINT= atrun_hint 2>/dev/null)"
check "atrun_hint: suppressed by NUDGE_NO_ATRUN_HINT=1" "" \
    "$(IS_DARWIN=true NUDGE_NO_ATRUN_HINT=1 atrun_hint 2>&1)"
check "atrun_hint: silent off macOS" "" \
    "$(IS_DARWIN=false atrun_hint 2>&1)"

# --- print_help documents the modern launchctl verbs -------------------------
help_mac=$(IS_DARWIN=true print_help)
check "help: 'launchctl enable' present"    "yes" "$(printf '%s' "$help_mac" | grep -q 'launchctl enable system/com.apple.atrun' && echo yes || echo no)"
check "help: 'launchctl bootstrap' present" "yes" "$(printf '%s' "$help_mac" | grep -q 'launchctl bootstrap system' && echo yes || echo no)"

finish

#!/usr/bin/env bash
# Option-resolution tests: env vars < CLI flags < (--no-* overrides).
# Output order from resolve_case is: NOTIFY AUTO_RETRY VERIFY RETRY_LIMIT
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# Scratch space for the F9 watchdog's stripped-down PATH (below). lib.sh set an
# EXIT trap to remove $PRELUDE; re-declare it rather than clobber it.
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/nudge-resolution.XXXXXX")
trap 'rm -f "$PRELUDE"; rm -rf "$WORKDIR"' EXIT

# --- baseline ----------------------------------------------------------------
check "defaults: nothing set" "false false false 2" "$(resolve_case)"

# --- env vars turn options on ------------------------------------------------
check "env: NUDGE_NOTIFY"      "true false false 2"  "$(NUDGE_NOTIFY=1 resolve_case)"
check "env: NUDGE_AUTO_RETRY"  "false true false 2"  "$(NUDGE_AUTO_RETRY=1 resolve_case)"
check "env: NUDGE_VERIFY"      "false false true 2"  "$(NUDGE_VERIFY=yes resolve_case)"
check "env: NUDGE_RETRIES sets count + implies auto-retry" \
    "false true false 5" "$(NUDGE_RETRIES=5 resolve_case)"
check "env: bad NUDGE_RETRIES ignored (keeps 2, no auto-retry)" \
    "false false false 2" "$(NUDGE_RETRIES=abc resolve_case)"
check "env: all on" "true true true 3" \
    "$(NUDGE_NOTIFY=1 NUDGE_VERIFY=1 NUDGE_RETRIES=3 resolve_case)"

# --- CLI flags override env (flags win) --------------------------------------
check "flag -v alone"          "false false true 2"  "$(resolve_case -v)"
check "flag -a alone"          "false true false 2"  "$(resolve_case -a)"
check "flag -r overrides env count" "false true false 3" \
    "$(NUDGE_RETRIES=9 resolve_case -r 3)"

# --- --no-* overrides turn a persistent default back off ---------------------
check "--no-verify beats NUDGE_VERIFY"        "false false false 2" "$(NUDGE_VERIFY=1 resolve_case --no-verify)"
check "--no-auto-retry beats NUDGE_AUTO_RETRY" "false false false 2" "$(NUDGE_AUTO_RETRY=1 resolve_case --no-auto-retry)"
check "--no-notify beats NUDGE_NOTIFY"        "false false false 2" "$(NUDGE_NOTIFY=1 resolve_case --no-notify)"
check "--no-auto-retry beats NUDGE_RETRIES"   "false false false 5" "$(NUDGE_RETRIES=5 resolve_case --no-auto-retry)"

# --- headless --execute-nudge stays hermetic (env vars ignored) --------------
# The at job is invoked with --execute-nudge first; env must NOT leak in.
check "execute-nudge ignores NUDGE_VERIFY" "false false false 2" \
    "$(NUDGE_VERIFY=1 resolve_case --execute-nudge -p s:0.0)"
check "execute-nudge ignores NUDGE_NOTIFY" "false false false 2" \
    "$(NUDGE_NOTIFY=1 resolve_case --execute-nudge -p s:0.0)"

# --- the F9 guard's own watchdog ----------------------------------------------
# Portable watchdog: run "$@", kill it after <secs>, and report timeout(1)'s 124.
#
# Stock macOS -- the very platform this compatibility work targets -- ships
# neither timeout nor gtimeout, so "run it bare when timeout is absent" dropped
# the guard exactly where it matters most: an I15 regression would HANG CI
# forever rather than fail it, and a hang-instead-of-fail is a defect in its own
# right. bash 3.2 has no `wait -n`, so poll instead: bash reaps a finished
# background child promptly and `wait` still yields its status afterwards.
watchdog() {
    local secs="$1"; shift
    local waited=0 pid
    "$@" &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$secs" ]; then
            kill -TERM "$pid" 2>/dev/null
            sleep 1
            kill -KILL "$pid" 2>/dev/null   # backstop, in case TERM was ignored
            wait "$pid" 2>/dev/null
            return 124                      # the code timeout(1) reports
        fi
        sleep 1
        waited=$((waited + 1))
    done
    wait "$pid"
}

GUARD_SECS=${GUARD_SECS:-5}
run_guarded() {
    # Prefer the host's timeout(1); fall back to our own watchdog so the guard
    # holds on a stock macOS too.
    if command -v timeout >/dev/null 2>&1; then timeout "$GUARD_SECS" "$@"
    elif command -v gtimeout >/dev/null 2>&1; then gtimeout "$GUARD_SECS" "$@"
    else watchdog "$GUARD_SECS" "$@"; fi
}

# --- the watchdog itself, on the no-timeout(1) path ---------------------------
# Exercised via a PATH holding only `sleep` (the watchdog's poll and the guarded
# command both need it) and `bash`, so the fallback branch really is the one
# under test even on a host that does have timeout(1).
NOTIMEOUT_BIN="$WORKDIR/notimeout"
mkdir -p "$NOTIMEOUT_BIN"
ln -s "$(command -v sleep)" "$NOTIMEOUT_BIN/sleep"
ln -s "$(command -v bash)"  "$NOTIMEOUT_BIN/bash"
check "F9 watchdog: the test PATH really lacks timeout(1)/gtimeout" "yes" \
    "$(PATH="$NOTIMEOUT_BIN"
       if command -v timeout >/dev/null 2>&1 || command -v gtimeout >/dev/null 2>&1
       then echo no; else echo yes; fi)"
check "F9 watchdog: a hang is killed and reported, not waited out" "124" \
    "$(PATH="$NOTIMEOUT_BIN"; GUARD_SECS=1; run_guarded sleep 5 >/dev/null 2>&1; echo $?)"
check "F9 watchdog: a command that exits on its own keeps its own status" "3" \
    "$(PATH="$NOTIMEOUT_BIN"; GUARD_SECS=5; run_guarded bash -c 'exit 3' >/dev/null 2>&1; echo $?)"

# ... and the same on the real thing: a prelude whose require_value always passes
# IS the I15 regression -- `-p` with no value spins the parse loop forever. On a
# host without timeout(1) that has to FAIL, not hang the suite. This is what the
# checks above buy; `sleep` only proves the mechanism.
sed 's/-ge 2 \] && return 0/-ge 0 ] \&\& return 0/' "$PRELUDE" > "$WORKDIR/prelude-i15"
check "F9 watchdog: the I15 sabotage really does neuter require_value" "yes" \
    "$(grep -q '\[ "\$2" -ge 0 \] && return 0' "$WORKDIR/prelude-i15" && echo yes || echo no)"
check "F9 watchdog: an I15 regression is killed (124), not hung, without timeout(1)" "124" \
    "$(PATH="$NOTIMEOUT_BIN"; GUARD_SECS=1
       run_guarded bash -c 'source "$1" -p 2>&1' _ "$WORKDIR/prelude-i15" >/dev/null 2>&1; echo $?)"

# --- F9: a value-taking flag with its value omitted must fail, not spin --------
# `nudge -p` (forgetting the pane) leaves $#==1, and in bash `shift 2` with one
# arg left is a FAILING no-op -- it shifts nothing -- so `while [[ $# -gt 0 ]]`
# re-enters the same branch forever at 100% CPU. -i is worse: MESSAGES+=("$2")
# appends an empty element every iteration until the OOM killer steps in. The
# script's own payload walkers already guard this (test_jobs.sh F2); the CLI
# parser they mirror did not. Mirrors F2's run_guarded pattern so a regression
# FAILS here instead of hanging the suite forever.
#
# Source the prelude (which ends in the arg parser) with the dangling flag as its
# only argument, exactly as resolve_case does. Echoes "<rc>|<output>".
dangling() {
    local out rc
    out=$(run_guarded bash -c 'source "$1" "$2" 2>&1' _ "$PRELUDE" "$1")
    rc=$?
    printf '%s|%s' "$rc" "$out"
}

# -p/-m/-i hang pre-fix (timeout kills them -> rc 124); -w/-r escape only by
# accident, exiting 1 on their value regex rather than on the missing value --
# so assert on the message too, not just the exit code.
for f in -p --pane -m --time -i --input -w --delay -r --retries; do
    r=$(dangling "$f")
    check "F9: dangling $f exits 1 (no hang)" "1" "${r%%|*}"
    check "F9: dangling $f names the missing value" "yes" \
        "$(case "${r#*|}" in *"requires a value"*) echo yes ;; *) echo no ;; esac)"
done

finish

#!/usr/bin/env bash
# Option-resolution tests: env vars < CLI flags < (--no-* overrides).
# Output order from resolve_case is: NOTIFY AUTO_RETRY VERIFY RETRY_LIMIT
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

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

# --- F9: a value-taking flag with its value omitted must fail, not spin --------
# `nudge -p` (forgetting the pane) leaves $#==1, and in bash `shift 2` with one
# arg left is a FAILING no-op -- it shifts nothing -- so `while [[ $# -gt 0 ]]`
# re-enters the same branch forever at 100% CPU. -i is worse: MESSAGES+=("$2")
# appends an empty element every iteration until the OOM killer steps in. The
# script's own payload walkers already guard this (test_jobs.sh F2); the CLI
# parser they mirror did not. Mirrors F2's run_guarded/timeout pattern so a
# regression FAILS here instead of hanging the suite forever.
run_guarded() {
    # Run "$@" under a short timeout if the host has one (stock macOS lacks
    # timeout(1)); post-fix there is no hang, so run directly when it's absent.
    if command -v timeout >/dev/null 2>&1; then timeout 5 "$@"
    elif command -v gtimeout >/dev/null 2>&1; then gtimeout 5 "$@"
    else "$@"; fi
}

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

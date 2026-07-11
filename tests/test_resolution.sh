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

finish

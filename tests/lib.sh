#!/usr/bin/env bash
# Shared helpers for the nudge test-suite.
#
# Sourcing this file:
#   * defines check()/finish() assertions,
#   * loads the nudge helper functions into the current shell, and
#   * provides resolve_case() for exercising option-resolution (env/flags).

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
NUDGE="$HERE/../scripts/nudge"

# shellcheck disable=SC1091
source "$HERE/assert.sh"

# Extract the script's top matter -- tool resolution, helper functions, the
# env-var default block, and argument parsing -- but WITHOUT the tmux/at
# dependency check, so tests can source it (and thus resolve option state)
# without those binaries installed. Cut off right before the main logic.
# NB: an explicit XXXXXX template -- bare `mktemp` (no args) errors on BSD/macOS.
PRELUDE=$(mktemp "${TMPDIR:-/tmp}/nudge-prelude.XXXXXX")
awk '
  /^SCRIPT_PATH=/ { exit }
  /^# --- Dependency check/ { skip = 1 }
  /^# --- Defaults/ { skip = 0 }
  !skip { print }
' "$NUDGE" >"$PRELUDE"
trap 'rm -f "$PRELUDE"' EXIT

# Load the helper functions + baseline default state into the current shell.
# shellcheck disable=SC1090
source "$PRELUDE"

# Resolve the four option variables from env vars (set by the caller) and CLI
# args ("$@"), echoed space-separated as "NOTIFY AUTO_RETRY VERIFY RETRY_LIMIT".
# Runs in a subshell so each case starts from a clean slate.
resolve_case() {
    (
        # shellcheck disable=SC1090
        source "$PRELUDE" "$@" >/dev/null 2>&1
        printf '%s %s %s %s' "$NOTIFY" "$AUTO_RETRY" "$VERIFY" "$RETRY_LIMIT"
    )
}

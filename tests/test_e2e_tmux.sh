#!/usr/bin/env bash
# End-to-end tests for the headless `--execute-nudge` path (the command the `at`
# job runs) against a real tmux pane. Exercises the --verify gate. Needs the
# tmux + at binaries (the daemon does NOT need to be running: these cases never
# schedule anything). Skipped cleanly if either is missing.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if ! command -v tmux >/dev/null 2>&1 || ! command -v at >/dev/null 2>&1; then
    echo "  SKIP: tmux and/or at not installed"
    exit 0
fi

SESS="nudgetest_$$"
cleanup() { tmux kill-session -t "$SESS" 2>/dev/null; }
trap 'cleanup; rm -f "$PRELUDE"' EXIT

fresh_pane() { # start a pane running `cat` so keystrokes echo to screen, not execute
    cleanup
    tmux new-session -d -s "$SESS" -x 120 -y 40
    PANE=$(tmux list-panes -t "$SESS" -F '#{session_name}:#{window_index}.#{pane_index}')
    tmux send-keys -t "$PANE" 'cat' Enter
    sleep 0.3
}
injected() { tmux capture-pane -pt "$PANE" | grep -q "$1" && echo yes || echo no; }

# --- verify PASSES when a banner is present -> message injected ----------------
fresh_pane
tmux send-keys -t "$PANE" 'session limit reached, resets 3:00am' Enter
sleep 0.3
"$NUDGE" --execute-nudge -p "$PANE" -v -i 'INJECTED_ONE' >/dev/null 2>&1
sleep 0.5
check "verify PASS (banner up): injected" "yes" "$(injected INJECTED_ONE)"

# --- verify SKIPS when no banner -> nothing injected --------------------------
fresh_pane
tmux send-keys -t "$PANE" 'all good, working normally' Enter
sleep 0.3
"$NUDGE" --execute-nudge -p "$PANE" -v -i 'INJECTED_TWO' >/dev/null 2>&1
sleep 0.5
check "verify SKIP (no banner): not injected" "no" "$(injected INJECTED_TWO)"

# --- without --verify: always injects ----------------------------------------
"$NUDGE" --execute-nudge -p "$PANE" -i 'INJECTED_THREE' >/dev/null 2>&1
sleep 0.5
check "no --verify: injected unconditionally" "yes" "$(injected INJECTED_THREE)"

finish

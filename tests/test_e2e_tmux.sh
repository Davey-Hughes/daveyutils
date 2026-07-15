#!/usr/bin/env bash
# End-to-end tests for the headless `--execute-nudge` path (the command the `at`
# job runs) against a real tmux pane. Exercises the --verify gate. Needs the
# tmux + at binaries (the daemon does NOT need to be running: these cases never
# schedule anything). Skipped cleanly if either is missing.
#
# ISOLATION -- read before touching any tmux call here.
#
# This file drives a REAL tmux server. It used to drive the *default* one, which
# is whatever server the developer is sitting in: `tmux ls` here listed their
# sessions, and any bare `kill-server` reached them. That is not hypothetical --
# it destroyed a live session (with a running agent in it) during development.
#
# TMUX_TMPDIR relocates the default socket into a throwaway directory, and
# `unset TMUX` detaches us from any inherited server. `nudge` inherits both as a
# child process, so its own tmux calls land on the same private server without
# needing a socket flag. The upshot: no bare `tmux` in this file -- not even
# `kill-server` -- can reach the developer's sessions.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if ! command -v tmux >/dev/null 2>&1 || ! command -v at >/dev/null 2>&1; then
    echo "  SKIP: tmux and/or at not installed"
    exit 0
fi

TMUX_DIR=$(mktemp -d) || { echo "  SKIP: cannot make a private tmux dir"; exit 0; }
export TMUX_TMPDIR="$TMUX_DIR"
unset TMUX

SESS="nudgetest_$$"
cleanup() { tmux kill-server 2>/dev/null; }
trap 'cleanup; rm -rf "$TMUX_DIR"; rm -f "$PRELUDE"' EXIT

# The test helpers deliberately carry no `die`, and this file runs without
# `set -e`, so a guard has to terminate on its own -- a bare `|| die ...` would
# print "command not found" and then fall through into the very send-keys it
# exists to prevent.
fatal() { echo "  FAIL: $*" >&2; exit 1; }

fresh_pane() { # start a pane running `cat` so keystrokes echo to screen, not execute
    tmux kill-session -t "$SESS" 2>/dev/null
    tmux new-session -d -s "$SESS" -x 120 -y 40 || fatal "could not start a private tmux session"
    PANE=$(tmux list-panes -t "$SESS" -F '#{session_name}:#{window_index}.#{pane_index}')
    # An empty PANE would make every `-t "$PANE"` below mean "the current pane"
    # rather than erroring -- the other half of how this file used to type into
    # the developer's own session. Refuse rather than guess.
    [ -n "$PANE" ] || fatal "tmux gave no pane for session $SESS"
    tmux send-keys -t "$PANE" 'cat' Enter
    sleep 0.3
}
injected() { tmux capture-pane -pt "$PANE" | grep -q "$1" && echo yes || echo no; }

# --- the isolation itself, asserted before anything is driven -----------------
# If this regresses, the file silently goes back to driving the developer's
# server, so it is pinned rather than trusted.
fresh_pane
check "isolation: private socket in TMUX_TMPDIR" "yes" \
    "$(find "$TMUX_DIR" -type s 2>/dev/null | grep -q . && echo yes || echo no)"
check "isolation: test session invisible to the default server" "no" \
    "$(env -u TMUX_TMPDIR tmux ls 2>/dev/null | grep -q "^$SESS:" && echo yes || echo no)"

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

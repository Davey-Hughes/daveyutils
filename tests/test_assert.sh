#!/usr/bin/env bash
# The assert.sh harness contract: `finish` TERMINATES with the tally's status,
# so no caller can accidentally discard a failure.
#
# test_video_pcm_to_flac.sh's bash<4.3 skip branch ran `finish` then `exit 0`,
# throwing that status away. On macOS's system bash 3.2 -- the exact host the
# skip exists for -- a real regression in select_streams/output_path printed
# "FAIL:" and tallied "1 failed", yet the file exited 0, so run.sh's
# `bash "$t" || rc=1` never tripped and the suite reported PASSED.
#
# Making finish itself the terminator kills that class of mistake harness-wide
# rather than patching the one call site that happened to get it wrong.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/assert-contract.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT

# A file that fails a check and then tries to exit 0 after finish -- exactly the
# shape of the old skip branch. finish must win.
cat > "$WORKDIR/failing.sh" <<EOF
source "$HERE/assert.sh"
check "deliberately failing" "a" "b"
finish
exit 0
EOF
bash "$WORKDIR/failing.sh" >/dev/null 2>&1
rc_fail=$?
check "finish: a failed check cannot exit 0" "yes" \
    "$([ "$rc_fail" -ne 0 ] && echo yes || echo no)"

# ... while an all-passing file still exits 0 (guards against over-fixing).
cat > "$WORKDIR/passing.sh" <<EOF
source "$HERE/assert.sh"
check "deliberately passing" "a" "a"
finish
EOF
bash "$WORKDIR/passing.sh" >/dev/null 2>&1
rc_pass=$?
check "finish: an all-passing file exits 0" "yes" \
    "$([ "$rc_pass" -eq 0 ] && echo yes || echo no)"

# The tally still reaches stdout before terminating.
tally=$(bash "$WORKDIR/failing.sh" 2>/dev/null | tail -1)
check "finish: prints the tally before exiting" "== 0 passed, 1 failed ==" "$tally"

finish

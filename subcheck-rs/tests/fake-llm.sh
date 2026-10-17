#!/usr/bin/env bash
# A stand-in LLM for the scan tests: reads a subcheck prompt on stdin and "corrects" the typos the
# fixtures plant ("befor", "trough") in subcheck's reply format. Offline and deterministic.
#
#   FAKE_LLM_LOG=<file>    append "<n> lines" per call, to count calls and lines sent
#   FAKE_LLM_FAIL=1        exit 3 without replying
#   FAKE_LLM_REPLY=<text>  reply with exactly this instead
set -euo pipefail

prompt=$(cat)
if [ -n "${FAKE_LLM_LOG:-}" ]; then
  printf '%s lines\n' "$(grep -c '^L[0-9]' <<<"$prompt")" >>"$FAKE_LLM_LOG"
fi
if [ "${FAKE_LLM_FAIL:-}" = 1 ]; then
  echo "fake-llm: told to fail" >&2
  exit 3
fi
if [ -n "${FAKE_LLM_REPLY+set}" ]; then
  printf '%s\n' "$FAKE_LLM_REPLY"
  exit 0
fi

found=0
re='^L([0-9]+) \[[^]]*\] \([^)]*\) (.*)$'
while IFS= read -r line; do
  [[ $line =~ $re ]] || continue
  id=${BASH_REMATCH[1]}
  text=${BASH_REMATCH[2]}
  fixed=${text//befor /before }
  fixed=${fixed//trough/through}
  if [ "$fixed" != "$text" ]; then
    printf 'L%s\t%s\t%s\n' "$id" "$fixed" "planted typo"
    found=1
  fi
done <<<"$prompt"
[ "$found" = 1 ] || echo NONE

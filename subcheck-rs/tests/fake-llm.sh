#!/usr/bin/env bash
# A stand-in LLM for the scan tests: reads a subcheck prompt on stdin and "corrects" the typos the
# fixtures plant ("befor", "trough") in subcheck's reply format. Offline and deterministic.
#
# Run through a link named `claude` or `codex`, it prints the reply as that preset reads it:
# claude's JSON, or codex's events, with made-up usage.
#
#   FAKE_LLM_LOG=<file>    append "<n> lines" per call, to count calls and lines sent
#   FAKE_LLM_FAIL=1        exit 3 without replying
#   FAKE_LLM_REPLY=<text>  reply with exactly this instead
#   FAKE_LLM_PLAIN=1       reply in plain text even as claude or codex
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
  reply=$FAKE_LLM_REPLY
else
  reply=
  re='^L([0-9]+) \[[^]]*\] \([^)]*\) (.*)$'
  while IFS= read -r line; do
    [[ $line =~ $re ]] || continue
    id=${BASH_REMATCH[1]}
    text=${BASH_REMATCH[2]}
    fixed=${text//befor /before }
    fixed=${fixed//trough/through}
    if [ "$fixed" != "$text" ]; then
      reply+="L$id"$'\t'"$fixed"$'\t'"planted typo"$'\n'
    fi
  done <<<"$prompt"
  reply=${reply%$'\n'}
  [ -n "$reply" ] || reply=NONE
fi

# `$1` as the inside of a JSON string. A character at a time, through printf: a backslash in a
# `${s//...}` replacement means different things to different bash versions (macOS has 3.2).
json() {
  local s=$1 i c
  for ((i = 0; i < ${#s}; i++)); do
    c=${s:i:1}
    case $c in
      '\') printf '\\\\' ;;
      '"') printf '\\"' ;;
      $'\t') printf '\\t' ;;
      $'\n') printf '\\n' ;;
      $'\r') printf '\\r' ;;
      # Any other control character, which JSON does not allow raw: by its code.
      [[:cntrl:]]) printf '\\u%04x' "'$c" ;;
      *) printf '%s' "$c" ;;
    esac
  done
}

name=$(basename "$0")
[ "${FAKE_LLM_PLAIN:-}" = 1 ] && name=plain
case "$name" in
  claude)
    printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","total_cost_usd":0.0125,"usage":{"input_tokens":3,"cache_creation_input_tokens":1000,"cache_read_input_tokens":200,"output_tokens":40}}\n' \
      "$(json "$reply")"
    ;;
  codex)
    printf '{"type":"thread.started","thread_id":"t"}\n{"type":"turn.started"}\n'
    printf '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"%s"}}\n' \
      "$(json "$reply")"
    printf '{"type":"turn.completed","usage":{"input_tokens":1500,"cached_input_tokens":1000,"output_tokens":40}}\n'
    ;;
  *)
    printf '%s\n' "$reply"
    ;;
esac

#!/usr/bin/env bash
# derive_title logic for mkvpropedit_set_name.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
# shellcheck disable=SC1090
source "$HERE/../scripts/mkvpropedit_set_name"

check "default: basename sans .mkv" "Show - S01 - Pilot" "$(derive_title 'Show - S01 - Pilot.mkv')"
check "default: path stripped"      "Pilot"              "$(derive_title '/a/b/Pilot.mkv')"
check "field 3 (old -d - -f 3)"     "Pilot"              "$(derive_title 'Show - S01 - Pilot.mkv' '-' 3)"
check "field 2 trimmed"             "S01"                "$(derive_title 'Show - S01 - Pilot.mkv' '-' 2)"

# `find -iname '*.mkv'` matches case-insensitively, so the extension strip must
# too -- otherwise an uppercase-extension file (Windows rippers, older MakeMKV)
# gets ".MKV" written INTO its title tag, and mkvpropedit exits 0 so the summary
# reports it as a success. The stray extension also lands in the -d/-f cut field.
check "default: uppercase .MKV stripped" "Movie" "$(derive_title 'Movie.MKV')"
check "default: mixed-case .Mkv stripped" "Movie" "$(derive_title 'Movie.Mkv')"
check "field 3 with uppercase .MKV"      "Pilot" "$(derive_title 'Show - S01 - Pilot.MKV' '-' 3)"

# --- batch resilience: one failing mkvpropedit must not abort the batch -------
# A stub `mkvpropedit` on PATH fails for b.mkv only. Both files must still be
# attempted (the loop must not run in a lost subshell, and a single failure
# must not kill the whole `find | while` under set -euo pipefail), and the
# script must report an accurate "N updated, M failed" summary while still
# exiting 0 overall.
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/mkv-batch.XXXXXX")
STUBDIR=$(mktemp -d "${TMPDIR:-/tmp}/mkv-stub.XXXXXX")
trap 'rm -rf "$WORKDIR" "$STUBDIR"' EXIT

MEDIA_DIR="$WORKDIR/media"
mkdir -p "$MEDIA_DIR"
: > "$MEDIA_DIR/a.mkv"
: > "$MEDIA_DIR/b.mkv"

STUB_LOG="$WORKDIR/stub.log"
: > "$STUB_LOG"

cat > "$STUBDIR/mkvpropedit" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "$STUB_LOG"
case "$1" in
    *b.mkv) exit 1 ;;
    *) exit 0 ;;
esac
STUB
chmod +x "$STUBDIR/mkvpropedit"

out=$(PATH="$STUBDIR:$PATH" STUB_LOG="$STUB_LOG" \
    bash "$HERE/../scripts/mkvpropedit_set_name" "$MEDIA_DIR" 2>"$WORKDIR/stderr.log")
rc=$?

check "batch: script exits 0 despite a failed file" "0" "$rc"
check "batch: a.mkv attempted" "yes" \
    "$(grep -q 'a\.mkv$' "$STUB_LOG" && echo yes || echo no)"
check "batch: b.mkv attempted (not skipped after failure)" "yes" \
    "$(grep -q 'b\.mkv$' "$STUB_LOG" && echo yes || echo no)"
check "batch: summary reports 1 updated, 1 failed" "yes" \
    "$(printf '%s' "$out" | grep -q 'done: 1 updated, 1 failed' && echo yes || echo no)"
check "batch: warns on stderr for the failed file" "yes" \
    "$(grep -q 'WARN: mkvpropedit failed for .*b\.mkv' "$WORKDIR/stderr.log" && echo yes || echo no)"

finish

#!/usr/bin/env bash
# Tests for scripts/cue2flac.
#
# cue2flac is a straight-line script (top-level `set -euo pipefail`, no main(),
# no source guard), so sourcing it would run it -- these drive the real script
# as a subprocess against a synthetic CUE/BIN pair with a stubbed ffmpeg.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"

CUE2FLAC="$HERE/../scripts/cue2flac"
SECTOR=2352

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/cue2flac.XXXXXX")
STUBDIR=$(mktemp -d "${TMPDIR:-/tmp}/cue2flac-stub.XXXXXX")
trap 'rm -rf "$WORKDIR" "$STUBDIR"' EXIT

# ffmpeg stub: drain the dd pipe (a stub that exits without reading gives dd a
# SIGPIPE, which `pipefail` would report as the very failure we're measuring),
# log the args, touch the output. Fails for the track whose output path matches
# $FFMPEG_FAIL_MATCH, leaving a partial file behind exactly as a real ffmpeg does.
cat > "$STUBDIR/ffmpeg" <<'STUB'
#!/usr/bin/env bash
cat >/dev/null
printf '%s\n' "$*" >> "$FFMPEG_LOG"
for out in "$@"; do :; done   # the output path is the last argument
: > "$out"
if [ -n "${FFMPEG_FAIL_MATCH:-}" ]; then
    case "$out" in
        *"$FFMPEG_FAIL_MATCH"*)
            echo "ffmpeg: simulated failure for $out" >&2
            exit 1 ;;
    esac
fi
exit 0
STUB
chmod +x "$STUBDIR/ffmpeg"

# msf <sector> -> the CUE's MM:SS:FF timestamp (75 frames/sec).
msf() {
    printf '%02d:%02d:%02d' $(( $1 / 4500 )) $(( ($1 % 4500) / 75 )) $(( $1 % 75 ))
}

# make_disc <dir> <n-audio> <leading-data-track: yes|no>
# Writes <dir>/disc.cue + a disc.bin big enough for every track to have length.
# Tracks are spaced 10 sectors apart -- the arithmetic is irrelevant here, only
# the AUDIO/MODE1 mix is.
make_disc() {
    local dir="$1" n="$2" data="$3"
    local cue="$dir/disc.cue" t=0 sector=0 i
    mkdir -p "$dir"
    printf 'FILE "disc.bin" BINARY\n' > "$cue"
    if [ "$data" = yes ]; then
        t=$(( t + 1 ))
        printf '  TRACK %02d MODE1/2352\n    INDEX 01 %s\n' "$t" "$(msf $sector)" >> "$cue"
        sector=$(( sector + 10 ))
    fi
    for (( i = 0; i < n; i++ )); do
        t=$(( t + 1 ))
        printf '  TRACK %02d AUDIO\n    INDEX 01 %s\n' "$t" "$(msf $sector)" >> "$cue"
        sector=$(( sector + 10 ))
    done
    dd if=/dev/zero of="$dir/disc.bin" bs=$SECTOR count=$(( sector + 10 )) status=none 2>/dev/null
}

# tracklist <n> -- <n> names, one per line, as pasted into -t.
tracklist() {
    local i
    for (( i = 1; i <= $1; i++ )); do printf 'Song %02d\n' "$i"; done
}

# run_cue2flac <dir> [args...] -- run the script over <dir>/disc.cue into
# <dir>/out. Echoes stdout; stderr lands in <dir>/stderr.log, ffmpeg's argv in
# <dir>/ffmpeg.log. Returns the script's exit status.
run_cue2flac() {
    local dir="$1"; shift
    : > "$dir/ffmpeg.log"
    PATH="$STUBDIR:$PATH" FFMPEG_LOG="$dir/ffmpeg.log" \
        bash "$CUE2FLAC" "$dir/disc.cue" "$dir/out" "$@" 2>"$dir/stderr.log"
}

# --- M13: the tracklist count must be validated against AUDIO tracks ----------
# A game soundtrack with a MODE1 data track 1 followed by 12 audio tracks is
# exactly the disc you'd paste a VGMdb tracklist for -- and the two documented
# features ("Data tracks (MODE1/MODE2) are skipped" + "-t ... paste from VGMdb")
# were mutually unusable on it: `tracks` counted all 13 INDEX 01 lines, so a
# 12-name paste died with "Tracklist has 12 names but CUE has 13 tracks."
# Working around it meant padding a dummy name for the data track, because the
# name lookup indexed by CUE track rather than by audio-track ordinal.
m13="$WORKDIR/m13"
make_disc "$m13" 12 yes
m13out=$(run_cue2flac "$m13" -t "$(tracklist 12)")
m13rc=$?
check "M13: mixed-mode disc + 12-name tracklist is accepted" "0" "$m13rc"
check "M13: all 12 audio tracks extracted" "12" \
    "$(ls "$m13/out" 2>/dev/null | wc -l | tr -d ' ')"
check "M13: the data track is still skipped" "yes" \
    "$(printf '%s' "$m13out" | grep -q 'Track 01: MODE1/2352 (skipping non-audio)' && echo yes || echo no)"
# The lookup: audio track 1 is disc track 2, and must take the FIRST name.
# Indexing by CUE track gave it names[1] -- "Song 02" -- shifting every name by one.
check "M13: the first audio track takes the first name" "yes" \
    "$([ -f "$m13/out/02 - Song 01.flac" ] && echo yes || echo no)"
check "M13: the last audio track takes the last name" "yes" \
    "$([ -f "$m13/out/13 - Song 12.flac" ] && echo yes || echo no)"
# -metadata track=: audio track 1 of 12, not CUE track 2 of 13.
check "M13: track metadata numbers the audio programme (1/12)" "yes" \
    "$(grep -q 'track=1/12' "$m13/ffmpeg.log" && echo yes || echo no)"
check "M13: track metadata is not the raw CUE index (2/13)" "yes" \
    "$(grep -q 'track=2/13' "$m13/ffmpeg.log" && echo no || echo yes)"

# --- M13: a genuinely mismatched tracklist must still be rejected -------------
# Guards against "fixing" the count by dropping the validation altogether.
m13b="$WORKDIR/m13b"
make_disc "$m13b" 12 yes
run_cue2flac "$m13b" -t "$(tracklist 11)" >/dev/null
check "M13: 11 names for 12 audio tracks still errors" "1" "$?"
check "M13: the error counts audio tracks, not CUE tracks" "yes" \
    "$(grep -q '11 names but CUE has 12 audio tracks' "$m13b/stderr.log" && echo yes || echo no)"

# --- M13: an all-audio disc must be unaffected --------------------------------
m13c="$WORKDIR/m13c"
make_disc "$m13c" 12 no
run_cue2flac "$m13c" -t "$(tracklist 12)" >/dev/null
check "M13: all-audio disc still accepts a 12-name tracklist" "0" "$?"
check "M13: all-audio disc numbers from 01" "yes" \
    "$([ -f "$m13c/out/01 - Song 01.flac" ] && echo yes || echo no)"
check "M13: all-audio disc metadata is 1/12" "yes" \
    "$(grep -q 'track=1/12' "$m13c/ffmpeg.log" && echo yes || echo no)"

# --- M12: one failing track must not abort the extraction ---------------------
# The `dd | ffmpeg` pipeline was a bare statement under top-level `set -e`, so a
# single bad boundary or codec hiccup killed the whole run: later tracks were
# never extracted, the summary never printed, and the partial FLAC was left on
# disk. Every rewritten sibling script guarantees the opposite (warn, continue,
# accurate summary) -- cue2flac is the one that was never rewritten.
m12="$WORKDIR/m12"
make_disc "$m12" 3 no
m12out=$(FFMPEG_FAIL_MATCH="02 - " run_cue2flac "$m12" -t "$(tracklist 3)")
m12rc=$?
check "M12: a failing track does not abort the run" "0" "$m12rc"
check "M12: the track before the failure was extracted" "yes" \
    "$([ -f "$m12/out/01 - Song 01.flac" ] && echo yes || echo no)"
check "M12: the track AFTER the failure was still extracted" "yes" \
    "$([ -f "$m12/out/03 - Song 03.flac" ] && echo yes || echo no)"
check "M12: the partial output of the failed track is removed" "no" \
    "$([ -f "$m12/out/02 - Song 02.flac" ] && echo yes || echo no)"
check "M12: the summary still prints, counting 2 extracted" "yes" \
    "$(printf '%s' "$m12out" | grep -q '2 track(s) extracted' && echo yes || echo no)"
check "M12: the summary reports the failure" "yes" \
    "$(printf '%s' "$m12out" | grep -q '1 failed' && echo yes || echo no)"
check "M12: the failure is WARNed on stderr" "yes" \
    "$(grep -q 'WARN.*track 02' "$m12/stderr.log" && echo yes || echo no)"

finish

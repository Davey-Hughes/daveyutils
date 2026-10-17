#!/usr/bin/env bash
# Rebuild subcheck-rs's test fixtures: small MKVs muxed from synthetic subtitles, and what mkvextract
# makes of each of their text tracks -- the oracle that `subcheck extract` must match byte for byte.
#
#   dev/make-subcheck-fixtures.sh            # writes subcheck-rs/tests/fixtures/
#   dev/make-subcheck-fixtures.sh OUT_DIR
#
# Needs mkvmerge, mkvextract, ffmpeg and jq. The output is committed, so neither CI nor `make check`
# needs any of them: run this only to add or change a fixture. Output is deterministic (mkvmerge
# --deterministic, ffmpeg bitexact), so rerunning it unchanged leaves `git status` clean.
#
# Synthetic text only. The repository is public, so no real fansub lines go in here.
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:-subcheck-rs/tests/fixtures}
for tool in mkvmerge mkvextract ffmpeg jq; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found" >&2; exit 1; }
done

src=$(mktemp -d)
trap 'rm -rf "${src:?}"' EXIT
mkdir -p "$out/expected"

# One ASS script exercising what extract has to reproduce: a Comment event (kept in CodecPrivate),
# two events with the same start, override tags, \N, a comma inside the text, and a ReadOrder that
# is not time order (the last line is the earliest-but-one in time).
cat >"$src/a.ass" <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Comment: 0,0:00:00.00,0:00:00.00,Default,,0,0,0,,OP
Dialogue: 0,0:00:01.00,0:00:02.50,Default,amy,0,0,0,,First line, with a comma.
Dialogue: 0,0:00:01.00,0:00:02.50,Default,bob,0,0,0,,Same start as the first.
Dialogue: 1,0:00:12.34,0:00:14.00,Default,,0,0,0,fx,{\an5\pos(100,300)}A
Dialogue: 0,0:01:05.07,0:01:07.99,Default,,0,0,0,,Later {\i1}italic{\i0} line\Nsecond row
Dialogue: 0,0:00:30.00,0:00:31.00,Default,,0,0,0,,Out of order in the file
EOF
sed 's/First line/Signs line/' "$src/a.ass" >"$src/a2.ass"

cat >"$src/c.ssa" <<'EOF'
[Script Info]
ScriptType: v4.00

[V4 Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding
Style: Default,Arial,20,16777215,255,0,0,0,0,1,2,0,2,10,10,10,0,1

[Events]
Format: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: Marked=0,0:00:01.00,0:00:02.00,Default,,0000,0000,0000,,SSA line one
Dialogue: Marked=0,0:00:05.00,0:00:06.00,Default,,0000,0000,0000,,SSA line two
EOF

# CRLF on purpose, and millisecond times that ASS's centiseconds could not hold.
printf '1\r\n00:00:01,005 --> 00:00:02,500\r\nHello <i>there</i>\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nTwo\r\nrows\r\n\r\n' >"$src/b.srt"

# What `subcheck scan` is tested on: two planted typos, "befor" in dialogue and "trough" spelled out
# as per-letter karaoke (one event per letter, positioned with \pos, written right to left so only
# the positions can put it back in order), plus a drawing, a line that repeats later, and a style of
# romaji fragments that text::lines must drop.
{
  cat <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1
Style: Song,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1
Style: Romaji,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,We should leave befor it rains.
Dialogue: 0,0:00:04.00,0:00:06.00,Default,,0,0,0,,{\i1}Nothing{\i0} wrong here.
Dialogue: 0,0:00:07.00,0:00:09.00,Default,,0,0,0,,{\p1}m 0 0 l 100 0 100 100 0 100{\p0}
Dialogue: 0,0:00:10.00,0:00:12.00,Default,,0,0,0,,Nothing wrong here.
Dialogue: 0,0:00:30.00,0:00:32.00,Default,,0,0,0,,The last line.
EOF
  lyric="go trough it"
  for ((i = ${#lyric} - 1; i >= 0; i--)); do
    c=${lyric:i:1}
    [ "$c" = " " ] && c=""
    printf 'Dialogue: 0,0:00:20.00,0:00:22.00,Song,,0,0,0,fx,{\\an5\\pos(%d,40)}%s\n' $((200 + 20 * i)) "$c"
  done
  syllables=(ka ze no)
  for i in $(seq 0 11); do
    printf 'Dialogue: 0,0:00:23.%02d,0:00:24.00,Romaji,,0,0,0,fx,%s\n' $((i * 5)) "${syllables[i % 3]}"
  done
} >"$src/typos.ass"

mux() { mkvmerge --quiet --deterministic 2026 "$@"; }

mux -o "$out/ass-plain.mkv" "$src/a.ass"
mux -o "$out/ass-zlib.mkv" --compression 0:zlib "$src/a.ass"
mux -o "$out/ass-nocues.mkv" --cues 0:none "$src/a.ass"
# +7 ms: ASS times are centiseconds, so this pins how extract truncates.
mux -o "$out/ass-sync7.mkv" --sync 0:7 "$src/a.ass"
mux -o "$out/ssa.mkv" "$src/c.ssa"
mux -o "$out/srt.mkv" --language 0:eng "$src/b.srt"
mux -o "$out/typos.mkv" --language 0:eng --track-name '0:English Full' "$src/typos.ass"

# ffmpeg muxes differently from mkvmerge: CRC-32 elements in every Cluster, several
# CueTrackPositions in one CuePoint, LF line endings in CodecPrivate, and no statistics tags.
ffmpeg -nostdin -loglevel error -y -i "$src/a.ass" -c copy -fflags +bitexact "$out/ass-ffmpeg.mkv"

# Subtitle blocks interleaved with video blocks, three text tracks, two languages.
ffmpeg -nostdin -loglevel error -y -f lavfi -i color=c=black:s=64x36:r=1:d=70 \
  -c:v libx264 -preset ultrafast -crf 51 -g 50 -threads 1 \
  -fflags +bitexact -flags:v +bitexact "$src/video.mkv"
mux -o "$out/multi.mkv" "$src/video.mkv" \
  --language 0:eng --track-name '0:English Full' "$src/a.ass" \
  --language 0:eng --track-name '0:English Signs' --default-track-flag 0:no "$src/a2.ass" \
  --language 0:jpn "$src/b.srt"

# A subtitle track with no events at all: no Cues to give, and a NUMBER_OF_FRAMES of 0 that says so.
# Real releases ship these (an empty "English" track); reading one must not walk the whole file.
grep -v '^Comment:' "$src/a.ass" | grep -v '^Dialogue:' >"$src/empty.ass"
mux -o "$out/empty.mkv" "$src/video.mkv" --language 0:eng --track-name '0:English Full' "$src/empty.ass"

# The oracle: mkvextract's output for every text subtitle track, named the way `subcheck extract`
# names its default output (<stem>.track<ID>.<ext>).
for mkv in "$out"/*.mkv; do
  stem=$(basename "$mkv" .mkv)
  mkvmerge -J "$mkv" | jq -r '.tracks[] | select(.type == "subtitles") | "\(.id) \(.properties.codec_id)"' |
    while read -r id codec; do
      case $codec in
        S_TEXT/ASS)  ext=ass ;;
        S_TEXT/SSA)  ext=ssa ;;
        S_TEXT/UTF8) ext=srt ;;
        *) continue ;;
      esac
      mkvextract --quiet "$mkv" tracks "$id:$out/expected/$stem.track$id.$ext"
    done
done

ls -l "$out" "$out/expected"

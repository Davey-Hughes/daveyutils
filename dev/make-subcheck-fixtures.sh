#!/usr/bin/env bash
# Rebuild subcheck-rs's test fixtures: small MKVs muxed from synthetic subtitles, and what mkvextract
# makes of each of their text tracks -- the oracle that `subcheck extract` must match byte for byte.
#
#   dev/make-subcheck-fixtures.sh            # writes subcheck-rs/tests/fixtures/
#   dev/make-subcheck-fixtures.sh OUT_DIR
#
# Needs mkvmerge, mkvextract, mkvpropedit, ffmpeg and jq. The output is committed, so neither CI nor
# `make check` needs any of them: run this only to add or change a fixture. Output is deterministic
# (mkvmerge --deterministic, ffmpeg bitexact), so rerunning it unchanged leaves `git status` clean.
#
# Synthetic text only. The repository is public, so no real fansub lines go in here.
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:-subcheck-rs/tests/fixtures}
for tool in mkvmerge mkvextract mkvpropedit ffmpeg jq; do
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

# What `subcheck timing` is tested on: a 2 fps video with a keyframe every 7 s (14 frames), and four
# tracks against it. Dialogue that starts on every keyframe and ends on the next, half the lines on
# the cut's own time and half half a frame before it (Aegisub's way), with lines between them that
# end 2 frames before a cut, and signs in mid-shot, nowhere near a cut; the same script muxed one
# frame (500 ms) late; karaoke and effects only, with no style timed to the cuts; and the signs on
# their own, muxed late as well: too far from the cuts for a verdict, which their lines shared with
# the late track give.
ass_time() { printf '%d:%02d:%02d.%02d' $(($1 / 360000)) $(($1 / 6000 % 60)) $(($1 / 100 % 60)) $(($1 % 100)); }
timing_header() {
  cat <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1
Style: OP,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1
Style: FX,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
EOF
}
{
  timing_header
  for i in $(seq 0 33); do
    start=$((i * 700))
    if ((i % 2)); then end=$(((i + 1) * 700)); else end=$(((i + 1) * 700 - 25)); fi
    printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Line %d on the cut.\n' "$(ass_time $start)" "$(ass_time $end)" "$i"
    if ((i % 2)); then
      printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Line %d between cuts.\n' "$(ass_time $((start + 300)))" "$(ass_time $((start + 600)))" "$i"
    else
      printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Sign %d.\n' "$(ass_time $((start + 200)))" "$(ass_time $((start + 400)))" "$i"
    fi
  done
} >"$src/timing.ass"
{
  timing_header
  # Karaoke a frame after every keyframe: were it not left out, it would read as a late track.
  for i in $(seq 0 33); do
    printf 'Dialogue: 0,%s,%s,OP,,0,0,0,,{\\k50}Ka{\\k50}ra{\\k50}o{\\k50}ke\n' "$(ass_time $((i * 700 + 50)))" "$(ass_time $(((i + 1) * 700 + 50)))"
  done
  # Effects starting 3 frames before a keyframe to 3 after, three of each, and ending between cuts.
  for k in $(seq 0 20); do
    printf 'Dialogue: 0,%s,%s,FX,,0,0,0,,Effect %d\n' "$(ass_time $(((k + 5) * 700 + (k % 7 - 3) * 50)))" "$(ass_time $(((k + 5) * 700 + 300)))" "$k"
  done
} >"$src/timing-karaoke.ass"
{
  timing_header
  grep ',Sign [0-9]*\.$' "$src/timing.ass"
} >"$src/timing-signs.ass"
ffmpeg -nostdin -loglevel error -y -f lavfi -i color=c=black:s=64x36:r=2:d=240 \
  -c:v libx264 -preset ultrafast -crf 51 -g 14 -sc_threshold 0 -threads 1 \
  -fflags +bitexact -flags:v +bitexact "$src/video-2fps.mkv"
mux -o "$out/timing.mkv" "$src/video-2fps.mkv" \
  --language 0:eng --track-name '0:Dialogue in sync' "$src/timing.ass" \
  --language 0:eng --track-name '0:Dialogue a frame late' --default-track-flag 0:no --sync 0:500 "$src/timing.ass" \
  --language 0:eng --track-name '0:Karaoke and effects' --default-track-flag 0:no "$src/timing-karaoke.ass" \
  --language 0:eng --track-name '0:Signs a frame late' --default-track-flag 0:no --sync 0:500 "$src/timing-signs.ass"

# What apply is tested on where the keyframes leave frames' stored times to the file: S;G 0's
# Valentine's special's video, in small. 23.976 fps less 1/24 ms, so every 24th frame from frame 5
# is exactly half a millisecond past one, stored at the later before frame 840 and at the earlier
# from it; keyframes on frame 0 and every 48th from frame 5, so on some of each. Its dialogue is a
# frame late: a line on each keyframe to the next, starting and ending on the frame after. The five
# lines after those each start or end on, or just after, one of the frames between keyframes that
# are half a millisecond past (29, 989 and 1469): the third is shown on no frame, and the fourth on
# one only because frame 1469 is stored early.
ties_ms() { # frame N's stored time
  local num=$((1001 * $1 - 1)) q r
  ((num < 0)) && { echo 0; return; }
  q=$((num / 24)) r=$((num % 24))
  if ((r > 12 || (r == 12 && $1 < 840))); then echo $((q + 1)); else echo "$q"; fi
}
{
  echo '# timestamp format v2'
  for n in $(seq 0 1677); do ties_ms "$n"; done
} >"$src/ties-video.txt"
ffmpeg -nostdin -loglevel error -y -f lavfi -i color=c=black:s=64x36:r=24000/1001 -frames:v 1678 \
  -c:v libx264 -preset ultrafast -crf 51 -g 1000 -sc_threshold 0 -threads 1 \
  -force_key_frames 'expr:eq(n,0)+eq(mod(n,48),5)' \
  -fflags +bitexact -flags:v +bitexact "$src/video-ties.mkv"
# The latest centisecond on frame N.
on_frame() { echo $(($(ties_ms "$1") / 10)); }
{
  timing_header
  for i in $(seq 0 28); do
    start=$(on_frame $((48 * i + 6))) end=$(on_frame $((48 * i + 54)))
    printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Line %d on the cut.\n' "$(ass_time "$start")" "$(ass_time "$end")" "$i"
  done
  printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Ends just after a frame.\n' "$(ass_time 110)" "$(ass_time 125)"
  printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Starts on a frame stored early.\n' "$(ass_time 4125)" "$(ass_time 4150)"
  printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Shown on no frame.\n' "$(ass_time 4125)" "$(ass_time 4126)"
  printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,Shown on a frame stored early.\n' "$(ass_time 6123)" "$(ass_time 6127)"
  printf 'Dialogue: 0,%s,%s,Default,,0,0,0,,The last line.\n' "$(ass_time 6125)" "$(ass_time 6131)"
} >"$src/ties.ass"
mux -o "$out/ties.mkv" --timestamps "0:$src/ties-video.txt" "$src/video-ties.mkv" \
  --language 0:eng --track-name '0:Dialogue a frame late' "$src/ties.ass"
# mkvmerge gives the video the 42 ms its timestamps step by, and not 1001/24 ms, even when told.
mkvpropedit --quiet "$out/ties.mkv" --edit track:v1 --set default-duration=41708333

# What `subcheck apply` is tested on: everything a remux must keep (a title, chapters, an attachment,
# an audio track with the commentary flag, a forced zlib track, a hearing-impaired SRT track) and a
# fix of each kind. In the Full track: a plain typo, one inside italics, one after a \N, and a song
# line that the Signs/Songs track repeats. In Signs/Songs: a sign on two layers and per-letter karaoke
# (which apply leaves for a hand fix). In the SRT track: a typo inside <i> and one on a second row.
apply_header() {
  cat <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1
Style: Song,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1
Style: Sign,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,5,10,10,10,1
Style: OP,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
EOF
}
{
  apply_header
  cat <<'EOF'
Dialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,We should leave befor it rains.
Dialogue: 0,0:00:03.00,0:00:04.50,Default,,0,0,0,,{\i1}Ihave{\i0} to go.
Dialogue: 0,0:00:05.00,0:00:06.50,Default,,0,0,0,,Two rows here\Nand teh second.
Dialogue: 0,0:00:07.00,0:00:08.50,Default,,0,0,0,,Nothing {\b1}wrong{\b0} here.
Dialogue: 0,0:00:09.00,0:00:10.00,Song,,0,0,0,,{\be2}We happen share the road
EOF
} >"$src/apply-full.ass"
{
  apply_header
  cat <<'EOF'
Dialogue: 0,0:00:02.00,0:00:04.00,Sign,,0,0,0,,{\bord3\pos(320,100)}Libary
Dialogue: 1,0:00:02.00,0:00:04.00,Sign,,0,0,0,,{\bord0\pos(320,100)}Libary
Dialogue: 0,0:00:09.00,0:00:10.00,Song,,0,0,0,,{\be2}We happen share the road
EOF
  lyric="go trough it"
  for ((i = 0; i < ${#lyric}; i++)); do
    c=${lyric:i:1}
    [ "$c" = " " ] && c=""
    printf 'Dialogue: 0,0:00:05.00,0:00:07.00,OP,,0,0,0,fx,{\\an5\\pos(%d,40)}%s\n' $((200 + 20 * i)) "$c"
  done
} >"$src/apply-signs.ass"
printf '1\r\n00:00:01,005 --> 00:00:02,500\r\nHello <i>tehre</i>\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nA line split\r\nacros two rows\r\n\r\n' >"$src/apply.srt"
printf 'CHAPTER01=00:00:00.000\nCHAPTER01NAME=Part A\nCHAPTER02=00:00:06.000\nCHAPTER02NAME=Part B\n' >"$src/apply-chapters.txt"
for i in $(seq 1 64); do printf 'not a real font, line %02d\n' "$i"; done >"$src/fake-font.ttf"
ffmpeg -nostdin -loglevel error -y -f lavfi -i color=c=black:s=64x36:r=1:d=12 \
  -f lavfi -i anullsrc=r=8000:cl=mono -t 12 -map 0:v -map 1:a \
  -c:v libx264 -preset ultrafast -crf 51 -g 6 -threads 1 -c:a flac \
  -fflags +bitexact -flags:v +bitexact -flags:a +bitexact "$src/apply-av.mkv"
mux -o "$out/apply.mkv" --title 'Apply fixture' --chapters "$src/apply-chapters.txt" \
  --attachment-mime-type font/ttf --attach-file "$src/fake-font.ttf" \
  --language 1:jpn --track-name '1:Commentary' --commentary-flag 1:yes "$src/apply-av.mkv" \
  --language 0:eng --track-name '0:English Full' "$src/apply-full.ass" \
  --language 0:eng --track-name '0:English Signs/Songs' --default-track-flag 0:no \
  --forced-display-flag 0:yes --compression 0:zlib "$src/apply-signs.ass" \
  --language 0:eng --track-name '0:English SDH' --default-track-flag 0:no \
  --hearing-impaired-flag 0:yes "$src/apply.srt"

# Text tracks timed by their DefaultDuration: blocks with no BlockDuration, so each line lasts the
# track's DefaultDuration. mkvmerge's --default-duration also retimes a track's lines to one per
# DefaultDuration: the SRT's are already that, 2 s each from 0 s; the ASS's become a frame each at
# 23.976 fps (0, 42 ms), a DefaultDuration no whole number of milliseconds. A typo in each.
printf '1\n00:00:00,000 --> 00:00:02,000\nOver tehre\n\n2\n00:00:02,000 --> 00:00:04,000\nAnd back\n\n' >"$src/timed.srt"
{
  timing_header
  printf 'Dialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,Over tehre\n'
  printf 'Dialogue: 0,0:00:02.00,0:00:04.00,Default,,0,0,0,,And back\n'
} >"$src/timed.ass"
mux -o "$out/timed.mkv" --language 0:eng --default-duration 0:2s "$src/timed.srt" \
  --language 0:eng --default-duration 0:24000/1001p "$src/timed.ass"

# `subcheck audio`'s files, and apply's check that a remux keeps each audio frame's time: low-passed
# noise in 30 ms bursts, 0.7 a second (FLAC packs the silence between them small), as copies that
# line up and copies that do not. audio.mkv, 30 s at 8 kHz: the sound; a copy 27 ms late, tagged
# jpn; a copy 20 ms late from 0:16 on (its second part placed 20 ms after the first ends); other
# noise. audio-ac3.mkv, 6 s: a FLAC, and an AC-3 of it at 48 kHz as ffmpeg encodes one, with
# ffmpeg's 256 samples of priming that nothing in the file records: 5.3 ms late.
burst="lowpass=f=3000,aeval=val(0)*lt(mod(t*0.7\,1)\,0.03)"
# flac OUT INPUT-ARGS...: the options must come before the output name, or ffmpeg ignores them.
flac() {
  local out=$1
  shift
  ffmpeg -nostdin -loglevel error -y "$@" -c:a flac -sample_fmt s16 -compression_level 12 \
    -fflags +bitexact -flags:a +bitexact "$out"
}
flac "$src/audio.flac" -f lavfi -i "anoisesrc=d=30:c=white:r=8000:a=0.2:seed=1" -af "$burst"
flac "$src/audio-other.flac" -f lavfi -i "anoisesrc=d=30:c=white:r=8000:a=0.2:seed=2" -af "$burst"
flac "$src/audio-head.flac" -i "$src/audio.flac" -af atrim=end_sample=128000
flac "$src/audio-tail.flac" -i "$src/audio.flac" -af "atrim=start_sample=128000,asetpts=PTS-STARTPTS"
printf "file '%s'\nduration 16.02\nfile '%s'\n" "$src/audio-head.flac" "$src/audio-tail.flac" >"$src/audio-stepped.txt"
ffmpeg -nostdin -loglevel error -y -f concat -safe 0 -i "$src/audio-stepped.txt" -c copy \
  -fflags +bitexact "$src/audio-stepped.mka"
mux -o "$out/audio.mkv" --language 0:eng --track-name '0:Main' "$src/audio.flac" \
  --language 0:jpn --track-name '0:27 ms late' --default-track-flag 0:no --sync 0:27 "$src/audio.flac" \
  --language 0:eng --track-name '0:20 ms late from 0:16' --default-track-flag 0:no "$src/audio-stepped.mka" \
  --language 0:eng --track-name '0:Other sound' --default-track-flag 0:no "$src/audio-other.flac"
flac "$src/audio-short.flac" -f lavfi -i "anoisesrc=d=6:c=white:r=8000:a=0.2:seed=3" -af "$burst"
ffmpeg -nostdin -loglevel error -y -i "$src/audio-short.flac" -ar 48000 -c:a ac3 -b:a 32k \
  -fflags +bitexact -flags:a +bitexact "$src/audio-short.ac3"
mux -o "$out/audio-ac3.mkv" --language 0:eng --track-name '0:FLAC' "$src/audio-short.flac" \
  --language 0:eng --track-name '0:AC-3 by ffmpeg' --default-track-flag 0:no "$src/audio-short.ac3"

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

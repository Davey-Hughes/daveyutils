#!/usr/bin/env bash
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
# shellcheck disable=SC1090
source "$HERE/../scripts/video_pcm_to_flac"

# ffprobe CSV: index,codec_name[,title]
csv=$'1,pcm_s16le,Surround\n2,aac,Stereo\n3,pcm_s24le'
check "pcm->flac selects pcm streams" "-c:1 flac -c:3 flac" "$(select_streams "$csv" pcm2flac)"
check "no flac streams to convert"    ""                    "$(select_streams "$csv" flac2pcm)"

csv2=$'1,flac,Lossless\n2,aac'
check "flac->pcm selects flac stream" "-c:1 pcm_s16le" "$(select_streams "$csv2" flac2pcm)"

check "output path joins show + input" "/out/MyShow/ep01.mkv" "$(output_path './ep01.mkv' /out MyShow)"

finish

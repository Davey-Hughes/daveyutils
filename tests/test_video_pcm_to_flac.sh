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

# stream_metadata uses `local -n` (namerefs), which needs bash 4.3+; macOS
# ships bash 3.2, where merely calling it errors out ("local: -n: invalid
# option"). Skip the nameref-dependent checks below on old bash -- the
# select_streams/output_path checks above are plain bash and still run.
if (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 3) )); then
    printf '  skip: %s needs bash 4.3+ (namerefs); this is bash %s\n' "$(basename "$0")" "$BASH_VERSION"
    # finish terminates with the tally's status. The `exit 0` that used to follow
    # it discarded that status, so on macOS's system bash 3.2 a genuine failure
    # in the checks above reported PASSED.
    finish
fi

# stream_metadata populates a real array by nameref; a title containing
# spaces must survive as a single argv element (not be word-split).
csv3=$'1,pcm_s16le,Surround PCM 5.1\n2,aac,Stereo'
declare -a md
stream_metadata md "$csv3" pcm2flac
check "metadata is 2 elements (title kept whole)" "2" "${#md[@]}"
check "metadata flag element"                     "-metadata:s:1" "${md[0]}"
check "title element intact with spaces"           "title=Surround FLAC 5.1" "${md[1]}"

# A trailing PCM stream with NO title must not make the helper return non-zero
# (it would abort the whole batch under `set -e`).
declare -a md_untitled
stream_metadata md_untitled $'1,pcm_s16le,Titled PCM\n3,pcm_s24le' pcm2flac
check "untitled trailing pcm stream -> rc 0" "0" "$?"
check "only the titled stream got metadata" "2" "${#md_untitled[@]}"

finish

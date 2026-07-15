#!/usr/bin/env bash
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
# shellcheck disable=SC1090
source "$HERE/../scripts/batch_makemkvcon"

check "disc dir from BDMV path"  "Disc Name"  "$(disc_subdir './Disc Name/BDMV/index.bdmv')"
check "nested BDMV"              "Movie"      "$(disc_subdir './Movie/BDMV/BACKUP/index.bdmv')"
check "no leading ./"           "X"          "$(disc_subdir 'X/BDMV/index.bdmv')"

finish

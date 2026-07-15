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

finish

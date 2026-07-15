#!/usr/bin/env bash
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
# shellcheck disable=SC1090
source "$HERE/../scripts/bisect_img"

check "1.33 is landscape"  "yes"  "$(is_landscape 1.33 && echo yes || echo no)"
check "1.0 is not"         "no"   "$(is_landscape 1.0  && echo yes || echo no)"
check "0.75 is not"        "no"   "$(is_landscape 0.75 && echo yes || echo no)"
check "2 is landscape"     "yes"  "$(is_landscape 2    && echo yes || echo no)"

finish

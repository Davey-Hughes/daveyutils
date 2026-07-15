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

check "out_prefix flattens with parent"  "sub_photo"  "$(out_prefix '/a/sub/photo.jpg')"
check "out_prefix strips extension"      "b_x"        "$(out_prefix 'b/x.jpeg')"

# The default invocation (`bisect_img` with no args) sets dir="." and outdir=".",
# and `find . -iname '*.jpg'` emits "./foo.jpg". basename "$(dirname './foo.jpg')"
# is ".", so the prefix became "._foo" and the halves were written as HIDDEN
# dotfiles (./._foo-1.jpg) -- invisible to ls, most file managers and a plain
# *.jpg glob -- while the script still reported "done: N bisected, 0 failed".
# The parent must resolve to a real directory name.
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/bisect-prefix.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT
mkdir -p "$WORKDIR/album"

check "out_prefix: ./foo.jpg is not hidden" "yes" \
    "$(cd "$WORKDIR/album" && case "$(out_prefix './foo.jpg')" in .*) echo no ;; *) echo yes ;; esac)"
check "out_prefix: ./foo.jpg uses the real cwd name" "album_foo" \
    "$(cd "$WORKDIR/album" && out_prefix './foo.jpg')"
check "out_prefix: bare foo.jpg (no dir part) uses cwd name" "album_foo" \
    "$(cd "$WORKDIR/album" && out_prefix 'foo.jpg')"

finish

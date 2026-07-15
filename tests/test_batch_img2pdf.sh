#!/usr/bin/env bash
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
# shellcheck disable=SC1090
source "$HERE/../scripts/batch_img2pdf"

# nounset_unsafe_expansions <file> <array> -- how many times <file> expands
# "${<array>[@]}" in the BARE form, i.e. not wrapped in the
# ${<array>[@]+"${<array>[@]}"} guard.
#
# On bash < 4.4 -- including the 3.2 macOS ships, which this repo targets -- an
# EMPTY array expands as though it were UNSET, so a bare "${a[@]}" under `set -u`
# aborts with "a[@]: unbound variable". bash 4.4 made ${a[@]} unconditionally
# exempt from nounset, so this cannot be reproduced at runtime on any bash the
# suite runs on. Verified on bash 5.3: an empty array, and even a never-declared
# one, expand silently under `set -u`, and neither `shopt -s compat43` nor
# BASH_COMPAT=4.3 restores the old rule -- while a scalar still aborts, so it is
# not that `set -u` is inactive. There is no runtime pin to be had here, so pin
# the idiom in the source instead.
#
# Each guarded occurrence contains exactly one bare occurrence as a substring,
# so the unguarded count is simply (all - guarded). Comments are dropped first:
# the scripts' own comments explain the fix by NAMING the bare form, and prose
# is not an expansion.
#
# Trailing comments are stripped only from a `#` that follows whitespace. The
# obvious `s/#.*$//` is worse than the bug it fixes: `${#pids[@]}` contains a
# `#`, so stripping from any `#` truncates real code and can hide a bare
# expansion later on the line -- trading a false positive (loud, safe) for a
# false negative (silent, unsafe). `${#` is preceded by `{`, never whitespace,
# so the anchored form leaves it alone.
nounset_unsafe_expansions() {
    local file="$1" arr="$2" code all guarded
    code=$(sed -e 's/^[[:space:]]*#.*$//' -e 's/[[:space:]]#.*$//' "$file")
    all=$(printf '%s\n' "$code" | grep -o -F "\${$arr[@]}" | wc -l | tr -d ' ')
    guarded=$(printf '%s\n' "$code" | grep -o -F "\${$arr[@]+\"\${$arr[@]}\"}" | wc -l | tr -d ' ')
    printf '%s' "$(( all - guarded ))"
}

check "defaults"          "./pdfs|0|book"     "$(parse_args book)"
check "custom outdir"     "/tmp/out|0|book"   "$(parse_args -o /tmp/out book)"
check "clean opt-in"      "./pdfs|1|book"     "$(parse_args --clean book)"

# --- batch resilience: one archive that fails to extract must not abort ------
# the run before any PDF is built (`wait "$pid"` following the final `&&` in
# the extraction loop is not errexit-exempt, so under `set -e` a single failed
# `unar` used to kill the script before the PDF loop ever ran).
# Stub unar/file/img2pdf on PATH: unar always fails; file always reports
# "image/jpeg"; img2pdf just records its args and touches the output file.
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-batch.XXXXXX")
STUBDIR=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-stub.XXXXXX")
trap 'rm -rf "$WORKDIR" "$STUBDIR"' EXIT

MAINDIR="$WORKDIR/main"
OUTDIR="$WORKDIR/outdir"
mkdir -p "$MAINDIR/images"
: > "$MAINDIR/archive.zip"
: > "$MAINDIR/images/page1.jpg"

UNAR_LOG="$WORKDIR/unar.log"
IMG2PDF_LOG="$WORKDIR/img2pdf.log"
: > "$UNAR_LOG"
: > "$IMG2PDF_LOG"

cat > "$STUBDIR/unar" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$UNAR_LOG"
exit 1
STUB

# Report image/* for image extensions only. A blanket "image/jpeg" would make
# the stub call a .DS_Store an image too, which no real `file` does -- and the
# --clean coverage tests below turn on exactly that distinction (an entry the
# image scan skipped is an entry the PDF does not cover).
cat > "$STUBDIR/file" <<'STUB'
#!/usr/bin/env bash
for path in "$@"; do :; done   # the path is the last argument
case "$path" in
    *.jpg|*.jpeg|*.png) printf 'image/jpeg\n' ;;
    *) printf 'application/octet-stream\n' ;;
esac
STUB

cat > "$STUBDIR/img2pdf" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$IMG2PDF_LOG"
prev=""
for a in "$@"; do
    [[ "$prev" == "-o" ]] && : > "$a"
    prev="$a"
done
exit 0
STUB
chmod +x "$STUBDIR/unar" "$STUBDIR/file" "$STUBDIR/img2pdf"

out=$(PATH="$STUBDIR:$PATH" UNAR_LOG="$UNAR_LOG" IMG2PDF_LOG="$IMG2PDF_LOG" \
    bash "$HERE/../scripts/batch_img2pdf" -o "$OUTDIR" "$MAINDIR" 2>"$WORKDIR/stderr.log")
rc=$?

check "batch: script exits 0 despite a failed archive" "0" "$rc"
check "batch: unar was attempted" "yes" \
    "$(grep -q 'archive\.zip$' "$UNAR_LOG" && echo yes || echo no)"
check "batch: PDF loop still ran (img2pdf invoked)" "yes" \
    "$(grep -q 'page1\.jpg' "$IMG2PDF_LOG" && echo yes || echo no)"
check "batch: summary reports 1 pdf, 0 failed" "yes" \
    "$(printf '%s' "$out" | grep -q 'done: 1 pdfs, 0 failed' && echo yes || echo no)"
check "batch: warns on stderr for the failed archive" "yes" \
    "$(grep -q 'WARN: an archive failed to extract' "$WORKDIR/stderr.log" && echo yes || echo no)"

# --- C3: --clean must not delete folders holding unconverted nested images ----
# Layout: book/cover.jpg + book/chapter1/p1.jpg. Only cover.jpg goes into the
# PDF (the image scan is -maxdepth 1), so rm -rf book/ would destroy chapter1/.
c3=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-c3.XXXXXX")
mkdir -p "$c3/main/book/chapter1"
: >"$c3/main/book/cover.jpg"
: >"$c3/main/book/chapter1/p1.jpg"
( cd "$c3" && PATH="$STUBDIR:$PATH" UNAR_LOG=/dev/null IMG2PDF_LOG=/dev/null \
    bash "$HERE/../scripts/batch_img2pdf" --clean -o "$c3/out" main ) \
    >"$c3/stdout.log" 2>"$c3/stderr.log"
check "C3: nested unconverted image survives --clean" "yes" \
    "$([ -f "$c3/main/book/chapter1/p1.jpg" ] && echo yes || echo no)"
# C3's core complaint was that the run "looks like a clean success" -- the exit
# status and the `done: 1 pdfs, 0 failed` summary are both indistinguishable
# from a run that cleaned. This WARN is the ONLY user-facing signal that a
# folder was kept, so it needs pinning: the earlier `2>&1` discarded stderr and
# left the whole fix unasserted. (F4)
check "C3: the run still reports the PDF as a success on stdout" "yes" \
    "$(grep -q 'done: 1 pdfs, 0 failed' "$c3/stdout.log" && echo yes || echo no)"
check "C3: stderr WARNs that the folder was kept" "yes" \
    "$(grep -q 'WARN: kept main/book' "$c3/stderr.log" && echo yes || echo no)"
check "C3: the WARN names the uncovered subdirectory" "yes" \
    "$(grep -q 'main/book/chapter1' "$c3/stderr.log" && echo yes || echo no)"
rm -rf "$c3"

# --- C3 sanity: a fully-covered folder (no subdirs) is still cleaned --------
c3b=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-c3b.XXXXXX")
mkdir -p "$c3b/main/flat"
: >"$c3b/main/flat/p1.jpg"
( cd "$c3b" && PATH="$STUBDIR:$PATH" UNAR_LOG=/dev/null IMG2PDF_LOG=/dev/null \
    bash "$HERE/../scripts/batch_img2pdf" --clean -o "$c3b/out" main ) >/dev/null 2>&1
check "C3: fully-covered folder is still removed by --clean" "no" \
    "$([ -d "$c3b/main/flat" ] && echo yes || echo no)"
rm -rf "$c3b"

# --- F2: symlinks must not be invisible to the --clean coverage check ---------
# `find -type f` matches NEITHER a symlink-to-file nor a symlink-to-dir, so a
# symlink was missed by both sides of the check: the image scan skipped it (so
# it never reached the PDF) AND the `total` count skipped it, giving total == n
# -> "fully covered" -> rm -rf. The `-mindepth 1 -type d` probe doesn't see a
# symlinked dir either. Blast radius is bounded (rm -rf doesn't follow a
# symlink, so the target survives) but `unar` can emit symlinks from zips, and
# a --clean that deletes an entry the PDF never covered is the C3 defect again.
# Counting with `! -type d` includes links, so the folder is correctly KEPT.
for f2case in file dir; do
    f2=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-f2.XXXXXX")
    mkdir -p "$f2/main/book" "$f2/outside"
    : >"$f2/main/book/cover.jpg"
    : >"$f2/outside/real.jpg"
    if [ "$f2case" = file ]; then
        ln -s ../../outside/real.jpg "$f2/main/book/link.jpg"
        f2link="$f2/main/book/link.jpg"
    else
        ln -s ../../outside "$f2/main/book/linkdir"
        f2link="$f2/main/book/linkdir"
    fi
    ( cd "$f2" && PATH="$STUBDIR:$PATH" UNAR_LOG=/dev/null IMG2PDF_LOG=/dev/null \
        bash "$HERE/../scripts/batch_img2pdf" --clean -o "$f2/out" main ) >/dev/null 2>&1
    check "F2: folder holding an uncovered symlink-to-$f2case survives --clean" "yes" \
        "$([ -d "$f2/main/book" ] && echo yes || echo no)"
    check "F2: the symlink-to-$f2case itself survives" "yes" \
        "$([ -L "$f2link" ] && echo yes || echo no)"
    rm -rf "$f2"
done

# --- F3: the WARN must name what kept the folder ------------------------------
# `book/{cover.jpg, .DS_Store}` -> total=2, n=1 -> kept, and kept forever.
# That is the correct, safe direction and it stays -- but .DS_Store is
# ubiquitous in macOS-authored zips, so on that platform --clean may effectively
# never clean, and "it holds files the PDF does not cover" never told the user a
# dotfile was the reason. Naming the entries is the fix; an ignore-list is NOT,
# because it would make --clean delete a folder the coverage check did not prove
# safe -- the C3 data-loss defect. Keeping the folder is always the safe way to
# be wrong; the user can act on a name, and cannot act on silence.
f3=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-f3.XXXXXX")
mkdir -p "$f3/main/book"
: >"$f3/main/book/cover.jpg"
: >"$f3/main/book/.DS_Store"
( cd "$f3" && PATH="$STUBDIR:$PATH" UNAR_LOG=/dev/null IMG2PDF_LOG=/dev/null \
    bash "$HERE/../scripts/batch_img2pdf" --clean -o "$f3/out" main ) \
    >/dev/null 2>"$f3/stderr.log"
check "F3: folder holding a dotfile is still kept" "yes" \
    "$([ -d "$f3/main/book" ] && echo yes || echo no)"
check "F3: the WARN names the dotfile that kept it" "yes" \
    "$(grep -q '\.DS_Store' "$f3/stderr.log" && echo yes || echo no)"
check "F3: the WARN does not name the image the PDF did cover" "yes" \
    "$(grep -q 'cover\.jpg' "$f3/stderr.log" && echo no || echo yes)"
rm -rf "$f3"

# --- M11: -h must print usage and exit, not set a sentinel ---------------------
# `-h` set MAINDIR="__help__" and kept parsing, so the `*) MAINDIR="$1"`
# positional branch silently overwrote it: `--clean -h book` -- a user reaching
# for help ON THE DESTRUCTIVE FLAG -- failed the `__help__` check and ran the
# full pipeline against book/ with CLEAN=1. Order-dependent too: `book -h` DID
# print usage. Every sibling script does `-h|--help) usage; exit 0 ;;` inline.
m11=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-m11.XXXXXX")
mkdir -p "$m11/main/flat"
: >"$m11/main/flat/p1.jpg"
m11out=$( cd "$m11" && PATH="$STUBDIR:$PATH" UNAR_LOG=/dev/null IMG2PDF_LOG=/dev/null \
    bash "$HERE/../scripts/batch_img2pdf" --clean -h main 2>/dev/null )
m11rc=$?
check "M11: '--clean -h DIR' prints usage" "yes" \
    "$(printf '%s' "$m11out" | grep -q 'Usage: batch_img2pdf' && echo yes || echo no)"
check "M11: '--clean -h DIR' exits 0" "0" "$m11rc"
check "M11: '--clean -h DIR' does NOT run the destructive pipeline" "yes" \
    "$([ -f "$m11/main/flat/p1.jpg" ] && echo yes || echo no)"

# The other order already worked; it must keep working.
m11out2=$( cd "$m11" && PATH="$STUBDIR:$PATH" bash "$HERE/../scripts/batch_img2pdf" main -h 2>/dev/null )
check "M11: 'DIR -h' still prints usage" "yes" \
    "$(printf '%s' "$m11out2" | grep -q 'Usage: batch_img2pdf' && echo yes || echo no)"

# And a directory legitimately named __help__ must be processed, not treated as
# a request for help -- the sentinel made its name magic.
mkdir -p "$m11/__help__/flat"
: >"$m11/__help__/flat/p1.jpg"
m11out3=$( cd "$m11" && PATH="$STUBDIR:$PATH" UNAR_LOG=/dev/null IMG2PDF_LOG=/dev/null \
    bash "$HERE/../scripts/batch_img2pdf" -o "$m11/out3" __help__ 2>/dev/null )
check "M11: a directory named __help__ is processed, not treated as -h" "yes" \
    "$(printf '%s' "$m11out3" | grep -q 'done: 1 pdfs, 0 failed' && echo yes || echo no)"
rm -rf "$m11"

# --- M9: the pids expansion must survive a run with no zips -------------------
# `batch_img2pdf DIR` where DIR holds already-extracted image folders and no
# *.zip is a normal workflow: the glob doesn't expand, `break` fires, and `pids`
# stays empty. Line 59's bare "${pids[@]}" under `set -u` then aborts on bash
# 4.3 and earlier -- macOS's 3.2 included -- BEFORE any PDF is built. The author
# already knows the idiom: select_streams guards with "${args[*]:-}", and
# `images`, `error_files` and `audio_streams` are all length-guarded; `pids` was
# the one that was missed.
check "M9: no nounset-unsafe \"\${pids[@]}\" expansion (bash < 4.4 / macOS 3.2)" "0" \
    "$(nounset_unsafe_expansions "$HERE/../scripts/batch_img2pdf" pids)"

# Companion to the pin above. This cannot go RED on bash >= 4.4 (see
# nounset_unsafe_expansions), but it does guard the FIX: the finding's suggested
# `[[ ${#pids[@]} -gt 0 ]] || return` would return from main() and skip the PDF
# loop entirely, turning a portability fix into a "builds nothing" bug. This
# asserts the no-zip run still reaches the PDF loop and still waits on real pids.
m9=$(mktemp -d "${TMPDIR:-/tmp}/img2pdf-m9.XXXXXX")
mkdir -p "$m9/main/flat"
: >"$m9/main/flat/p1.jpg"
m9out=$( cd "$m9" && PATH="$STUBDIR:$PATH" UNAR_LOG=/dev/null IMG2PDF_LOG="$m9/img2pdf.log" \
    bash "$HERE/../scripts/batch_img2pdf" -o "$m9/out" main 2>/dev/null )
check "M9: a directory with no zips still builds its PDFs" "yes" \
    "$(printf '%s' "$m9out" | grep -q 'done: 1 pdfs, 0 failed' && echo yes || echo no)"
check "M9: the PDF loop really ran (img2pdf invoked)" "yes" \
    "$(grep -q 'p1\.jpg' "$m9/img2pdf.log" && echo yes || echo no)"
rm -rf "$m9"

finish

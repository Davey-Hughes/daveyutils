#!/usr/bin/env bash
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
# shellcheck disable=SC1090
source "$HERE/../scripts/batch_img2pdf"

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
    bash "$HERE/../scripts/batch_img2pdf" --clean -o "$c3/out" main ) >/dev/null 2>&1
check "C3: nested unconverted image survives --clean" "yes" \
    "$([ -f "$c3/main/book/chapter1/p1.jpg" ] && echo yes || echo no)"
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

finish

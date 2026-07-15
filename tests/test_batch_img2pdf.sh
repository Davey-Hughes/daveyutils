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

cat > "$STUBDIR/file" <<'STUB'
#!/usr/bin/env bash
printf 'image/jpeg\n'
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

finish

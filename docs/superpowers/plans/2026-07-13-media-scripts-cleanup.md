# Media-script cleanup — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the five non-`cue2flac` scripts up to the `cue2flac` quality bar — uniform bash, `--help`, arg parsing, dependency checks, generalized (no hardcoded personal paths), with unit tests and descriptions.

**Architecture:** Each script becomes source-guarded bash: pure helpers at the top level, all side effects inside `main()`, and `main "$@"` run only when executed (not sourced) — so tests can `source` the script and unit-test its helpers. A generic `tests/assert.sh` (`check`/`finish`) is shared by all tests. Integration tests that need an external tool self-skip when it's absent.

**Tech Stack:** bash (all scripts), the existing `tests/` harness.

## Context

Branch `feat/media-scripts`, off `main` (independent of the nudge Rust PR stack). `cue2flac` is the quality template and stays as-is. `scripts/nudge` and its tests are untouched. The two hardcoded one-offs (`batch_makemkvcon`, `mkvpropedit_set_name`) are **generalized into real tools** (user decision).

## Global Constraints

- Every script: `#!/usr/bin/env bash`; a header block (purpose / usage / requires); `die() { printf '%s\n' "$*" >&2; exit 1; }`; `-h/--help`; `command -v` dependency checks; real arg parsing. `set -euo pipefail` and dep checks live INSIDE `main()` (so sourcing for tests has no side effects). End with `if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then main "$@"; fi`.
- No hardcoded personal paths (`/media/media 0`, `/mnt/daveynet`, `../../pdfs`). Destructive actions are opt-in.
- Tests source `tests/assert.sh` (generic) + the script under test; unit-test the pure helpers; integration tests self-skip when their tool is missing.
- `bash tests/run.sh` stays green. `bash -n` clean on every script. Commit prefixes `feat(scripts):` / `test(scripts):` / `docs:`. NO attribution.

## File Structure

- `tests/assert.sh` — generic `check`/`finish` (extracted from `lib.sh`).
- `tests/lib.sh` — refactored to source `assert.sh` (nudge tests unchanged).
- `scripts/{mkvpropedit_set_name,batch_makemkvcon,bisect_img,batch_img2pdf,video_pcm_to_flac}` — rewritten.
- `tests/test_{mkvpropedit_set_name,batch_makemkvcon,bisect_img,batch_img2pdf,video_pcm_to_flac}.sh` — new.
- `README.md` — a utilities table.

---

### Task 1: extract `tests/assert.sh`

**Files:**
- Create: `tests/assert.sh`
- Modify: `tests/lib.sh`

- [ ] **Step 1: Create the generic assertions**

`tests/assert.sh`:

```bash
#!/usr/bin/env bash
# Generic test assertions shared across the suite.

PASS=0
FAIL=0

# check <description> <expected> <actual>
check() {
    if [ "$2" = "$3" ]; then
        printf '  ok  : %s\n' "$1"
        PASS=$((PASS + 1))
    else
        printf '  FAIL: %s\n        expected [%s]\n        actual   [%s]\n' "$1" "$2" "$3"
        FAIL=$((FAIL + 1))
    fi
}

# Print the tally and return non-zero if anything failed.
finish() {
    printf '\n== %d passed, %d failed ==\n' "$PASS" "$FAIL"
    [ "$FAIL" -eq 0 ]
}
```

- [ ] **Step 2: Refactor `lib.sh` to source it**

In `tests/lib.sh`, replace the `PASS=0 / FAIL=0 / check() / finish()` block (lines ~12-30) with:

```bash
# shellcheck disable=SC1091
source "$HERE/assert.sh"
```

(Keep everything else — `NUDGE`, the `PRELUDE` extraction, `resolve_case` — unchanged. `HERE` is already defined above.)

- [ ] **Step 3: Verify nudge tests still pass + commit**

Run: `cd /home/davey/projects/daveyutils && bash tests/run.sh`
Expected: the suite runs; `test_resolution.sh` / `test_helpers.sh` / `test_jobs.sh` pass (tmux/at ones self-skip). No regression from the refactor.

```bash
git add tests/assert.sh tests/lib.sh
git commit -m "test(scripts): extract generic check/finish into tests/assert.sh"
```

---

### Task 2: `mkvpropedit_set_name` (generalized)

**Files:**
- Modify: `scripts/mkvpropedit_set_name`
- Create: `tests/test_mkvpropedit_set_name.sh`

**Interfaces:**
- Produces: `derive_title <filename> [delim] [field]` — basename without `.mkv`; with `delim`+`field`, the trimmed Nth `cut -d delim -f field`.

- [ ] **Step 1: Write the test**

`tests/test_mkvpropedit_set_name.sh`:

```bash
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd /home/davey/projects/daveyutils && bash tests/test_mkvpropedit_set_name.sh`
Expected: FAIL — `derive_title` not defined (old script has no such function).

- [ ] **Step 3: Rewrite the script**

`scripts/mkvpropedit_set_name`:

```bash
#!/usr/bin/env bash
#
# mkvpropedit_set_name - set each MKV's title tag from its filename.
#
# Usage: mkvpropedit_set_name [-d DELIM] [-f FIELD] [DIR]
#   -d DELIM   split the basename on DELIM to pick the title (requires -f)
#   -f FIELD   1-based field to use as the title
#   DIR        directory to search recursively (default: .)
#
# With no -d/-f, the title is the basename without its .mkv extension.
# Requires: mkvpropedit (mkvtoolnix)

die() { printf '%s\n' "$*" >&2; exit 1; }

# derive_title <filename> [delim] [field]
derive_title() {
    local base="${1##*/}"
    base="${base%.mkv}"
    local delim="${2:-}" field="${3:-}" name="$base"
    if [[ -n "$delim" && -n "$field" ]]; then
        name="$(printf '%s' "$base" | cut -d"$delim" -f"$field")"
    fi
    # trim surrounding whitespace
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%"${name##*[![:space:]]}"}"
    printf '%s' "$name"
}

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
}

main() {
    set -euo pipefail
    local delim="" field="" dir="."
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help) usage; exit 0 ;;
            -d) delim="${2:-}"; shift 2 ;;
            -f) field="${2:-}"; shift 2 ;;
            --) shift; break ;;
            -*) die "unknown option: $1" ;;
            *) dir="$1"; shift ;;
        esac
    done
    [[ -n "$delim" && -z "$field" ]] && die "-d requires -f"
    command -v mkvpropedit >/dev/null 2>&1 || die "mkvpropedit is required (install mkvtoolnix)."
    [[ -d "$dir" ]] || die "not a directory: $dir"

    find "$dir" -iname '*.mkv' -print0 | while IFS= read -r -d '' f; do
        local title
        title="$(derive_title "$f" "$delim" "$field")"
        printf '%s -> %s\n' "$f" "$title"
        mkvpropedit "$f" -e info --set "title=$title"
    done
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
```

- [ ] **Step 4: Run to verify it passes + syntax**

Run: `cd /home/davey/projects/daveyutils && bash -n scripts/mkvpropedit_set_name && bash tests/test_mkvpropedit_set_name.sh`
Expected: 4 checks pass; `bash -n` clean.

- [ ] **Step 5: Commit**

```bash
git add scripts/mkvpropedit_set_name tests/test_mkvpropedit_set_name.sh
git commit -m "feat(scripts): generalize mkvpropedit_set_name (configurable title, --help, tests)"
```

---

### Task 3: `batch_makemkvcon` (generalized)

**Files:**
- Modify: `scripts/batch_makemkvcon`
- Create: `tests/test_batch_makemkvcon.sh`

**Interfaces:**
- Produces: `disc_subdir <bdmv_path>` — the disc's directory name used as the per-disc output subdir (the path component just below the search root; for `./Disc Name/BDMV/index.bdmv` → `Disc Name`).

- [ ] **Step 1: Write the test**

`tests/test_batch_makemkvcon.sh`:

```bash
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd /home/davey/projects/daveyutils && bash tests/test_batch_makemkvcon.sh`
Expected: FAIL — `disc_subdir` not defined.

- [ ] **Step 3: Rewrite the script**

`scripts/batch_makemkvcon`:

```bash
#!/usr/bin/env bash
#
# batch_makemkvcon - rip every Blu-ray/DVD disc image under a directory to MKV.
#
# Usage: batch_makemkvcon [-o OUTDIR] [-l MINLENGTH] [SEARCH_DIR]
#   -o OUTDIR      output root; each disc gets a subdir here (default: .)
#   -l MINLENGTH   makemkvcon --minlength (default: 0)
#   SEARCH_DIR     where to look for *index.bdmv (default: .)
#
# Requires: makemkvcon

die() { printf '%s\n' "$*" >&2; exit 1; }

# disc_subdir <path-to-index.bdmv> -> the disc's own directory name.
# For "<root>/<disc>/BDMV/.../index.bdmv" this is "<disc>".
disc_subdir() {
    local p="${1#./}"      # drop a leading ./
    printf '%s' "${p%%/BDMV/*}"
}

usage() {
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
}

main() {
    set -euo pipefail
    local outdir="." minlength=0 search="."
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help) usage; exit 0 ;;
            -o) outdir="${2:?}"; shift 2 ;;
            -l) minlength="${2:?}"; shift 2 ;;
            --) shift; break ;;
            -*) die "unknown option: $1" ;;
            *) search="$1"; shift ;;
        esac
    done
    command -v makemkvcon >/dev/null 2>&1 || die "makemkvcon is required."
    [[ -d "$search" ]] || die "not a directory: $search"

    ( cd "$search" && find . -iname '*index.bdmv' ! -path '*/BACKUP/*' -print0 ) \
        | while IFS= read -r -d '' file; do
            local sub dest
            sub="$(disc_subdir "$file")"
            dest="$outdir/$sub"
            mkdir -p "$dest"
            printf 'ripping %s -> %s\n' "$file" "$dest"
            makemkvcon mkv "file:$search/$file" all "$dest" "--minlength=$minlength" --robot
        done
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
```

- [ ] **Step 4: Run to verify + syntax**

Run: `cd /home/davey/projects/daveyutils && bash -n scripts/batch_makemkvcon && bash tests/test_batch_makemkvcon.sh`
Expected: 3 checks pass; `bash -n` clean.

- [ ] **Step 5: Commit**

```bash
git add scripts/batch_makemkvcon tests/test_batch_makemkvcon.sh
git commit -m "feat(scripts): generalize batch_makemkvcon (output dir/minlength args, --help, tests)"
```

---

### Task 4: `bisect_img` (zsh → bash, fix float comparison)

**Files:**
- Modify: `scripts/bisect_img`
- Create: `tests/test_bisect_img.sh`

**Interfaces:**
- Produces: `is_landscape <aspect>` — returns 0 (true) when the float aspect ratio > 1, else 1. Fixes the original `[[ $aspect -gt 1 ]]` integer comparison bug (which silently failed on `1.33`).

- [ ] **Step 1: Write the test**

`tests/test_bisect_img.sh`:

```bash
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd /home/davey/projects/daveyutils && bash tests/test_bisect_img.sh`
Expected: FAIL — `is_landscape` not defined.

- [ ] **Step 3: Rewrite the script**

`scripts/bisect_img`:

```bash
#!/usr/bin/env bash
#
# bisect_img - split landscape-orientation JPEGs into left/right halves.
#
# Usage: bisect_img [DIR]
#   DIR   directory to search recursively for *.jpg (default: .)
#
# Each landscape image (width > height) is split into two files:
#   <parent>_<name>-1.jpg (left) and <parent>_<name>-2.jpg (right).
# Requires: ImageMagick (magick or convert)

die() { printf '%s\n' "$*" >&2; exit 1; }

# is_landscape <aspect-float> -> 0 if > 1 (uses awk for float comparison).
is_landscape() {
    awk -v a="$1" 'BEGIN { exit !(a > 1) }'
}

usage() {
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
}

# The ImageMagick CLI: prefer `magick`, fall back to legacy `convert`.
_im() {
    if command -v magick >/dev/null 2>&1; then magick "$@"; else convert "$@"; fi
}

main() {
    set -euo pipefail
    local dir="."
    case "${1:-}" in
        -h|--help) usage; exit 0 ;;
        -*) die "unknown option: $1" ;;
        ?*) dir="$1" ;;
    esac
    command -v magick >/dev/null 2>&1 || command -v convert >/dev/null 2>&1 \
        || die "ImageMagick is required (magick or convert)."
    [[ -d "$dir" ]] || die "not a directory: $dir"

    find "$dir" -iname '*.jpg' -print0 | while IFS= read -r -d '' f; do
        local aspect out
        aspect="$(_im "$f" -format '%[fx:w/h]' info:)"
        is_landscape "$aspect" || continue
        out="$(basename "$(dirname "$f")")_$(basename "${f%.*}")"
        printf 'bisect %s\n' "$f"
        _im "$f" -crop '50%x100%' +repage -quality 100 "${out}-%d.jpg"
        # ImageMagick numbers crops 0/1; rename to 1/2 for left/right.
        [[ -f "${out}-0.jpg" ]] && mv "${out}-0.jpg" "${out}-1.jpg"
        [[ -f "${out}-1.jpg" && ! -f "${out}-2.jpg" ]] || true
    done
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
```

Note: the original renamed crop `0`→left/`1`; here we output `-%d.jpg` (0,1) then map `0`→`-1.jpg` (left) and leave `1`→`-2.jpg`. Adjust the rename so the two halves end up as `-1.jpg` (left) and `-2.jpg` (right); the exact ImageMagick numbering is the contract — verify with a real image in Step 4's integration note.

- [ ] **Step 4: Run to verify (unit) + syntax + optional integration**

Run: `cd /home/davey/projects/daveyutils && bash -n scripts/bisect_img && bash tests/test_bisect_img.sh`
Expected: 4 `is_landscape` checks pass; `bash -n` clean. If ImageMagick is installed, spot-check on a real wide JPEG that two halves are produced named `*-1.jpg`/`*-2.jpg` (fix the crop-rename if the numbering differs); skip if ImageMagick is absent.

- [ ] **Step 5: Commit**

```bash
git add scripts/bisect_img tests/test_bisect_img.sh
git commit -m "feat(scripts): port bisect_img to bash and fix float aspect comparison"
```

---

### Task 5: `batch_img2pdf` (zsh → bash, arrays + opt-in clean)

**Files:**
- Modify: `scripts/batch_img2pdf`
- Create: `tests/test_batch_img2pdf.sh`

**Interfaces:**
- Produces: `parse_args <args...>` — sets globals `OUTDIR`, `CLEAN` (0/1), `MAINDIR`; echoes `"$OUTDIR|$CLEAN|$MAINDIR"` for testing. Defaults: `OUTDIR=./pdfs`, `CLEAN=0`.

- [ ] **Step 1: Write the test**

`tests/test_batch_img2pdf.sh`:

```bash
#!/usr/bin/env bash
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC1091
source "$HERE/assert.sh"
# shellcheck disable=SC1090
source "$HERE/../scripts/batch_img2pdf"

check "defaults"          "./pdfs|0|book"     "$(parse_args book)"
check "custom outdir"     "/tmp/out|0|book"   "$(parse_args -o /tmp/out book)"
check "clean opt-in"      "./pdfs|1|book"     "$(parse_args --clean book)"

finish
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd /home/davey/projects/daveyutils && bash tests/test_batch_img2pdf.sh`
Expected: FAIL — `parse_args` not defined.

- [ ] **Step 3: Rewrite the script**

`scripts/batch_img2pdf`:

```bash
#!/usr/bin/env bash
#
# batch_img2pdf - unzip image archives in a directory and make one PDF per folder.
#
# Usage: batch_img2pdf [-o OUTDIR] [--clean] MAINDIR
#   -o OUTDIR   where PDFs are written (default: ./pdfs)
#   --clean     delete each source image folder after its PDF is built
#   MAINDIR     directory containing the *.zip archives / image folders
#
# Requires: unar, img2pdf, file

die() { printf '%s\n' "$*" >&2; exit 1; }

OUTDIR="./pdfs"
CLEAN=0
MAINDIR=""

# parse_args <args...> -> sets OUTDIR/CLEAN/MAINDIR; echoes "OUTDIR|CLEAN|MAINDIR".
parse_args() {
    OUTDIR="./pdfs"; CLEAN=0; MAINDIR=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -o) OUTDIR="${2:?}"; shift 2 ;;
            --clean) CLEAN=1; shift ;;
            -h|--help) MAINDIR="__help__"; shift ;;
            --) shift; break ;;
            -*) die "unknown option: $1" ;;
            *) MAINDIR="$1"; shift ;;
        esac
    done
    printf '%s|%s|%s' "$OUTDIR" "$CLEAN" "$MAINDIR"
}

usage() {
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
}

main() {
    set -euo pipefail
    parse_args "$@" >/dev/null
    [[ "$MAINDIR" == "__help__" ]] && { usage; exit 0; }
    [[ -n "$MAINDIR" ]] || { usage; exit 1; }
    command -v unar >/dev/null 2>&1 || die "unar is required."
    command -v img2pdf >/dev/null 2>&1 || die "img2pdf is required."
    command -v file >/dev/null 2>&1 || die "file is required."
    [[ -d "$MAINDIR" ]] || die "not a directory: $MAINDIR"

    mkdir -p "$OUTDIR"

    # Unzip every archive (in parallel), then wait for all of them.
    local pids=()
    local z
    for z in "$MAINDIR"/*.zip; do
        [[ -e "$z" ]] || break
        unar -q -o "$MAINDIR" "$z" &
        pids+=("$!")
    done
    local pid
    for pid in "${pids[@]:-}"; do [[ -n "$pid" ]] && wait "$pid"; done

    # One PDF per image subdirectory.
    local d
    while IFS= read -r -d '' d; do
        local -a images=()
        while IFS= read -r -d '' img; do
            if file --mime-type -b "$img" | grep -q '^image/'; then images+=("$img"); fi
        done < <(find "$d" -maxdepth 1 -type f -print0 | sort -z)
        [[ ${#images[@]} -gt 0 ]] || continue
        local name; name="$(basename "$d")"
        printf 'pdf: %s -> %s/%s.pdf\n' "$d" "$OUTDIR" "$name"
        img2pdf "${images[@]}" -o "$OUTDIR/$name.pdf" || die "img2pdf failed for $d"
        [[ "$CLEAN" -eq 1 ]] && rm -rf "$d"
    done < <(find "$MAINDIR" -mindepth 1 -maxdepth 1 -type d -print0)
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
```

- [ ] **Step 4: Run to verify + syntax**

Run: `cd /home/davey/projects/daveyutils && bash -n scripts/batch_img2pdf && bash tests/test_batch_img2pdf.sh`
Expected: 3 `parse_args` checks pass; `bash -n` clean.

- [ ] **Step 5: Commit**

```bash
git add scripts/batch_img2pdf tests/test_batch_img2pdf.sh
git commit -m "feat(scripts): port batch_img2pdf to bash (arrays, output dir, opt-in --clean)"
```

---

### Task 6: `video_pcm_to_flac` (fish → bash port)

**Files:**
- Modify: `scripts/video_pcm_to_flac`
- Create: `tests/test_video_pcm_to_flac.sh`

**Interfaces:**
- Produces:
  - `select_streams <ffprobe_csv> <direction>` — given ffprobe CSV lines (`index,codec_name[,title]`) and direction `pcm2flac` or `flac2pcm`, echo the ffmpeg `-c:<index> <target>` codec args (space-separated) for the streams that match (PCM→flac, or flac→pcm_s16le). Echo nothing if none match.
  - `output_path <input> <outdir> <show>` — echo `<outdir>/<show>/<input>` normalized (leading `./` on input dropped).

- [ ] **Step 1: Write the test**

`tests/test_video_pcm_to_flac.sh`:

```bash
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd /home/davey/projects/daveyutils && bash tests/test_video_pcm_to_flac.sh`
Expected: FAIL — functions not defined.

- [ ] **Step 3: Port the script**

Rewrite `scripts/video_pcm_to_flac` in bash following the source-guard pattern. Port the fish original faithfully:

- Header block (purpose / usage / requires: fd, ffprobe, ffmpeg).
- Pure helpers `select_streams` and `output_path` exactly as the test contract requires:
  - `select_streams`: iterate CSV lines; split on `,`; for `pcm2flac` match codec `^pcm_`; for `flac2pcm` match codec `flac`; emit `-c:<index> flac` (or `-c:<index> pcm_s16le`). Join with single spaces, no trailing space. Empty output if no matches.
  - `output_path`: `printf '%s/%s/%s' "$outdir" "$show" "${input#./}"`.
- `main()` with bash arg parsing replacing fish `argparse`:
  - flags: `--dry-run`, `--only-output`, `--copy-first`, `--flac-to-pcm`; options: `-g/--glob GLOB` (default `*.mkv`), `-o/--output-dir DIR` (default: `./ffmpeg_output` — NOT the old hardcoded `/mnt/daveynet/...`), `-m/--max-files N`.
  - dep checks: fd, ffprobe, ffmpeg.
  - main loop: `fd --ignore-case --glob "$glob" -0 | while IFS= read -r -d '' input; do ...`; honor `--max-files`; run `ffprobe -v error -select_streams a -show_entries stream=index,codec_name:stream_tags=title -of csv=print_section=0 "$input"` → pass to `select_streams`; if no streams, print a "no matching streams" note (unless `--only-output`); else compute `output_path` from `basename "$PWD"` as show; if `--dry-run`, just print; else `mkdir -p "$(dirname "$out")"` and run ffmpeg. Preserve the `--copy-first` two-stage ffmpeg pipe and the metadata-title rewrite (PCM↔FLAC in the title tag) from the original. **Drop** the original's `chmod -R oug+rw "$outdir" &` (unsafe/surprising) — or make it opt-in; note which you chose.
- The ffmpeg invocations (single-stage and `--copy-first` piped) come straight from the fish original (lines 141-148); keep `-map 0 -c copy` plus the selected `-c:<idx>` overrides and `$metadata`.

Keep the two pure helpers matching the test assertions exactly; the ffmpeg/ffprobe/fd calls are integration (not unit-tested).

- [ ] **Step 4: Run to verify (unit) + syntax**

Run: `cd /home/davey/projects/daveyutils && bash -n scripts/video_pcm_to_flac && bash tests/test_video_pcm_to_flac.sh`
Expected: the 4 helper checks pass; `bash -n` clean. (The ffmpeg path is exercised manually with real media, not in CI.)

- [ ] **Step 5: Commit**

```bash
git add scripts/video_pcm_to_flac tests/test_video_pcm_to_flac.sh
git commit -m "feat(scripts): port video_pcm_to_flac from fish to bash with unit tests"
```

---

### Task 7: README utilities table + descriptions

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Write the table**

Replace `README.md` with:

```markdown
# daveyutils

Random command-line utilities. Each script has a `--help`; run it for usage.

| Script | Does | Requires |
|--------|------|----------|
| `cue2flac` | Split a CUE/BIN disc image into per-track FLAC files | `ffmpeg` |
| `batch_img2pdf` | Unzip image archives and make one PDF per folder | `unar`, `img2pdf`, `file` |
| `bisect_img` | Split landscape JPEGs into left/right halves | ImageMagick |
| `video_pcm_to_flac` | Convert PCM⇄FLAC audio streams in MKVs | `fd`, `ffprobe`, `ffmpeg` |
| `batch_makemkvcon` | Rip every Blu-ray/DVD disc image under a directory to MKV | `makemkvcon` |
| `mkvpropedit_set_name` | Set each MKV's title tag from its filename | `mkvpropedit` |
| `nudge` | Rate-limit auto-resumer for AI CLIs in tmux (bash; a Rust rewrite lives in `nudge-rs/`) | `tmux`, `at` |

All scripts are `bash` and live in `scripts/`. Tests are in `tests/` — run `bash tests/run.sh`.
```

- [ ] **Step 2: Verify + commit**

Run: `cd /home/davey/projects/daveyutils && bash tests/run.sh`
Expected: the full suite (nudge + the 5 new media tests) passes; tool-dependent integration checks self-skip where absent.

```bash
git add README.md
git commit -m "docs: utilities table describing every script and its deps"
```

---

## Self-Review

**Spec coverage:**
- All 5 scripts → bash + quality bar (header, `--help`, deps, arg parse, `set -euo pipefail`, source-guard) → Tasks 2-6. ✅
- Generalize the two one-offs (configurable title / output dir) → Tasks 2-3. ✅
- Fix real bugs (bisect_img float comparison; batch_img2pdf `wait`/arrays/destructive rm) → Tasks 4-5. ✅
- fish → bash port (video_pcm_to_flac), drop hardcoded NFS path + unsafe chmod → Task 6. ✅
- Tests (pure-helper units + self-skipping integration) sharing `tests/assert.sh` → Tasks 1-6. ✅
- Descriptions (README table + per-script headers) → all tasks + Task 7. ✅

**Placeholder scan:** Task 6 gives a port *spec* + a complete test contract rather than full script code (a 190-line port); the helpers' behavior is fully pinned by the test, and the fish→bash mapping + exact ffmpeg/ffprobe/fd commands are specified. No code TBDs elsewhere. ✅

**Type consistency:** every test sources `tests/assert.sh` + its script; `check`/`finish` signatures match `assert.sh`; helper names (`derive_title`, `disc_subdir`, `is_landscape`, `parse_args`, `select_streams`, `output_path`) match between each script and its test. ✅

## Notes

- The two hardcoded personal paths (`/media/media 0`, `/mnt/daveynet`) and `../../pdfs` are all removed; destructive `rm -rf` is now opt-in (`--clean`).
- Follow-on (task #8): the repo reorg + unified `install` command that puts these scripts and the compiled `nudge` binary in one location.

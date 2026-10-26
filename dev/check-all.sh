#!/usr/bin/env bash
# The local mirror of CI (.forgejo/workflows/ci.yml, and its macOS-only counterpart in
# .github/workflows/tests.yml + nudge-rs.yml + subcheck-rs.yml). Run it before you push a branch and
# open a PR: CI runs these same jobs there, and this is them several minutes earlier.
#
#   dev/check-all.sh            # everything CI gates on
#   dev/check-all.sh --no-rust  # bash only; skip the Rust legs
#
# The point of this file is that the local gate and the CI gate cannot drift: CI's gating jobs are
# reproduced here in the same order with the same commands. When a job changes there, change it here.
#
# `make check` is NOT this gate and never was — it runs the bash suite and the Rust crates' tests,
# but not rustfmt, not clippy, and not the syntax sweep. Landing on `make check` alone would miss
# three things CI fails on.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# Parse up front so a typo (`--no-rus`) fails immediately rather than silently falling through to a
# full run — a flag that quietly does the opposite of what was asked is the same class of bug as a
# gate that quietly covers less than it claims.
no_rust=0
case "${1:-}" in
  "")        ;;
  --no-rust) no_rust=1 ;;
  *)         echo "error: unknown argument: $1" >&2; exit 2 ;;
esac
[ "$#" -le 1 ] || { echo "error: too many arguments" >&2; exit 2; }

run() { echo; echo "==> $*"; "$@"; }

# --- the `bash test-suite` job: syntax sweep, then the suite ---
# Sourcing a script is a side effect, not a syntax check: a file that no test file sources can carry
# a stray quote or a missing `fi` all the way to main with CI green. This is the check that is
# actually about syntax, so it sweeps every script, covered by a test or not.
echo; echo "==> bash -n over scripts/, tests/ and dev/"
for f in scripts/*; do [ -f "$f" ] && bash -n "$f"; done
for f in tests/*.sh; do [ -f "$f" ] && bash -n "$f"; done
for f in dev/*.sh; do [ -f "$f" ] && bash -n "$f"; done
echo "    $(ls scripts | wc -l) scripts, $(ls tests/*.sh | wc -l) test files, $(ls dev/*.sh | wc -l) dev scripts — syntax clean"

# As in CI: usage's tests fail, rather than skip, without jq.
USAGE_NEED_JQ=1 run bash tests/run.sh

# --- the `nudge-rs` and `subcheck-rs` jobs: the same four legs for each crate ---
crates=(nudge-rs subcheck-rs)
if [ "$no_rust" -eq 1 ]; then
  echo; echo "==> SKIPPED: ${crates[*]} (--no-rust)"
  echo "    CI still runs them. Do not land on the strength of this run alone."
else
  # As in CI: subcheck's `apply` tests fail, rather than skip, without mkvmerge, and its `audio`
  # tests without ffmpeg.
  export SUBCHECK_NEED_MKVMERGE=1 SUBCHECK_NEED_FFMPEG=1
  for crate in "${crates[@]}"; do
    manifest=$crate/Cargo.toml
    run cargo fmt --manifest-path "$manifest" --all --check
    run cargo clippy --manifest-path "$manifest" --all-targets -- -D warnings
    # nextest when present, matching `make check`'s real path in CI; cargo test otherwise. Doctests
    # are run explicitly either way — nextest cannot run them, so relying on it alone would drop them.
    if cargo nextest --version >/dev/null 2>&1; then
      run cargo nextest run --manifest-path "$manifest"
    else
      echo; echo "note: cargo-nextest not installed, using cargo test (slower)" >&2
      run cargo test --manifest-path "$manifest"
    fi
    run cargo test --manifest-path "$manifest" --doc
  done
fi

# --- the `make` job ---
# `make all` is gated in CI because nothing else drives the Makefile, and a `make` that exits 0 while
# installing a non-functional bin/ ships green — which is how the CARGO_TARGET_DIR dangling-symlink
# bug got out. bin/ is repo-local and gitignored, so running this here touches nothing outside the
# working copy.
if [ "$no_rust" -eq 1 ]; then
  echo; echo "==> SKIPPED: make all (needs the Rust builds)"
else
  run make all
fi

echo
echo "all green"

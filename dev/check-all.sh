#!/usr/bin/env bash
# The local mirror of CI (.github/workflows/tests.yml + nudge-rs.yml). Run it before landing a
# branch — dev/land.sh runs it for you, on the merged tree, before the commit exists.
#
#   dev/check-all.sh            # everything CI gates on
#   dev/check-all.sh --no-rust  # bash only; skip the nudge-rs leg
#
# The point of this file is that the local gate and the CI gate cannot drift: CI's gating jobs are
# reproduced here in the same order with the same commands. When a job changes there, change it here.
#
# `make check` is NOT this gate and never was — it runs the bash suite and nudge-rs's tests, but not
# rustfmt, not clippy, and not the syntax sweep. Landing on `make check` alone would miss three
# things CI fails on.
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

run bash tests/run.sh

# --- the `nudge-rs` job ---
if [ "$no_rust" -eq 1 ]; then
  echo; echo "==> SKIPPED: nudge-rs (--no-rust)"
  echo "    CI still runs it. Do not land on the strength of this run alone."
else
  run cargo fmt --manifest-path nudge-rs/Cargo.toml --all --check
  run cargo clippy --manifest-path nudge-rs/Cargo.toml --all-targets -- -D warnings
  # nextest when present, matching `make check`'s real path in CI; cargo test otherwise. Doctests are
  # run explicitly either way — nextest cannot run them, so relying on it alone would drop them.
  if cargo nextest --version >/dev/null 2>&1; then
    run cargo nextest run --manifest-path nudge-rs/Cargo.toml
  else
    echo; echo "note: cargo-nextest not installed, using cargo test (slower)" >&2
    run cargo test --manifest-path nudge-rs/Cargo.toml
  fi
  run cargo test --manifest-path nudge-rs/Cargo.toml --doc
fi

# --- the `make` job ---
# `make all` is gated in CI because nothing else drives the Makefile, and a `make` that exits 0 while
# installing a non-functional bin/ ships green — which is how the CARGO_TARGET_DIR dangling-symlink
# bug got out. bin/ is repo-local and gitignored, so running this here touches nothing outside the
# working copy.
if [ "$no_rust" -eq 1 ]; then
  echo; echo "==> SKIPPED: make all (needs the nudge-rs build)"
else
  run make all
fi

echo
echo "all green"

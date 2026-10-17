# daveyutils — collect every runnable utility into ./bin (gitignored).
#
#   make            build nudge and subcheck and link them into ./bin
#   make all        that, plus a symlink for every script in scripts/
#   make clean      remove ./bin
#   make distclean  remove ./bin and the Rust build artifacts
#   make check      run the test suites
#
# Put ./bin on your PATH once and every utility here is available:
#
#   export PATH="/path/to/daveyutils/bin:$PATH"
#
# The bash scripts are SYMLINKED, so editing one takes effect immediately.
# `nudge` and `subcheck` are the Rust binaries from nudge-rs/ and subcheck-rs/,
# built and linked separately below.

BIN := bin
# Pin cargo's output dirs. An inherited CARGO_TARGET_DIR would otherwise put the
# binaries somewhere else, leaving bin/nudge dangling while make still reported
# success -- so we pass --target-dir explicitly rather than trusting the default.
TARGET_DIR := nudge-rs/target
NUDGE      := $(TARGET_DIR)/release/nudge
SUBCHECK_TARGET_DIR := subcheck-rs/target
SUBCHECK            := $(SUBCHECK_TARGET_DIR)/release/subcheck
SCRIPTS := $(wildcard scripts/*)
CRATES  := nudge-rs subcheck-rs

.PHONY: all bins nudge subcheck link link-nudge link-subcheck link-scripts build-nudge build-subcheck path-hint clean distclean check help

# Bare `make` builds and links the compiled utilities only. They are the ones
# you re-run after a change; the scripts are symlinks that never go stale.
# `make all` relinks those too, which is what a first install (or a new script
# in scripts/) wants.
.DEFAULT_GOAL := bins

# These goals are ordered steps, not independent work -- under -j the PATH hint
# would race ahead of the links it is describing.
.NOTPARALLEL:

## bins: (default) build nudge and subcheck and link them into ./bin
bins: link-nudge link-subcheck path-hint

## nudge: build nudge alone and link it into ./bin
nudge: link-nudge path-hint

## subcheck: build subcheck alone and link it into ./bin
subcheck: link-subcheck path-hint

## all: build both binaries and symlink every utility into ./bin
all: link

# Kept as the old name for "link everything", so `make link` still works.
link: link-nudge link-subcheck link-scripts path-hint

link-nudge: build-nudge | $(BIN)
	@ln -sfn "../$(NUDGE)" "$(BIN)/nudge"
	@test -e "$(BIN)/nudge" || { printf 'error: %s/nudge is a dangling symlink (no binary at %s)\n' "$(BIN)" "$(NUDGE)" >&2; exit 1; }
	@echo "  link  $(BIN)/nudge -> ../$(NUDGE)"

link-subcheck: build-subcheck | $(BIN)
	@ln -sfn "../$(SUBCHECK)" "$(BIN)/subcheck"
	@test -e "$(BIN)/subcheck" || { printf 'error: %s/subcheck is a dangling symlink (no binary at %s)\n' "$(BIN)" "$(SUBCHECK)" >&2; exit 1; }
	@echo "  link  $(BIN)/subcheck -> ../$(SUBCHECK)"

link-scripts: | $(BIN)
	@for s in $(SCRIPTS); do \
		ln -sfn "../$$s" "$(BIN)/$$(basename $$s)" && \
		echo "  link  $(BIN)/$$(basename $$s) -> ../$$s"; \
	done

path-hint:
	@echo
	@echo "Add to your PATH:"
	@echo "  export PATH=\"$(CURDIR)/$(BIN):\$$PATH\""

$(BIN):
	@mkdir -p $(BIN)

## build-nudge: cargo build --release the nudge binary
build-nudge:
	@cargo build --release --manifest-path nudge-rs/Cargo.toml --target-dir "$(TARGET_DIR)"
	@test -x "$(NUDGE)" || { printf 'error: cargo reported success but there is no binary at %s\n' "$(NUDGE)" >&2; exit 1; }

## build-subcheck: cargo build --release the subcheck binary
build-subcheck:
	@cargo build --release --manifest-path subcheck-rs/Cargo.toml --target-dir "$(SUBCHECK_TARGET_DIR)"
	@test -x "$(SUBCHECK)" || { printf 'error: cargo reported success but there is no binary at %s\n' "$(SUBCHECK)" >&2; exit 1; }

## clean: remove ./bin (leaves the Rust build cache alone)
clean:
	@rm -rf $(BIN)
	@echo "removed $(BIN)"

## distclean: remove ./bin and the Rust build artifacts
distclean: clean
	@cargo clean --manifest-path nudge-rs/Cargo.toml --target-dir "$(TARGET_DIR)"
	@cargo clean --manifest-path subcheck-rs/Cargo.toml --target-dir "$(SUBCHECK_TARGET_DIR)"
	@echo "removed nudge-rs and subcheck-rs build artifacts"

## check: run the bash test-suite and every crate's Rust tests
#
# nextest when it is available: it pools every test from every binary instead of
# running the binaries one at a time, which on this suite measured ~2x faster.
#
# The `--doc` run is not optional garnish. rustdoc compiles doctests separately
# and nextest DOES NOT RUN THEM -- `cargo test` was running them as a side
# effect, so swapping the runner alone would drop that coverage silently. There
# are zero doctests today, which is exactly why the pairing goes in now: the
# first one written must not be the one that discovers this.
#
# The fallback branch needs no `--doc` of its own; plain `cargo test` includes
# doctests already.
check:
	@bash tests/run.sh
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		for c in $(CRATES); do \
			cargo nextest run --manifest-path $$c/Cargo.toml && \
			cargo test --manifest-path $$c/Cargo.toml --doc || exit 1; \
		done; \
	else \
		printf 'note: cargo-nextest not installed, using cargo test (slower)\n' >&2; \
		for c in $(CRATES); do \
			cargo test --manifest-path $$c/Cargo.toml || exit 1; \
		done; \
	fi

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'

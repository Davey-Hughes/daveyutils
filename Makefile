# daveyutils — collect every runnable utility into ./bin (gitignored).
#
#   make            build nudge and link it into ./bin
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
# `nudge` is the Rust binary from nudge-rs/, built and linked separately below.

BIN := bin
# Pin cargo's output dir. An inherited CARGO_TARGET_DIR would otherwise put the
# binary somewhere else, leaving bin/nudge dangling while make still reported
# success -- so we pass --target-dir explicitly rather than trusting the default.
TARGET_DIR := nudge-rs/target
NUDGE      := $(TARGET_DIR)/release/nudge
SCRIPTS := $(wildcard scripts/*)

.PHONY: all nudge link link-nudge link-scripts build-nudge path-hint clean distclean check help

# Bare `make` builds and links nudge only. It is the one utility here that has
# to be compiled, so it is the one you re-run after a change; the scripts are
# symlinks that never go stale. `make all` relinks those too, which is what a
# first install (or a new script in scripts/) wants.
.DEFAULT_GOAL := nudge

# These goals are ordered steps, not independent work -- under -j the PATH hint
# would race ahead of the links it is describing.
.NOTPARALLEL:

## nudge: (default) build nudge and link it into ./bin
nudge: link-nudge path-hint

## all: build nudge and symlink every utility into ./bin
all: link

# Kept as the old name for "link everything", so `make link` still works.
link: link-nudge link-scripts path-hint

link-nudge: build-nudge | $(BIN)
	@ln -sfn "../$(NUDGE)" "$(BIN)/nudge"
	@test -e "$(BIN)/nudge" || { printf 'error: %s/nudge is a dangling symlink (no binary at %s)\n' "$(BIN)" "$(NUDGE)" >&2; exit 1; }
	@echo "  link  $(BIN)/nudge -> ../$(NUDGE)"

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

## clean: remove ./bin (leaves the Rust build cache alone)
clean:
	@rm -rf $(BIN)
	@echo "removed $(BIN)"

## distclean: remove ./bin and the Rust build artifacts
distclean: clean
	@cargo clean --manifest-path nudge-rs/Cargo.toml --target-dir "$(TARGET_DIR)"
	@echo "removed nudge-rs build artifacts"

## check: run the bash test-suite and the Rust tests
check:
	@bash tests/run.sh
	@cargo test --manifest-path nudge-rs/Cargo.toml

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'

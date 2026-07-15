# daveyutils — collect every runnable utility into ./bin (gitignored).
#
#   make            build nudge and link everything into ./bin
#   make clean      remove ./bin
#   make distclean  remove ./bin and the Rust build artifacts
#   make check      run the test suites
#
# Put ./bin on your PATH once and every utility here is available:
#
#   export PATH="/path/to/daveyutils/bin:$PATH"
#
# The bash scripts are SYMLINKED, so editing one takes effect immediately.
# `nudge` is the Rust binary from nudge-rs/ (the bash scripts/nudge is kept as
# the reference oracle for the port and is deliberately NOT linked).

BIN     := bin
NUDGE   := nudge-rs/target/release/nudge
# Every script except the bash nudge (superseded by the Rust binary).
SCRIPTS := $(filter-out scripts/nudge,$(wildcard scripts/*))

.PHONY: all link build-nudge clean distclean check help

all: link

## link: build nudge, then symlink every utility into ./bin
link: build-nudge | $(BIN)
	@for s in $(SCRIPTS); do \
		ln -sfn "../$$s" "$(BIN)/$$(basename $$s)" && \
		echo "  link  $(BIN)/$$(basename $$s) -> ../$$s"; \
	done
	@ln -sfn "../$(NUDGE)" "$(BIN)/nudge" && echo "  link  $(BIN)/nudge -> ../$(NUDGE)"
	@echo
	@echo "Add to your PATH:"
	@echo "  export PATH=\"$(CURDIR)/$(BIN):\$$PATH\""

$(BIN):
	@mkdir -p $(BIN)

## build-nudge: cargo build --release the nudge binary
build-nudge:
	@cargo build --release --manifest-path nudge-rs/Cargo.toml

## clean: remove ./bin (leaves the Rust build cache alone)
clean:
	@rm -rf $(BIN)
	@echo "removed $(BIN)"

## distclean: remove ./bin and the Rust build artifacts
distclean: clean
	@cargo clean --manifest-path nudge-rs/Cargo.toml
	@echo "removed nudge-rs build artifacts"

## check: run the bash test-suite and the Rust tests
check:
	@bash tests/run.sh
	@cargo test --manifest-path nudge-rs/Cargo.toml

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'

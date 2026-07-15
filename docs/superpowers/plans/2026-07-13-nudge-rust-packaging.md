# nudge Rust rewrite — packaging (Phase 1, increment 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make nudge installable: shell completions (`clap_complete`), a Homebrew formula, AUR PKGBUILDs (source + `-git`), and a `nudge-rs` README — completing Phase 1.

**Architecture:** A `nudge --completions <shell>` flag prints a completion script via `clap_complete` (testable). Distribution files (`packaging/homebrew/nudge.rb`, `packaging/aur/nudge/PKGBUILD`, `packaging/aur/nudge-git/PKGBUILD`) are static templates that build the crate and install the binary + completions; they're reviewed by inspection (not run in CI). A README documents install + usage.

**Tech Stack:** Rust 2021, adds `clap_complete`; plus static packaging files (Ruby formula, PKGBUILD shell, Markdown).

## Context

Increment 5, stacked on `feat/nudge-rust-cli` (increment 4, PR #10). The nudge CLI is functionally complete; this makes it distributable. The packaging files reference `nudge --completions <shell>` (added in Task 1) to install completions.

Distribution defaults (flag to the user; adjust as desired):
- AUR: `nudge` (source, from a `v$pkgver` release tag) + `nudge-git` (from the repo). `provides/conflicts` each other.
- Homebrew: formula in-repo (`packaging/homebrew/nudge.rb`), `--HEAD`-installable now; a `url`+`sha256` for a tagged release is filled in on first release.
- Runtime dep: `tmux`. License: GPL-3.0-or-later. Repo: `https://github.com/Davey-Hughes/daveyutils`.

## Global Constraints

- Crate at `nudge-rs/`, edition 2021. Add `clap_complete = "4"`. No other crates.
- Packaging files live under `packaging/` at the repo root. They are NOT executed by CI or tests — reviewed by inspection. The only testable code is completion generation.
- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` pass. Commit prefixes `feat/docs(nudge-rs): …`; NO attribution.

## File Structure

- `nudge-rs/Cargo.toml` — add `clap_complete`.
- `nudge-rs/src/cli.rs` — a `--completions <SHELL>` flag.
- `nudge-rs/src/app.rs` — `write_completions` / `print_completions`.
- `nudge-rs/src/lib.rs` — handle `--completions` in `run`.
- `packaging/homebrew/nudge.rb` — Homebrew formula.
- `packaging/aur/nudge/PKGBUILD` — AUR source package.
- `packaging/aur/nudge-git/PKGBUILD` — AUR `-git` package.
- `nudge-rs/README.md` — tool README.

---

### Task 1: shell completions (`--completions <shell>`)

**Files:**
- Modify: `nudge-rs/Cargo.toml` (add `clap_complete`)
- Modify: `nudge-rs/src/cli.rs` (add the flag)
- Modify: `nudge-rs/src/app.rs` (`write_completions` / `print_completions`)
- Modify: `nudge-rs/src/lib.rs` (handle it in `run`)

**Interfaces:**
- Produces:
  - `cli::Cli.completions: Option<clap_complete::Shell>` (`--completions <SHELL>`).
  - `app::write_completions<W: std::io::Write>(shell: clap_complete::Shell, w: &mut W)` — generate the script for `nudge`.
  - `app::print_completions(shell: clap_complete::Shell)` — write to stdout.

- [ ] **Step 1: Add the dep**

`nudge-rs/Cargo.toml` `[dependencies]`:

```toml
clap_complete = "4"
```

- [ ] **Step 2: Add the flag**

In `nudge-rs/src/cli.rs`, add to `Cli` (after the daemon flags):

```rust
    /// Print a shell completion script for SHELL (bash, zsh, fish, …) to stdout.
    #[arg(long = "completions", value_name = "SHELL")]
    pub completions: Option<clap_complete::Shell>,
```

- [ ] **Step 3: Write the failing test + impl in app.rs**

Add to `nudge-rs/src/app.rs`:

```rust
use clap::CommandFactory;

/// Write a shell completion script for the `nudge` binary.
pub fn write_completions<W: std::io::Write>(shell: clap_complete::Shell, w: &mut W) {
    clap_complete::generate(shell, &mut Cli::command(), "nudge", w);
}

/// Print a shell completion script to stdout.
pub fn print_completions(shell: clap_complete::Shell) {
    write_completions(shell, &mut std::io::stdout());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_completions_mention_the_binary() {
        let mut buf: Vec<u8> = Vec::new();
        write_completions(clap_complete::Shell::Bash, &mut buf);
        let script = String::from_utf8(buf).unwrap();
        assert!(script.contains("nudge"), "completion script should mention the binary");
        assert!(!script.is_empty());
    }

    #[test]
    fn zsh_and_fish_generate_nonempty_scripts() {
        for sh in [clap_complete::Shell::Zsh, clap_complete::Shell::Fish] {
            let mut buf: Vec<u8> = Vec::new();
            write_completions(sh, &mut buf);
            assert!(!buf.is_empty(), "{sh} script must be non-empty");
        }
    }
}
```

(If `app.rs` already has a `#[cfg(test)] mod tests`, add these two tests to it instead of a second module.)

- [ ] **Step 4: Handle it in `run`**

In `nudge-rs/src/lib.rs`, near the top of `run` (before the daemon check is fine; completions is a pure print):

```rust
    if let Some(shell) = cli.completions {
        app::print_completions(shell);
        return Ok(());
    }
```

- [ ] **Step 5: Verify + commit**

Run: `cd nudge-rs && cargo test app && cargo build && ./target/debug/nudge --completions bash | head -3 && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: the 2 completion tests pass; `nudge --completions bash` prints a script; clippy clean.

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/cli.rs nudge-rs/src/app.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): shell completions via --completions <shell>"
```

---

### Task 2: packaging files + README

**Files:**
- Create: `packaging/homebrew/nudge.rb`
- Create: `packaging/aur/nudge/PKGBUILD`
- Create: `packaging/aur/nudge-git/PKGBUILD`
- Create: `nudge-rs/README.md`

**Interfaces:** none (static files, inspection-reviewed).

- [ ] **Step 1: Homebrew formula**

`packaging/homebrew/nudge.rb`:

```ruby
class Nudge < Formula
  desc "Rate-limit auto-resumer for AI CLIs in tmux"
  homepage "https://github.com/Davey-Hughes/daveyutils"
  license "GPL-3.0-or-later"
  head "https://github.com/Davey-Hughes/daveyutils.git", branch: "main"

  # On the first tagged release, add:
  #   url "https://github.com/Davey-Hughes/daveyutils/archive/refs/tags/nudge-v0.1.0.tar.gz"
  #   sha256 "..."

  depends_on "rust" => :build
  depends_on "tmux"

  def install
    cd "nudge-rs" do
      system "cargo", "install", "--locked", "--root", prefix, "--path", "."
      generate_completions_from_executable(bin/"nudge", "--completions", shells: [:bash, :zsh, :fish])
    end
  end

  test do
    assert_match "nudge", shell_output("#{bin}/nudge --version")
  end
end
```

- [ ] **Step 2: AUR `-git` package (builds from the repo, works today)**

`packaging/aur/nudge-git/PKGBUILD`:

```bash
# Maintainer: Davey Hughes
pkgname=nudge-git
pkgver=r1
pkgrel=1
pkgdesc="Rate-limit auto-resumer for AI CLIs in tmux"
arch=('x86_64' 'aarch64')
url="https://github.com/Davey-Hughes/daveyutils"
license=('GPL-3.0-or-later')
depends=('tmux')
makedepends=('cargo' 'git')
provides=('nudge')
conflicts=('nudge')
source=("git+https://github.com/Davey-Hughes/daveyutils.git")
sha256sums=('SKIP')

pkgver() {
  cd "daveyutils"
  printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"
}

build() {
  cd "daveyutils/nudge-rs"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --release --locked
}

package() {
  cd "daveyutils/nudge-rs"
  install -Dm755 target/release/nudge "$pkgdir/usr/bin/nudge"
  install -Dm644 ../LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"

  install -dm755 "$pkgdir/usr/share/bash-completion/completions"
  target/release/nudge --completions bash > "$pkgdir/usr/share/bash-completion/completions/nudge"
  install -dm755 "$pkgdir/usr/share/zsh/site-functions"
  target/release/nudge --completions zsh > "$pkgdir/usr/share/zsh/site-functions/_nudge"
  install -dm755 "$pkgdir/usr/share/fish/vendor_completions.d"
  target/release/nudge --completions fish > "$pkgdir/usr/share/fish/vendor_completions.d/nudge.fish"
}
```

- [ ] **Step 3: AUR source package (activates on the first `nudge-v*` tag)**

`packaging/aur/nudge/PKGBUILD`:

```bash
# Maintainer: Davey Hughes
pkgname=nudge
pkgver=0.1.0
pkgrel=1
pkgdesc="Rate-limit auto-resumer for AI CLIs in tmux"
arch=('x86_64' 'aarch64')
url="https://github.com/Davey-Hughes/daveyutils"
license=('GPL-3.0-or-later')
depends=('tmux')
makedepends=('cargo')
provides=('nudge')
conflicts=('nudge-git')
# Release tags are named `nudge-v$pkgver`.
source=("$pkgname-$pkgver.tar.gz::https://github.com/Davey-Hughes/daveyutils/archive/refs/tags/nudge-v$pkgver.tar.gz")
sha256sums=('SKIP')  # replace SKIP with the tarball sha256 on release

build() {
  cd "daveyutils-nudge-v$pkgver/nudge-rs"
  export RUSTUP_TOOLCHAIN=stable
  cargo build --release --locked
}

package() {
  cd "daveyutils-nudge-v$pkgver/nudge-rs"
  install -Dm755 target/release/nudge "$pkgdir/usr/bin/nudge"
  install -Dm644 ../LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"

  install -dm755 "$pkgdir/usr/share/bash-completion/completions"
  target/release/nudge --completions bash > "$pkgdir/usr/share/bash-completion/completions/nudge"
  install -dm755 "$pkgdir/usr/share/zsh/site-functions"
  target/release/nudge --completions zsh > "$pkgdir/usr/share/zsh/site-functions/_nudge"
  install -dm755 "$pkgdir/usr/share/fish/vendor_completions.d"
  target/release/nudge --completions fish > "$pkgdir/usr/share/fish/vendor_completions.d/nudge.fish"
}
```

- [ ] **Step 4: nudge-rs README**

`nudge-rs/README.md`:

```markdown
# nudge

Rate-limit auto-resumer for AI CLIs (Claude Code, Antigravity) running in tmux.

nudge watches a tmux pane for a rate-limit banner ("resets 3:00pm" / "resets in
1h30m"), and re-injects your messages when the limit clears — via a small
resident daemon it manages itself. No `at`, no `fzf`, no coreutils; Linux + macOS.

## Install

**cargo**
```sh
cargo install --path nudge-rs
```

**Arch (AUR)** — `nudge` (release) or `nudge-git` (latest):
```sh
# from packaging/aur/nudge-git
makepkg -si
```

**Homebrew**
```sh
brew install --HEAD packaging/homebrew/nudge.rb
```

Shell completions: `nudge --completions bash|zsh|fish` (the packages install these
automatically).

## Usage

```sh
nudge -p bot:0.1                 # auto-detect the reset time from the pane
nudge -p bot:0.1 -m "14:30"      # explicit time
nudge -p bot:0.1 -a -r -1 -v     # auto-retry forever, verify before each send
nudge                            # interactive pane picker
nudge --list                     # pending jobs
nudge --cancel 3 / --edit 3      # manage a job
```

The daemon is auto-started on first schedule. To run it at login:

```sh
nudge --install-daemon           # register with systemd --user / launchd
```

## Development

```sh
cd nudge-rs
cargo test
cargo run -- --help
```

The bash predecessor lives at `scripts/nudge` (kept as the reference oracle).
```

- [ ] **Step 5: Verify (files exist; crate unaffected) + commit**

Run: `cd nudge-rs && cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: crate suite still green (these files don't touch the crate). Confirm the files exist: `ls ../packaging/homebrew/nudge.rb ../packaging/aur/nudge/PKGBUILD ../packaging/aur/nudge-git/PKGBUILD README.md`.

```bash
cd /home/davey/projects/daveyutils
git add packaging/ nudge-rs/README.md
git commit -m "docs(nudge-rs): Homebrew formula, AUR PKGBUILDs, and README"
```

---

## Self-Review

**Spec coverage (increment 5):**
- Shell completions (`clap_complete`) → Task 1, tested. ✅
- Homebrew formula → Task 2. ✅
- AUR PKGBUILDs (source + `-git`) installing binary + completions + license → Task 2. ✅
- README (install + usage) → Task 2. ✅
- Out of scope (Phase 1 done): the follow-on media-script cleanup and repo reorg + `install` command are separate efforts.

**Placeholder scan:** The formula/PKGBUILD `SKIP`/commented `url`+`sha256` are intentional release-time placeholders (documented inline), not plan gaps — the `-git` package and `--HEAD` formula are functional today. No code TBDs. ✅

**Type consistency:** `Cli.completions: Option<clap_complete::Shell>` consumed by `run`; `write_completions`/`print_completions` signatures consistent; `Cli::command()` needs `use clap::CommandFactory`. ✅

## Notes — Phase 1 complete after this

- First release: tag `nudge-v0.1.0`, add the tarball `sha256` to `packaging/aur/nudge/PKGBUILD` and the `url`+`sha256` to the Homebrew formula, and publish the AUR package(s).
- The deferred nudge polish items (interactive ratatui `--list` dashboard, local-zone time display, `conflicts_with` flag guards, error-chain display) remain tracked in the SDD ledger.
- Remaining daveyutils work (separate from nudge): the media-script cleanup and the repo reorg + unified `install` command (task #8).

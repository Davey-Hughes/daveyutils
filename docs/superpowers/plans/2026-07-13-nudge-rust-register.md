# nudge Rust rewrite — daemon registration (Phase 1, increment 3c)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Register the daemon with the OS's user service manager — generate a systemd `--user` unit (Linux) and a launchd LaunchAgent plist (macOS), with the actual enable step gated behind an install function that no test ever calls.

**Architecture:** All *generation* is pure and unit-tested: the unit/plist text, the file paths, and the command *plan* (which shell commands install would run). Only `register::install`/`uninstall` actually write files and run `systemctl`/`launchctl` — they are never invoked by tests, so nothing touches the host's service manager during the build. The wiring of a `nudge --install-daemon` CLI command onto `install()` is increment 4.

**Tech Stack:** Rust 2021, adds `plist` (serde plist generation for macOS); reuses `paths`, `serde`.

## Context

Increment 3c, stacked on `feat/nudge-rust-scheduler` (3b, PR #8). The generated services run `<exec> --daemon`; the `--daemon` flag itself is wired in increment 4 (the CLI), so 3c generates registration that references it in advance. Also folds in the 3a-review-deferred `paths::resolve()` unset-`$HOME` guard.

## Global Constraints

- Crate at `nudge-rs/`, edition 2021. Add `plist = "1"`. No other new crates.
- **No test may write a real unit/plist to `~/.config`/`~/Library`, run `systemctl`/`launchctl`, or otherwise touch the host service manager.** Tests cover only pure generation (text/plist/paths/command-plan). `install`/`uninstall` are effectful and untested.
- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` pass every commit. Commit prefixes `feat/refactor(nudge-rs): …`; NO attribution.

## File Structure

- `nudge-rs/Cargo.toml` — add `plist`.
- `nudge-rs/src/paths.rs` — guard `resolve()` against unset `$HOME`; add `config_dir`.
- `nudge-rs/src/register/mod.rs` — `Manager`, `InstallPlan`, `plan_for`, `install`/`uninstall`.
- `nudge-rs/src/register/systemd.rs` — unit text, unit path, systemd install plan.
- `nudge-rs/src/register/launchd.rs` — plist bytes, plist path, launchd install plan.
- `nudge-rs/src/lib.rs` — add `pub mod register;`.

---

### Task 1: systemd unit generation + `$HOME` guard

**Files:**
- Modify: `nudge-rs/src/paths.rs`
- Create: `nudge-rs/src/register/mod.rs`
- Create: `nudge-rs/src/register/systemd.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod register;`)

**Interfaces:**
- Produces:
  - `paths::config_dir(home: &Path, xdg_config: Option<&Path>) -> PathBuf` — `$XDG_CONFIG_HOME` or `home/.config`. And `resolve()` falls back to `.` only after logging when `$HOME` is unset (see step).
  - `register::InstallPlan { pub files: Vec<(std::path::PathBuf, String)>, pub commands: Vec<Vec<String>> }` (derives `Debug, PartialEq`).
  - `register::systemd::unit_text(exec: &Path) -> String`.
  - `register::systemd::unit_path(home: &Path, xdg_config: Option<&Path>) -> PathBuf` — `<config_dir>/systemd/user/nudged.service`.
  - `register::systemd::install_plan(exec: &Path, unit_path: &Path) -> InstallPlan`.

- [ ] **Step 1: Guard `$HOME` and add `config_dir` in paths.rs**

In `nudge-rs/src/paths.rs`, add:

```rust
/// The user config dir: `$XDG_CONFIG_HOME`, else `<home>/.config`.
pub fn config_dir(home: &Path, xdg_config: Option<&Path>) -> PathBuf {
    xdg_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"))
}
```

And change `resolve()`'s `home` line so an unset `$HOME` is visible rather than silently cwd-relative:

```rust
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("nudge: warning: $HOME is unset; using '.' for state paths");
            PathBuf::from(".")
        }
    };
```

- [ ] **Step 2: Write the failing tests**

`nudge-rs/src/register/systemd.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn unit_text_runs_the_daemon_and_is_installable() {
        let t = unit_text(Path::new("/usr/bin/nudge"));
        assert!(t.contains("ExecStart=/usr/bin/nudge --daemon"), "got:\n{t}");
        assert!(t.contains("[Install]"));
        assert!(t.contains("WantedBy=default.target"));
        assert!(t.contains("Restart=on-failure"));
    }

    #[test]
    fn unit_path_is_under_systemd_user() {
        let p = unit_path(Path::new("/home/d"), None);
        assert_eq!(p, Path::new("/home/d/.config/systemd/user/nudged.service"));
        let p2 = unit_path(Path::new("/home/d"), Some(Path::new("/cfg")));
        assert_eq!(p2, Path::new("/cfg/systemd/user/nudged.service"));
    }

    #[test]
    fn install_plan_writes_unit_and_enables() {
        let plan = install_plan(Path::new("/usr/bin/nudge"), Path::new("/home/d/.config/systemd/user/nudged.service"));
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].0, Path::new("/home/d/.config/systemd/user/nudged.service"));
        assert!(plan.files[0].1.contains("ExecStart=/usr/bin/nudge --daemon"));
        // daemon-reload then enable --now.
        assert!(plan.commands.contains(&vec![
            "systemctl".into(), "--user".into(), "daemon-reload".into()
        ]));
        assert!(plan.commands.contains(&vec![
            "systemctl".into(), "--user".into(), "enable".into(), "--now".into(), "nudged.service".into()
        ]));
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cd nudge-rs && cargo test systemd`
Expected: FAIL — module/functions missing.

- [ ] **Step 4: Implement**

`nudge-rs/src/register/mod.rs`:

```rust
//! Register (and unregister) the nudge daemon with the OS user service
//! manager. Generation is pure and tested; `install`/`uninstall` actually
//! touch the host and are never called by tests.

pub mod launchd;
pub mod systemd;

use std::path::PathBuf;

/// The concrete steps to register the daemon: files to write, then commands to
/// run.
#[derive(Debug, PartialEq)]
pub struct InstallPlan {
    pub files: Vec<(PathBuf, String)>,
    pub commands: Vec<Vec<String>>,
}
```

Prepend to `nudge-rs/src/register/systemd.rs`:

```rust
//! systemd `--user` service generation.

use std::path::{Path, PathBuf};

use super::InstallPlan;
use crate::paths::config_dir;

/// The `.service` unit text that runs `<exec> --daemon`.
pub fn unit_text(exec: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=nudge — rate-limit auto-resumer daemon\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} --daemon\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.display()
    )
}

/// `<config_dir>/systemd/user/nudged.service`.
pub fn unit_path(home: &Path, xdg_config: Option<&Path>) -> PathBuf {
    config_dir(home, xdg_config).join("systemd/user/nudged.service")
}

/// Files to write and commands to run to install the systemd unit.
pub fn install_plan(exec: &Path, unit_path: &Path) -> InstallPlan {
    InstallPlan {
        files: vec![(unit_path.to_path_buf(), unit_text(exec))],
        commands: vec![
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            vec![
                "systemctl".into(),
                "--user".into(),
                "enable".into(),
                "--now".into(),
                "nudged.service".into(),
            ],
        ],
    }
}
```

`nudge-rs/src/register/launchd.rs` (stub for now so `pub mod launchd;` compiles; filled in Task 2):

```rust
//! launchd LaunchAgent generation. (implemented in Task 2)
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod register;
```

- [ ] **Step 5: Run to verify they pass**

Run: `cd nudge-rs && cargo test systemd paths && cargo clippy --all-targets -- -D warnings`
Expected: systemd + paths tests PASS; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add nudge-rs/src/paths.rs nudge-rs/src/register/mod.rs nudge-rs/src/register/systemd.rs nudge-rs/src/register/launchd.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): systemd --user unit generation + $HOME guard"
```

---

### Task 2: launchd plist generation

**Files:**
- Modify: `nudge-rs/Cargo.toml` (add `plist`)
- Modify: `nudge-rs/src/register/launchd.rs` (replace the stub)

**Interfaces:**
- Produces:
  - `register::launchd::LABEL: &str` = `"com.nudge.daemon"`.
  - `register::launchd::plist_bytes(exec: &Path) -> Vec<u8>` — an XML plist with `Label`, `ProgramArguments = [exec, "--daemon"]`, `RunAtLoad = true`, `KeepAlive = true`.
  - `register::launchd::plist_path(home: &Path) -> PathBuf` — `<home>/Library/LaunchAgents/com.nudge.daemon.plist`.
  - `register::launchd::install_plan(exec: &Path, plist_path: &Path, uid: u32) -> InstallPlan` — write the plist, then `launchctl bootstrap gui/<uid> <plist_path>`.

- [ ] **Step 1: Add the dep**

In `nudge-rs/Cargo.toml` `[dependencies]`:

```toml
plist = "1"
```

- [ ] **Step 2: Write the failing tests**

Append to `nudge-rs/src/register/launchd.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plist_has_label_program_and_flags() {
        let bytes = plist_bytes(Path::new("/usr/local/bin/nudge"));
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.contains("com.nudge.daemon"), "got:\n{xml}");
        assert!(xml.contains("/usr/local/bin/nudge"));
        assert!(xml.contains("--daemon"));
        // RunAtLoad / KeepAlive true keys present.
        assert!(xml.contains("RunAtLoad"));
        assert!(xml.contains("KeepAlive"));
    }

    #[test]
    fn plist_path_is_under_launch_agents() {
        let p = plist_path(Path::new("/Users/d"));
        assert_eq!(p, Path::new("/Users/d/Library/LaunchAgents/com.nudge.daemon.plist"));
    }

    #[test]
    fn install_plan_bootstraps_the_agent() {
        let p = Path::new("/Users/d/Library/LaunchAgents/com.nudge.daemon.plist");
        let plan = install_plan(Path::new("/usr/local/bin/nudge"), p, 501);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].0, p);
        assert!(plan.commands.contains(&vec![
            "launchctl".into(), "bootstrap".into(), "gui/501".into(),
            "/Users/d/Library/LaunchAgents/com.nudge.daemon.plist".into()
        ]));
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cd nudge-rs && cargo test launchd`
Expected: FAIL — functions missing.

- [ ] **Step 4: Implement**

Prepend to `nudge-rs/src/register/launchd.rs` (replacing the stub doc line):

```rust
//! launchd LaunchAgent generation.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::InstallPlan;

pub const LABEL: &str = "com.nudge.daemon";

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LaunchAgent {
    label: String,
    program_arguments: Vec<String>,
    run_at_load: bool,
    keep_alive: bool,
}

/// XML plist that runs `<exec> --daemon` at load and keeps it alive.
pub fn plist_bytes(exec: &Path) -> Vec<u8> {
    let agent = LaunchAgent {
        label: LABEL.to_string(),
        program_arguments: vec![exec.display().to_string(), "--daemon".to_string()],
        run_at_load: true,
        keep_alive: true,
    };
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &agent).expect("serialize launchd plist");
    buf
}

/// `<home>/Library/LaunchAgents/com.nudge.daemon.plist`.
pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Files to write and commands to run to install the LaunchAgent.
pub fn install_plan(exec: &Path, plist_path: &Path, uid: u32) -> InstallPlan {
    let xml = String::from_utf8(plist_bytes(exec)).expect("plist is valid utf-8");
    InstallPlan {
        files: vec![(plist_path.to_path_buf(), xml)],
        commands: vec![vec![
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{uid}"),
            plist_path.display().to_string(),
        ]],
    }
}
```

- [ ] **Step 5: Run to verify they pass**

Run: `cd nudge-rs && cargo test launchd && cargo clippy --all-targets -- -D warnings`
Expected: launchd tests PASS; clippy clean. (If `plist::to_writer_xml`'s exact name differs, adjust to the real `plist` 1.x API that writes an XML plist to a `Vec<u8>`; keep the test assertions.)

- [ ] **Step 6: Commit**

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/register/launchd.rs
git commit -m "feat(nudge-rs): launchd LaunchAgent plist generation"
```

---

### Task 3: OS dispatch + gated `install`/`uninstall`

**Files:**
- Modify: `nudge-rs/src/register/mod.rs`

**Interfaces:**
- Produces:
  - `register::Manager` — `enum { Systemd, Launchd }`; `Manager::current() -> Manager` (macos → Launchd, else Systemd).
  - `register::plan_for(manager: Manager, exec: &Path, home: &Path, xdg_config: Option<&Path>, uid: u32) -> InstallPlan` — pure dispatch to the systemd/launchd `install_plan`.
  - `register::install(exec: &Path) -> anyhow::Result<()>` — resolve the current OS + env, write the plan's files, run its commands, print a `loginctl enable-linger` hint on Linux. **Effectful; never called by tests.**
  - `register::uninstall() -> anyhow::Result<()>` — disable/bootout and remove the unit/plist. **Effectful; never called by tests.**

- [ ] **Step 1: Write the failing test (pure dispatch only)**

Append a test module to `nudge-rs/src/register/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plan_for_systemd_writes_a_unit_and_enables() {
        let plan = plan_for(Manager::Systemd, Path::new("/usr/bin/nudge"), Path::new("/home/d"), None, 1000);
        assert!(plan.files[0].0.ends_with("systemd/user/nudged.service"));
        assert!(plan.commands.iter().any(|c| c.contains(&"enable".to_string())));
    }

    #[test]
    fn plan_for_launchd_writes_a_plist_and_bootstraps() {
        let plan = plan_for(Manager::Launchd, Path::new("/usr/local/bin/nudge"), Path::new("/Users/d"), None, 501);
        assert!(plan.files[0].0.ends_with("Library/LaunchAgents/com.nudge.daemon.plist"));
        assert!(plan.commands.iter().any(|c| c.contains(&"bootstrap".to_string())));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd nudge-rs && cargo test register`
Expected: FAIL — `Manager`/`plan_for` missing.

- [ ] **Step 3: Implement**

Add to `nudge-rs/src/register/mod.rs` (below `InstallPlan`):

```rust
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    pub fn current() -> Manager {
        if cfg!(target_os = "macos") {
            Manager::Launchd
        } else {
            Manager::Systemd
        }
    }
}

/// Pure dispatch: the install plan for a given manager.
pub fn plan_for(
    manager: Manager,
    exec: &Path,
    home: &Path,
    xdg_config: Option<&Path>,
    uid: u32,
) -> InstallPlan {
    match manager {
        Manager::Systemd => {
            let unit = systemd::unit_path(home, xdg_config);
            systemd::install_plan(exec, &unit)
        }
        Manager::Launchd => {
            let plist = launchd::plist_path(home);
            launchd::install_plan(exec, &plist, uid)
        }
    }
}

/// Write the plan's files and run its commands. EFFECTFUL — touches the host
/// service manager. Only ever called from an explicit CLI opt-in, never tests.
pub fn install(exec: &Path) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set"))?;
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from);
    let uid = current_uid();
    let plan = plan_for(Manager::current(), exec, &home, xdg_config.as_deref(), uid);

    for (path, contents) in &plan.files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, contents)?;
        println!("nudge: wrote {}", path.display());
    }
    for cmd in &plan.commands {
        let (prog, args) = cmd.split_first().expect("non-empty command");
        let status = std::process::Command::new(prog).args(args).status()?;
        if !status.success() {
            anyhow::bail!("`{}` failed with {status}", cmd.join(" "));
        }
    }
    if Manager::current() == Manager::Systemd {
        println!(
            "nudge: if this is a headless / SSH session, run once:\n  \
             loginctl enable-linger $USER"
        );
    }
    println!("nudge: daemon registered.");
    Ok(())
}

/// Disable and remove the daemon registration. EFFECTFUL; never called by tests.
pub fn uninstall() -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set"))?;
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from);
    match Manager::current() {
        Manager::Systemd => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", "nudged.service"])
                .status();
            let unit = systemd::unit_path(&home, xdg_config.as_deref());
            let _ = std::fs::remove_file(&unit);
            println!("nudge: removed {}", unit.display());
        }
        Manager::Launchd => {
            let uid = current_uid();
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{}", launchd::LABEL)])
                .status();
            let plist = launchd::plist_path(&home);
            let _ = std::fs::remove_file(&plist);
            println!("nudge: removed {}", plist.display());
        }
    }
    Ok(())
}

/// Current user's uid (via `id -u`, portable across Linux/macOS without libc).
fn current_uid() -> u32 {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1000)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd nudge-rs && cargo test register && cargo clippy --all-targets -- -D warnings`
Expected: `register::tests::*` PASS; clippy clean. (`install`/`uninstall` are not exercised by any test.)

- [ ] **Step 5: Full suite, lint, commit**

Run: `cd nudge-rs && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: everything green; no test wrote to `~/.config`/`~/Library` or ran `systemctl`/`launchctl`.

```bash
git add nudge-rs/src/register/mod.rs
git commit -m "feat(nudge-rs): OS-dispatched daemon install/uninstall (gated, generation tested)"
```

---

## Self-Review

**Spec coverage (3c slice):**
- systemd `--user` unit generation → Task 1. ✅
- launchd LaunchAgent plist generation → Task 2. ✅
- OS dispatch + gated real enable (`install`/`uninstall`), `loginctl enable-linger` hint → Task 3. ✅
- `$HOME` unset guard (3a-review-deferred) → Task 1. ✅
- Hermetic: only pure generation/paths/plan tested; `install`/`uninstall` never called by tests → all tasks. ✅
- Out of 3c (→ 4): wiring `nudge --install-daemon`/`--uninstall-daemon`/`--daemon` in the CLI; the deferred 3b daemon fixes (retry-interval floor, swallowed-error logging, `init_tracing` in the entrypoint).

**Placeholder scan:** No TBD/TODO; every code step complete; the launchd stub in Task 1 is explicitly replaced in Task 2. ✅

**Type consistency:** `InstallPlan { files, commands }` consistent across mod/systemd/launchd/tests. `unit_path`/`plist_path`/`config_dir` signatures match their tests. `plan_for(Manager, &Path, &Path, Option<&Path>, u32)` matches Task 3 tests. `install`/`uninstall` return `anyhow::Result<()>` (anyhow already a dependency from increment 2). ✅

## Notes for the next increment (4 — CLI)

- `clap`: `nudge --daemon` → `daemon::init_tracing()` + `daemon::run(&paths::resolve()-derived, …)`; `nudge --install-daemon`/`--uninstall-daemon` → `register::install(std::env::current_exe()?)` / `register::uninstall()`; `schedule`/`--list`/`--cancel`/`--edit` → `ipc::client` calls; the ratatui picker; notifications (`notify-rust`).
- Fix WITH the CLI: daemon retry-interval floor (`max(settle_secs, ~1s)`) + fractional-settle handling; swap `let _ =` on `apply_outcome`/`remove` for `tracing::warn!` on Err; call `init_tracing()` in the `--daemon` entrypoint.

# Remediation Increment 1 — criticals (data loss)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the three CRITICAL findings from the whole-repo review — the only ones that silently lose user data — plus one cheap test-hygiene fix.

**Architecture:** The two Rust criticals share one root cause: nothing enforces a single daemon. Fix it in depth — an advisory `flock` held for the daemon's lifetime (authoritative, crash-safe), a connect-probe so a live socket is never stolen (good errors, closes the deterministic path), and a pid-unique queue temp file so two writers can never share it. The bash critical is a guard so `--clean` can only delete a folder the PDF actually covered.

**Tech Stack:** Rust (adds `fs4` for advisory file locking), bash.

## Context

Branch `fix/critical-data-loss`, off `main` (`560dc27`). Findings and their full failure scenarios: `docs/superpowers/reviews/2026-07-15-whole-repo-review-findings.md` (C1, C2, C3). Read that file first — it has the verified reproduction paths.

## Global Constraints

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean; `bash tests/run.sh` green; `bash -n` clean on changed scripts.
- Every fix needs a regression test that FAILS against the current code. Prove it: run the new test before the fix and capture the failure.
- Do NOT run `batch_img2pdf` with `--clean` against anything you care about; use throwaway tempdirs only.
- Commit prefixes `fix(nudge-rs):` / `fix(scripts):` / `test(scripts):`. NO attribution lines.

## File Structure

- `nudge-rs/Cargo.toml` — add `fs4`.
- `nudge-rs/src/daemon.rs` — acquire/hold the singleton lock.
- `nudge-rs/src/ipc/server.rs` — probe before unlinking.
- `nudge-rs/src/queue.rs` — pid-unique temp file.
- `nudge-rs/src/register/mod.rs` — don't enable the unit over a live ad-hoc daemon.
- `scripts/batch_img2pdf` — `--clean` coverage guard.
- `tests/test_jobs_e2e.sh` — purge only what the test created.

---

### Task 1: daemon singleton (C1 + C2)

**Files:**
- Modify: `nudge-rs/Cargo.toml`, `nudge-rs/src/daemon.rs`, `nudge-rs/src/ipc/server.rs`, `nudge-rs/src/queue.rs`
- Create: `nudge-rs/tests/daemon_singleton.rs`

**Interfaces:**
- Produces:
  - `daemon::acquire_singleton_lock(state_dir: &std::path::Path) -> std::io::Result<std::fs::File>` — creates/opens `<state_dir>/nudge.lock` and takes an EXCLUSIVE, NON-BLOCKING advisory lock. Returns the `File` (the lock lives as long as it is held open — the caller MUST keep it alive for the daemon's lifetime). Errors with `ErrorKind::WouldBlock`/`AlreadyExists` semantics if another process holds it.
  - `ipc::server::serve` — unchanged signature, but MUST NOT steal a live socket (see below).

- [ ] **Step 1: Add the lock dependency**

`nudge-rs/Cargo.toml` `[dependencies]`:

```toml
fs4 = "0.13"
```

(If that version/API doesn't resolve, use the nearest `fs4` release, or `fs2 = "0.4"` — both expose a `FileExt` trait with `try_lock_exclusive`. The tests below are the contract; adapt the call.)

- [ ] **Step 2: Write the failing tests**

`nudge-rs/tests/daemon_singleton.rs`:

```rust
//! The daemon must be a singleton: a second one must neither take the lock nor
//! steal a live socket. Hermetic — tempdir lock/socket, no real daemon spawned.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use nudge::daemon::acquire_singleton_lock;
use nudge::queue::Queue;

#[test]
fn second_lock_attempt_fails_while_first_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let first = acquire_singleton_lock(dir.path()).expect("first lock should succeed");
    assert!(
        acquire_singleton_lock(dir.path()).is_err(),
        "a second daemon must NOT be able to take the lock while the first holds it"
    );
    drop(first);
    // Once released, a new daemon can take it.
    assert!(
        acquire_singleton_lock(dir.path()).is_ok(),
        "lock must be reusable after the holder exits"
    );
}

#[test]
fn serve_refuses_to_steal_a_live_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");

    // A "live daemon" already listening on the socket.
    let live = UnixListener::bind(&socket).unwrap();

    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
    let err = nudge::ipc::server::serve(&socket, queue)
        .expect_err("serve must refuse to bind over a live socket instead of stealing it");
    let _ = err;

    // The original listener must still own a working socket.
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "the live daemon's socket must still be connectable — it was stolen/unlinked"
    );
    drop(live);
}

#[test]
fn serve_reclaims_a_stale_socket_file() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    // A stale socket file with nobody listening (previous daemon crashed).
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    // serve() should reclaim it. Run it briefly on a thread; it loops forever,
    // so just prove the socket becomes connectable (i.e. it bound successfully).
    let queue = Arc::new(Mutex::new(Queue::load(dir.path().join("q.json")).unwrap()));
    let s = socket.clone();
    std::thread::spawn(move || {
        let _ = nudge::ipc::server::serve(&s, queue);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&socket).is_ok() {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ok, "serve must reclaim a stale socket file and bind");
}
```

Add a unit test to `nudge-rs/src/queue.rs`'s test module proving the temp file is not a shared fixed name:

```rust
    #[test]
    fn save_uses_a_process_unique_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut q = Queue::load(path.clone()).unwrap();
        q.add(spec()).unwrap();
        // The old fixed sibling name must not be what we write: two daemons
        // sharing `q.json.tmp` could truncate each other mid-write.
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the fixed shared temp name must not be left behind"
        );
        // And no temp files should survive a successful save.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }
```

- [ ] **Step 3: Run to verify they fail (RED)**

Run: `cd nudge-rs && cargo test --test daemon_singleton; cargo test save_uses_a_process_unique_temp_file`
Expected: `acquire_singleton_lock` doesn't exist (compile error), and `serve_refuses_to_steal_a_live_socket` fails — today `serve` unlinks and rebinds happily. Capture this output; it is the proof the tests bite.

- [ ] **Step 4: Implement the lock**

In `nudge-rs/src/daemon.rs`:

```rust
/// Take the daemon singleton lock: an exclusive, non-blocking advisory lock on
/// `<state_dir>/nudge.lock`. The returned File MUST be held for the daemon's
/// lifetime — dropping it releases the lock. The OS releases it automatically if
/// the process dies, so a crashed daemon never wedges the next one.
pub fn acquire_singleton_lock(state_dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    use fs4::fs_std::FileExt;
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("nudge.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    file.try_lock_exclusive().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!("another nudge daemon already holds {}: {e}", path.display()),
        )
    })?;
    Ok(file)
}
```

(Adapt the `FileExt` import/method to the `fs4` version that resolves — the test is the contract.)

Then in `run`, take the lock FIRST and keep it alive for the whole function:

```rust
    // Refuse to start a second daemon: two schedulers on one queue.json double-fire
    // jobs and clobber each other's state.
    let _lock = acquire_singleton_lock(&paths.state_dir).map_err(|e| {
        tracing::error!("nudge: not starting: {e}");
        e
    })?;
```

Bind `_lock` (not `_`) so it is not dropped immediately.

- [ ] **Step 5: Implement the socket probe**

In `nudge-rs/src/ipc/server.rs`, replace the unconditional `let _ = std::fs::remove_file(socket);` at the top of `serve`:

```rust
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Never steal a live socket. If something answers, another daemon owns it.
    // Only a socket nobody is listening on is stale and safe to reclaim.
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another nudge daemon is already listening on {}", socket.display()),
                ));
            }
            Err(_) => {
                tracing::warn!("nudge ipc: reclaiming stale socket {}", socket.display());
                std::fs::remove_file(socket)?;
            }
        }
    }
    let listener = UnixListener::bind(socket)?;
```

- [ ] **Step 6: Implement the pid-unique temp file**

In `nudge-rs/src/queue.rs`'s `save`, replace the fixed temp path:

```rust
        // Process-unique: two daemons sharing one temp name can truncate each
        // other mid-write and publish a corrupt queue.
        let tmp = self.path.with_extension(format!("json.{}.tmp", std::process::id()));
```

- [ ] **Step 7: Run to verify GREEN**

Run: `cd nudge-rs && cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: the 3 singleton tests + the queue temp test pass; whole suite green.

- [ ] **Step 8: Commit**

```bash
git add nudge-rs/Cargo.toml nudge-rs/Cargo.lock nudge-rs/src/daemon.rs nudge-rs/src/ipc/server.rs nudge-rs/src/queue.rs nudge-rs/tests/daemon_singleton.rs
git commit -m "fix(nudge-rs): enforce a single daemon and stop stealing a live socket"
```

---

### Task 2: `--install-daemon` must not run over an ad-hoc daemon (C2, second path)

**Files:**
- Modify: `nudge-rs/src/register/mod.rs`

**Interfaces:**
- Produces: `register::install` warns and refuses (or stops the ad-hoc daemon) when a daemon is already listening, instead of enabling a unit that starts a second one.

- [ ] **Step 1: Implement**

In `register::install`, before writing files/running commands, detect a live daemon on the IPC socket and refuse with actionable advice:

```rust
    // An ad-hoc daemon (auto-started by `nudge -p ...`) still owns the socket.
    // Enabling the unit here would start a SECOND daemon: two schedulers on one
    // queue double-fire jobs and erase each other's state.
    let paths = crate::paths::resolve();
    if std::os::unix::net::UnixStream::connect(&paths.socket).is_ok() {
        anyhow::bail!(
            "a nudge daemon is already running (socket {}).\n\
             Stop it first, then re-run --install-daemon:\n  pkill -f 'nudge --daemon'",
            paths.socket.display()
        );
    }
```

Keep everything else unchanged.

- [ ] **Step 2: Verify + commit**

Run: `cd nudge-rs && cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: green. (`install` stays untested by design — it touches the host service manager; this guard is inspection-verified. Do NOT call `install` from a test.)

```bash
git add nudge-rs/src/register/mod.rs
git commit -m "fix(nudge-rs): refuse --install-daemon while a daemon is already running"
```

---

### Task 3: `batch_img2pdf --clean` must not delete unconverted images (C3)

**Files:**
- Modify: `scripts/batch_img2pdf`
- Modify: `tests/test_batch_img2pdf.sh`

**Interfaces:**
- Produces: `folder_fully_covered <dir> <image-count>` — returns 0 only when `<dir>` contains no subdirectories AND its total file count equals `<image-count>` (i.e. everything in it went into the PDF).

- [ ] **Step 1: Write the failing test**

Add to `tests/test_batch_img2pdf.sh` (it already stubs `unar`/`file`/`img2pdf` on PATH — follow that existing pattern):

```bash
# --- C3: --clean must not delete folders holding unconverted nested images ----
# Layout: book/cover.jpg + book/chapter1/p1.jpg. Only cover.jpg goes into the
# PDF (the image scan is -maxdepth 1), so rm -rf book/ would destroy chapter1/.
c3=$(mktemp -d); mkdir -p "$c3/main/book/chapter1"
: >"$c3/main/book/cover.jpg"; : >"$c3/main/book/chapter1/p1.jpg"
( cd "$c3" && PATH="$STUB_DIR:$PATH" bash "$HERE/../scripts/batch_img2pdf" --clean -o "$c3/out" main ) >/dev/null 2>&1
check "C3: nested unconverted image survives --clean" "yes" \
    "$([ -f "$c3/main/book/chapter1/p1.jpg" ] && echo yes || echo no)"
rm -rf "$c3"
```

(Adapt `$STUB_DIR`/`$HERE` to whatever the file already uses; the assertion — the nested image survives — is the contract.)

- [ ] **Step 2: Run to verify it fails (RED)**

Run: `cd /home/davey/projects/daveyutils && bash tests/test_batch_img2pdf.sh`
Expected: FAIL — `p1.jpg` is gone, because `rm -rf book/` deleted the whole tree. Capture this.

- [ ] **Step 3: Implement the guard**

Add a helper next to the other pure helpers in `scripts/batch_img2pdf`:

```bash
# folder_fully_covered <dir> <n-images>
# True only when <dir> has no subdirectories and holds exactly <n-images> files,
# i.e. everything in it went into the PDF. --clean deletes the whole tree, so a
# folder with nested content the PDF never covered must NOT be removed.
folder_fully_covered() {
    local dir="$1" n="$2"
    [[ -z "$(find "$dir" -mindepth 1 -type d -print -quit)" ]] || return 1
    local total
    total="$(find "$dir" -type f | wc -l)"
    [[ "$total" -eq "$n" ]]
}
```

and gate the delete (replacing `[[ "$CLEAN" -eq 1 ]] && rm -rf "$d"`):

```bash
            if [[ "$CLEAN" -eq 1 ]]; then
                if folder_fully_covered "$d" "${#images[@]}"; then
                    rm -rf "$d"
                else
                    printf 'WARN: kept %s: it holds files the PDF does not cover\n' "$d" >&2
                fi
            fi
```

- [ ] **Step 4: Verify GREEN + suite**

Run: `bash -n scripts/batch_img2pdf && bash tests/test_batch_img2pdf.sh && bash tests/run.sh`
Expected: the C3 check passes (nested image survives); existing checks still pass; suite green.

- [ ] **Step 5: Commit**

```bash
git add scripts/batch_img2pdf tests/test_batch_img2pdf.sh
git commit -m "fix(scripts): batch_img2pdf --clean keeps folders the PDF does not cover"
```

---

### Task 4: test purge must only remove jobs it created

**Files:**
- Modify: `tests/test_jobs_e2e.sh`

Note on severity: the review called this "destroys the user's own scheduled jobs". Verified narrower — the bash nudge defaults to at queue `n` (`AT_QUEUE="${NUDGE_AT_QUEUE:-n}"`), and this test uses throwaway queues `w v u`, so real nudge jobs are NOT at risk. It only affects someone who uses `at -q w|v|u` themselves. Still wrong: a test must not blanket-delete a queue it doesn't own.

- [ ] **Step 1: Implement**

In `tests/test_jobs_e2e.sh`, track the ids the test creates and remove only those. Replace the blanket `purge()`:

```bash
# Ids this test created; purge removes ONLY these. A blanket `atrm $(atq -q w)`
# would delete jobs in queues w/v/u that the test did not create.
CREATED_IDS=""
remember_id() { [ -n "$1" ] && CREATED_IDS="$CREATED_IDS $1"; }
purge() {
    local id
    for id in $CREATED_IDS; do atrm "$id" 2>/dev/null; done
    CREATED_IDS=""
}
```

Then call `remember_id "$ID"` after each successful `schedule`/staging step that produces an id (including the "foreign" jobs staged in queues v and u). Remove the bare `purge` call at the top (there is nothing of ours to purge before we've created anything) — keep it in the EXIT trap.

- [ ] **Step 2: Verify**

Run: `cd /home/davey/projects/daveyutils && bash tests/run.sh`
Expected: suite green (the e2e file still passes or self-skips as before).

Prove the fix: create a job of your own in queue w (`echo true | at -q w now + 1 hour`), note its id via `atq -q w`, run `bash tests/test_jobs_e2e.sh`, and confirm YOUR job still exists afterwards. Then remove it (`atrm <id>`). Report what you observed.

- [ ] **Step 3: Commit**

```bash
git add tests/test_jobs_e2e.sh
git commit -m "test(scripts): purge only the at jobs the e2e test created"
```

---

## Self-Review

**Spec coverage:** C1+C2 (singleton: lock + probe + unique temp + install guard) → Tasks 1-2 ✅. C3 (`--clean` coverage guard) → Task 3 ✅. Test purge scoping → Task 4 ✅.

**Placeholder scan:** No TBDs. The `fs4` API and the test file's stub-helper names are explicitly "adapt to what resolves; the assertion is the contract" — the behavior is fully pinned by the tests. ✅

**Type consistency:** `acquire_singleton_lock(&Path) -> io::Result<File>` used identically in daemon.rs and the test. `serve(&Path, Arc<Mutex<Queue>>) -> io::Result<()>` unchanged. `folder_fully_covered <dir> <n>` matches its call site. ✅

## Notes

- The lock is the authoritative fix (crash-safe: the OS drops it when the process dies). The probe closes the deterministic install-over-adhoc path and gives a good error. The unique temp file removes the corruption vector. They are complementary — keep all three.
- Increment 2 (Rust correctness, 10 findings) and Increment 3 (bash, 7) follow; see the findings doc.

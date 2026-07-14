# nudge Rust rewrite — daemon foundation (Phase 1, increment 3a)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the hermetic foundation of the nudge daemon — cross-platform paths, the `TargetSpec`→live-`Target` bridge, the IPC protocol, and a Unix-socket server/client that manages the persistent job queue — with zero OS-service registration and no persistent process.

**Architecture:** The daemon (built in 3b) owns a `Queue` and serves requests over a Unix domain socket. This increment implements everything *except* the timer loop and OS registration: request/response types, JSON line framing, a request handler that mutates the queue, and a socket server/client. All tests are hermetic — the socket lives in a tempdir and the server handles one connection per spawned thread, so nothing registers with systemd/launchd or lingers.

**Tech Stack:** Rust 2021, reusing `serde`/`serde_json`, `anyhow`, `jiff`, and the `job`/`queue`/`target` modules from increments 1–2. Unix-only IPC via `std::os::unix::net`.

## Context

Increment 3a, stacked on `feat/nudge-rust-tmux` (increment 2, PR #6). Consumes `job::{Job, JobSpec}`, `queue::Queue`, `target::{Target, tmux::TmuxTarget}`. The design spec's own "open questions" set the defaults used here (XDG paths; socket in `$XDG_RUNTIME_DIR`).

## Global Constraints

- Crate at `nudge-rs/`, edition 2021. No new crates (YAGNI) — everything needed is already a dependency.
- **Hermetic tests only.** No test may register a systemd/launchd unit, spawn a persistent daemon, or bind a socket outside a `tempfile::tempdir()`. The socket server is tested by spawning a thread that handles exactly one connection per client request, then joins.
- IPC is Unix-only (`std::os::unix::net`); both CI targets (Linux, macOS) are Unix.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass at every commit. Commit prefixes `feat/refactor/test(nudge-rs): …`; NO attribution lines.
- The `job::Target` → `job::TargetSpec` rename (Task 2) is the resolution of the collision deferred from increment 2; after it, no `as TargetKind` alias is needed.

## File Structure

- `nudge-rs/src/paths.rs` — resolve state dir, queue path, socket path (cross-platform, env-overridable).
- `nudge-rs/src/job.rs` — rename `Target` → `TargetSpec`; add `serde` derives to `JobSpec`; add `connect()` bridge.
- `nudge-rs/src/ipc/mod.rs` — `Request`/`Response`, JSON line framing, `handle_request`.
- `nudge-rs/src/ipc/server.rs` — `serve_once` / `serve`.
- `nudge-rs/src/ipc/client.rs` — `request`.
- `nudge-rs/src/lib.rs` — add `pub mod paths;` and `pub mod ipc;`.
- `nudge-rs/tests/ipc_socket.rs` — hermetic socket round-trip integration test.

---

### Task 1: `paths` — cross-platform state/queue/socket locations

**Files:**
- Create: `nudge-rs/src/paths.rs`
- Modify: `nudge-rs/src/lib.rs` (add `pub mod paths;`)

**Interfaces:**
- Produces:
  - `paths::Paths { pub state_dir: PathBuf, pub queue: PathBuf, pub socket: PathBuf }` (derives `Debug, Clone, PartialEq, Eq`).
  - `paths::Os` — `enum { Linux, Macos }` (derives `Debug, Clone, Copy, PartialEq, Eq`).
  - `paths::resolve_from(home: &Path, xdg_state: Option<&Path>, xdg_runtime: Option<&Path>, os: Os) -> Paths` — pure.
  - `paths::resolve() -> Paths` — reads `$HOME`, `$XDG_STATE_HOME`, `$XDG_RUNTIME_DIR`, and the current OS.

- [ ] **Step 1: Write the failing tests**

Append to `nudge-rs/src/paths.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn linux_prefers_xdg_state_and_runtime() {
        let p = resolve_from(
            Path::new("/home/d"),
            Some(Path::new("/home/d/.local/state")),
            Some(Path::new("/run/user/1000")),
            Os::Linux,
        );
        assert_eq!(p.state_dir, Path::new("/home/d/.local/state/nudge"));
        assert_eq!(p.queue, Path::new("/home/d/.local/state/nudge/queue.json"));
        assert_eq!(p.socket, Path::new("/run/user/1000/nudge.sock"));
    }

    #[test]
    fn linux_falls_back_to_home_when_xdg_unset() {
        let p = resolve_from(Path::new("/home/d"), None, None, Os::Linux);
        assert_eq!(p.state_dir, Path::new("/home/d/.local/state/nudge"));
        // No XDG_RUNTIME_DIR -> socket sits in the state dir.
        assert_eq!(p.socket, Path::new("/home/d/.local/state/nudge/nudge.sock"));
    }

    #[test]
    fn macos_uses_application_support() {
        let p = resolve_from(Path::new("/Users/d"), None, None, Os::Macos);
        assert_eq!(
            p.state_dir,
            Path::new("/Users/d/Library/Application Support/nudge")
        );
        assert_eq!(
            p.socket,
            Path::new("/Users/d/Library/Application Support/nudge/nudge.sock")
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd nudge-rs && cargo test paths`
Expected: FAIL — `resolve_from`/`Paths`/`Os` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `nudge-rs/src/paths.rs`:

```rust
//! Cross-platform locations for nudge's state file and IPC socket.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub queue: PathBuf,
    pub socket: PathBuf,
}

/// Resolve paths from explicit inputs (pure; used by tests and `resolve`).
pub fn resolve_from(
    home: &Path,
    xdg_state: Option<&Path>,
    xdg_runtime: Option<&Path>,
    os: Os,
) -> Paths {
    let state_dir = match os {
        Os::Linux => xdg_state
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("nudge"),
        Os::Macos => home.join("Library/Application Support/nudge"),
    };

    // The socket belongs in a runtime dir when one exists (Linux); otherwise it
    // lives beside the state file.
    let socket_dir = match os {
        Os::Linux => xdg_runtime.map(Path::to_path_buf).unwrap_or_else(|| state_dir.clone()),
        Os::Macos => state_dir.clone(),
    };

    Paths {
        queue: state_dir.join("queue.json"),
        socket: socket_dir.join("nudge.sock"),
        state_dir,
    }
}

/// Resolve paths from the current environment and OS.
pub fn resolve() -> Paths {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let xdg_state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let os = if cfg!(target_os = "macos") { Os::Macos } else { Os::Linux };
    resolve_from(&home, xdg_state.as_deref(), xdg_runtime.as_deref(), os)
}
```

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod paths;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd nudge-rs && cargo test paths && cargo clippy --all-targets -- -D warnings`
Expected: 3 `paths::tests::*` PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add nudge-rs/src/paths.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): cross-platform paths for state and socket"
```

---

### Task 2: rename `job::Target` → `TargetSpec` + `connect()` bridge

**Files:**
- Modify: `nudge-rs/src/job.rs` (rename enum, add `connect`)
- Modify: `nudge-rs/src/queue.rs` (test uses `Target::Tmux`)
- Modify: `nudge-rs/src/inject.rs` (test import)
- Modify: `nudge-rs/tests/tmux_e2e.rs` (test import)

**Interfaces:**
- Consumes: `target::{Target, tmux::TmuxTarget}`.
- Produces:
  - `job::TargetSpec` (was `job::Target`) — `#[serde(tag = "kind")] enum { Tmux { pane: String } }`.
  - `Job.target: TargetSpec` (field type renamed).
  - `impl TargetSpec { pub fn connect(&self) -> Box<dyn crate::target::Target> }` — `Tmux { pane }` → `TmuxTarget::new(pane)`.

- [ ] **Step 1: Rename the enum and add the bridge**

In `nudge-rs/src/job.rs`: rename the enum `Target` to `TargetSpec` (its definition and the `Job.target` field type and the `JobSpec.target` field type). Then add, below the enum:

```rust
impl TargetSpec {
    /// Build the live, connectable target this descriptor names.
    pub fn connect(&self) -> Box<dyn crate::target::Target> {
        match self {
            TargetSpec::Tmux { pane } => {
                Box::new(crate::target::tmux::TmuxTarget::new(pane.clone()))
            }
        }
    }
}
```

Update the existing `job.rs` roundtrip test: it constructs `Target::Tmux { pane: ... }` — change to `TargetSpec::Tmux { pane: ... }`. The assertion `json.contains(r#""kind":"Tmux""#)` stays valid (the serde tag is the variant name `Tmux`, unaffected by the type rename).

- [ ] **Step 2: Update the three consumers**

- `nudge-rs/src/queue.rs` test module: `use crate::job::Target;` → `use crate::job::TargetSpec;`, and `Target::Tmux { pane: ... }` → `TargetSpec::Tmux { pane: ... }`.
- `nudge-rs/src/inject.rs` test module: replace `use crate::job::{Job, Target as TargetKind};` with `use crate::job::{Job, TargetSpec};`, and every `TargetKind::Tmux` → `TargetSpec::Tmux`.
- `nudge-rs/tests/tmux_e2e.rs`: replace `use nudge::job::{Job, Target as TargetKind};` with `use nudge::job::{Job, TargetSpec};`, and every `TargetKind::Tmux` → `TargetSpec::Tmux`.

- [ ] **Step 3: Verify the whole suite still passes**

Run: `cd nudge-rs && cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: all prior tests still pass (29 + 3 paths = 32); no `Target`/`TargetKind` references remain (grep to confirm: `grep -rn "TargetKind\|job::Target\b" src tests` returns nothing). clippy clean.

Note: `connect()` returns a real `TmuxTarget`, so it is exercised by the firing path in increment 3b; here it need only compile and be reachable. Do not add a test that calls `capture()`/`send_line()` on the connected target (that would shell out to tmux).

- [ ] **Step 4: Commit**

```bash
git add nudge-rs/src/job.rs nudge-rs/src/queue.rs nudge-rs/src/inject.rs nudge-rs/tests/tmux_e2e.rs
git commit -m "refactor(nudge-rs): rename job::Target to TargetSpec and add connect() bridge"
```

---

### Task 3: `ipc` protocol — messages, framing, request handler

**Files:**
- Create: `nudge-rs/src/ipc/mod.rs`
- Modify: `nudge-rs/src/job.rs` (add `Serialize, Deserialize` to `JobSpec`)
- Modify: `nudge-rs/src/lib.rs` (add `pub mod ipc;`)

**Interfaces:**
- Consumes: `job::{Job, JobSpec}`, `queue::Queue`.
- Produces:
  - `ipc::Request` — `#[serde(tag = "op")] enum { Schedule(JobSpec), List, Cancel(u64), Ping }` (Serialize, Deserialize, Debug, PartialEq).
  - `ipc::Response` — `enum { Scheduled(u64), Jobs(Vec<Job>), Cancelled(bool), Pong, Error(String) }` (Serialize, Deserialize, Debug, PartialEq).
  - `ipc::write_msg<W: std::io::Write, T: serde::Serialize>(w: &mut W, msg: &T) -> std::io::Result<()>` — one JSON object + `\n`, flushed.
  - `ipc::read_msg<R: std::io::BufRead, T: serde::de::DeserializeOwned>(r: &mut R) -> std::io::Result<Option<T>>` — reads one line; `Ok(None)` on EOF.
  - `ipc::handle_request(req: Request, queue: &mut Queue) -> Response`.

- [ ] **Step 1: Add serde to `JobSpec`**

In `nudge-rs/src/job.rs`, change the `JobSpec` derive line from `#[derive(Clone, Debug, PartialEq)]` to:

```rust
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
```

(`serde::{Serialize, Deserialize}` are already imported in job.rs.)

- [ ] **Step 2: Write the failing tests**

`nudge-rs/src/ipc/mod.rs`:

```rust
//! IPC protocol between the nudge CLI and the daemon: newline-delimited JSON
//! `Request`/`Response` messages over a Unix socket.

pub mod client;
pub mod server;

use std::io::{BufRead, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::job::{Job, JobSpec};
use crate::queue::Queue;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "op")]
pub enum Request {
    Schedule(JobSpec),
    List,
    Cancel(u64),
    Ping,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Response {
    Scheduled(u64),
    Jobs(Vec<Job>),
    Cancelled(bool),
    Pong,
    Error(String),
}

/// Write one message as a single JSON line and flush.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

/// Read one newline-delimited JSON message. `Ok(None)` on clean EOF.
pub fn read_msg<R: BufRead, T: DeserializeOwned>(r: &mut R) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let msg = serde_json::from_str(line.trim_end())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

/// Apply a request to the queue and produce the response.
pub fn handle_request(req: Request, queue: &mut Queue) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Schedule(spec) => match queue.add(spec) {
            Ok(id) => Response::Scheduled(id),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::List => Response::Jobs(queue.all().to_vec()),
        Request::Cancel(id) => match queue.remove(id) {
            Ok(removed) => Response::Cancelled(removed),
            Err(e) => Response::Error(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::TargetSpec;
    use std::io::BufReader;

    fn spec() -> JobSpec {
        JobSpec {
            target: TargetSpec::Tmux { pane: "bot:0.1".into() },
            messages: vec!["go".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 2,
            settle_secs: 5.0,
        }
    }

    #[test]
    fn framing_roundtrips_one_message_per_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_msg(&mut buf, &Request::Ping).unwrap();
        write_msg(&mut buf, &Request::Cancel(7)).unwrap();
        // Two distinct lines.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 2);

        let mut r = BufReader::new(&buf[..]);
        let a: Request = read_msg(&mut r).unwrap().unwrap();
        let b: Request = read_msg(&mut r).unwrap().unwrap();
        assert_eq!(a, Request::Ping);
        assert_eq!(b, Request::Cancel(7));
        // EOF -> None.
        assert!(read_msg::<_, Request>(&mut r).unwrap().is_none());
    }

    #[test]
    fn handle_schedule_list_cancel_against_a_real_queue() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();

        let resp = handle_request(Request::Schedule(spec()), &mut q);
        assert_eq!(resp, Response::Scheduled(1));

        match handle_request(Request::List, &mut q) {
            Response::Jobs(jobs) => assert_eq!(jobs.len(), 1),
            other => panic!("expected Jobs, got {other:?}"),
        }

        assert_eq!(handle_request(Request::Cancel(1), &mut q), Response::Cancelled(true));
        assert_eq!(handle_request(Request::Cancel(1), &mut q), Response::Cancelled(false));
    }

    #[test]
    fn ping_pongs() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::load(dir.path().join("q.json")).unwrap();
        assert_eq!(handle_request(Request::Ping, &mut q), Response::Pong);
    }
}
```

You will create `client.rs` and `server.rs` in Task 4; for THIS task, temporarily stub them so `pub mod client; pub mod server;` compile — create `nudge-rs/src/ipc/client.rs` and `nudge-rs/src/ipc/server.rs` each containing only a doc comment line (`//! (implemented in Task 4)`). Task 4 fills them in.

Add to `nudge-rs/src/lib.rs`:

```rust
pub mod ipc;
```

- [ ] **Step 3: Run tests to verify they fail, then pass**

Run: `cd nudge-rs && cargo test ipc`
Expected: compile-fail first (types missing / JobSpec not serde), then after Steps 1–2 the 3 `ipc::tests::*` PASS.

- [ ] **Step 4: Lint and commit**

Run: `cd nudge-rs && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: full suite green (35 unit + 3 tmux integration); clippy clean.

```bash
git add nudge-rs/src/ipc/mod.rs nudge-rs/src/ipc/client.rs nudge-rs/src/ipc/server.rs nudge-rs/src/job.rs nudge-rs/src/lib.rs
git commit -m "feat(nudge-rs): IPC Request/Response protocol, framing, and queue handler"
```

---

### Task 4: Unix-socket server + client (hermetic round-trip)

**Files:**
- Modify: `nudge-rs/src/ipc/server.rs` (replace the stub)
- Modify: `nudge-rs/src/ipc/client.rs` (replace the stub)
- Create: `nudge-rs/tests/ipc_socket.rs`

**Interfaces:**
- Consumes: `ipc::{Request, Response, read_msg, write_msg, handle_request}`, `queue::Queue`.
- Produces:
  - `ipc::server::serve_once(listener: &std::os::unix::net::UnixListener, queue: &std::sync::Mutex<Queue>) -> std::io::Result<()>` — accept one connection, handle one request, reply.
  - `ipc::server::serve(socket: &std::path::Path, queue: std::sync::Arc<std::sync::Mutex<Queue>>) -> std::io::Result<()>` — bind and loop `serve_once` forever (used by the daemon in 3b).
  - `ipc::client::request(socket: &std::path::Path, req: &Request) -> std::io::Result<Response>` — connect, send one request, read one response.

- [ ] **Step 1: Implement the server**

Replace `nudge-rs/src/ipc/server.rs` with:

```rust
//! Unix-socket server: applies IPC requests to the shared queue.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{handle_request, read_msg, write_msg, Request, Response};
use crate::queue::Queue;

/// Handle exactly one incoming connection (one request → one response).
pub fn serve_once(listener: &UnixListener, queue: &Mutex<Queue>) -> std::io::Result<()> {
    let (stream, _) = listener.accept()?;
    handle_conn(stream, queue)
}

/// Bind `socket` and serve connections forever. Removes a stale socket file
/// first. Used by the daemon (increment 3b).
pub fn serve(socket: &Path, queue: Arc<Mutex<Queue>>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket); // clear a stale socket from a prior run
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(socket)?;
    loop {
        serve_once(&listener, &queue)?;
    }
}

fn handle_conn(stream: UnixStream, queue: &Mutex<Queue>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let req: Request = match read_msg(&mut reader)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let resp: Response = {
        let mut q = queue.lock().expect("queue mutex poisoned");
        handle_request(req, &mut q)
    };
    write_msg(&mut &stream, &resp)
}
```

- [ ] **Step 2: Implement the client**

Replace `nudge-rs/src/ipc/client.rs` with:

```rust
//! Unix-socket client: send one request, read one response.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::{read_msg, write_msg, Request, Response};

/// Connect to the daemon socket, send `req`, and return its `Response`.
pub fn request(socket: &Path, req: &Request) -> std::io::Result<Response> {
    let stream = UnixStream::connect(socket)?;
    write_msg(&mut &stream, req)?;
    let mut reader = BufReader::new(&stream);
    read_msg(&mut reader)?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no response"))
}
```

- [ ] **Step 3: Write the hermetic integration test**

`nudge-rs/tests/ipc_socket.rs`:

```rust
//! Hermetic round-trip over a real Unix socket in a tempdir. The server thread
//! handles exactly one connection per client request, then joins — nothing
//! lingers, and no OS service is involved.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use nudge::ipc::{client, server, Request, Response};
use nudge::job::{JobSpec, TargetSpec};
use nudge::queue::Queue;

fn spec() -> JobSpec {
    JobSpec {
        target: TargetSpec::Tmux { pane: "bot:0.1".into() },
        messages: vec!["go".into()],
        send_delay_secs: 0.75,
        fire_at: "2026-07-13T15:00:00Z".parse().unwrap(),
        notify: false,
        verify: false,
        auto_retry: false,
        retries_left: 2,
        settle_secs: 5.0,
    }
}

/// Serve exactly one request on a background thread, then run the client.
fn one_shot(listener: &Arc<UnixListener>, queue: &Arc<Mutex<Queue>>, req: Request) -> Response {
    let l = Arc::clone(listener);
    let q = Arc::clone(queue);
    let handle = thread::spawn(move || server::serve_once(&l, &q).unwrap());
    let socket = l_addr(&listener);
    let resp = client::request(&socket, &req).unwrap();
    handle.join().unwrap();
    resp
}

fn l_addr(listener: &UnixListener) -> std::path::PathBuf {
    listener
        .local_addr()
        .unwrap()
        .as_pathname()
        .unwrap()
        .to_path_buf()
}

#[test]
fn socket_roundtrip_schedule_list_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nudge.sock");
    let queue_path = dir.path().join("queue.json");

    let listener = Arc::new(UnixListener::bind(&socket).unwrap());
    let queue = Arc::new(Mutex::new(Queue::load(queue_path).unwrap()));

    assert_eq!(one_shot(&listener, &queue, Request::Ping), Response::Pong);

    match one_shot(&listener, &queue, Request::Schedule(spec())) {
        Response::Scheduled(id) => assert_eq!(id, 1),
        other => panic!("expected Scheduled, got {other:?}"),
    }

    match one_shot(&listener, &queue, Request::List) {
        Response::Jobs(jobs) => assert_eq!(jobs.len(), 1),
        other => panic!("expected Jobs, got {other:?}"),
    }

    assert_eq!(
        one_shot(&listener, &queue, Request::Cancel(1)),
        Response::Cancelled(true)
    );
    assert_eq!(
        one_shot(&listener, &queue, Request::Cancel(1)),
        Response::Cancelled(false)
    );
}
```

Note: `one_shot` takes `&Arc<UnixListener>` but calls `l_addr(&listener)` — fix by passing the socket path explicitly if the signature is awkward; the behavior (one serve_once per client request, joined) is the contract. Adjust the helper's plumbing as needed so it compiles, keeping the assertions unchanged.

- [ ] **Step 4: Run the integration test**

Run: `cd nudge-rs && cargo test --test ipc_socket -- --nocapture`
Expected: `socket_roundtrip_schedule_list_cancel` PASSES; no hang (each `serve_once` returns after one connection and is joined).

- [ ] **Step 5: Lint, full suite, commit**

Run: `cd nudge-rs && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: full suite green (35 unit + 3 tmux + 1 ipc_socket); clippy clean.

```bash
git add nudge-rs/src/ipc/server.rs nudge-rs/src/ipc/client.rs nudge-rs/tests/ipc_socket.rs
git commit -m "feat(nudge-rs): Unix-socket IPC server and client with hermetic round-trip test"
```

---

## Self-Review

**Spec coverage (3a slice):**
- Cross-platform state/queue/socket paths (XDG defaults) → Task 1. ✅
- `TargetSpec` rename + spec→live-target bridge (resolves increment-2 deferral) → Task 2. ✅
- Unix-socket IPC protocol + queue-backed handlers (schedule/list/cancel) → Tasks 3–4. ✅
- Hermetic tests, no OS registration, no lingering daemon → all tasks. ✅
- Out of 3a (→ 3b): the scheduler timer loop + firing (inject), catch-up on startup, systemd/launchd registration (gated real enable), CLI auto-start. Intentionally deferred.

**Placeholder scan:** No TBD/TODO; every code step complete; tests concrete. The Task-4 `one_shot` helper carries an explicit "adjust plumbing to compile, keep assertions" note — the behavior is fully specified. ✅

**Type consistency:** `Request`/`Response` variants match across `ipc/mod.rs`, `server.rs`, `client.rs`, and `ipc_socket.rs`. `handle_request(Request, &mut Queue) -> Response` consistent. `TargetSpec` used post-rename everywhere (Task 2 removes all `job::Target`/`TargetKind`). `JobSpec` gains serde in Task 3, required by `Request::Schedule`. `Queue::{load, add, remove, all}` signatures match increment 1. ✅

## Notes for the next increment (3b)

- Daemon binary subcommand (`nudged` / `nudge --daemon`) that: loads the queue at `paths::resolve().queue`, wraps it in `Arc<Mutex<Queue>>`, spawns `ipc::server::serve` on one thread and the scheduler loop on another.
- Scheduler: compute next due job, sleep until then (or wake on a new schedule via a condvar/channel from the IPC side), fire via `job.target.connect()` + `inject::run_injection`; on startup run catch-up (fire jobs whose `fire_at` passed within `NUDGE_CATCHUP_GRACE`, default 6h; `--verify` guards stale ones).
- Registration: generate the systemd `--user` unit / launchd plist (pure string/plist generation, unit-tested), with the *actual* `systemctl --user enable` / `launchctl bootstrap` gated behind an explicit opt-in flag so tests never mutate the host.

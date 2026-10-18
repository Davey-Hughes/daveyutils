//! Opening a moment in mpv. One window serves a whole review: the first `o` starts mpv with an IPC
//! socket, and every later one loads the next file into that window. The playback keys go to the
//! same socket.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde_json::{json, Value};

use crate::scan::file_stamp;
use crate::text::timestamp;

/// How long a new mpv gets to open its socket.
const START_TIMEOUT: Duration = Duration::from_secs(3);

/// The app id of mpv's window on Wayland, so that a window rule can match it and no other mpv
/// (README, "Window focus").
pub const APP_ID: &str = "subcheck-mpv";

/// A moment to open: a file, where to start, and which subtitle track to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moment {
    pub file: String,
    pub start_ms: i64,
    /// mpv's `--sid`.
    pub sid: Option<usize>,
}

/// What a playback key asks of mpv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// Pause, or resume.
    Pause,
    /// Seek this many seconds; back when negative.
    Seek(i32),
}

/// Where playback is, as mpv reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayState {
    pub paused: bool,
    pub at_ms: i64,
}

/// The socket, from explicit inputs: in the runtime directory when `XDG_RUNTIME_DIR` is absolute,
/// else in the state directory (macOS has no runtime directory).
pub fn socket_path_from(runtime: Option<&Path>, state: &Path) -> PathBuf {
    match runtime.filter(|p| p.is_absolute()) {
        Some(dir) => dir.join("subcheck-mpv.sock"),
        None => state.join("mpv.sock"),
    }
}

/// The socket for this process.
pub fn socket_path(state: &Path) -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    socket_path_from(runtime.as_deref(), state)
}

/// The IPC command that loads `m` in place of whatever is playing. Named arguments, because mpv
/// 0.38 put an `index` argument before `options`; `options` as an object, so no value needs
/// escaping (both checked against mpv 0.41).
pub fn loadfile_command(m: &Moment) -> Value {
    let mut options = serde_json::Map::new();
    options.insert("start".into(), timestamp(m.start_ms).into());
    if let Some(sid) = m.sid {
        options.insert("sid".into(), sid.to_string().into());
    }
    options.insert("sub-visibility".into(), "yes".into());
    json!({"name": "loadfile", "url": m.file, "flags": "replace", "options": options})
}

pub struct Player {
    program: OsString,
    socket: PathBuf,
    /// The mpv this review started, until it exits.
    child: Option<Child>,
    /// The file this review last loaded into mpv, with its size and mtime then.
    loaded: Option<(String, (u64, Timestamp))>,
    /// How many of [`focus_flags`] this mpv takes: all, until one that exited on them started with
    /// fewer.
    focus: usize,
}

impl Player {
    pub fn new(program: impl Into<OsString>, socket: PathBuf) -> Player {
        Player {
            program: program.into(),
            socket,
            child: None,
            loaded: None,
            focus: focus_flags().len(),
        }
    }

    /// Show `m` in the mpv listening on the socket, or start one and show it there. Playing: `o`
    /// is there to show the line.
    pub fn open(&mut self, m: &Moment) -> Result<(), String> {
        self.reap();
        let mut conn = match Conn::connect(&self.socket) {
            Ok(conn) => conn,
            Err(_) => self.start()?,
        };
        // The file mpv has open already is sought, not reopened (6 GB over NFS), if this review
        // loaded it and it is unchanged on disk since: a remux in place keeps the path, and after
        // a fix of one letter even the size. One still loading cannot seek yet, so it is loaded
        // again.
        let stamp = file_stamp(Path::new(&m.file)).ok();
        let unchanged = matches!(
            (&self.loaded, stamp),
            (Some((file, then)), Some(now)) if *file == m.file && *then == now
        );
        let open = conn.request(&json!(["get_property", "path"])).ok();
        let sought = open.as_ref().and_then(Value::as_str) == Some(m.file.as_str())
            && unchanged
            && conn
                .request(&json!(["seek", timestamp(m.start_ms), "absolute+exact"]))
                .is_ok();
        if sought {
            if let Some(sid) = m.sid {
                conn.request(&json!(["set_property", "sid", sid]))?;
            }
            conn.request(&json!(["set_property", "sub-visibility", true]))?;
        } else {
            conn.request(&loadfile_command(m))?;
            self.loaded = stamp.map(|s| (m.file.clone(), s));
        }
        conn.request(&json!(["set_property", "pause", false]))?;
        Ok(())
    }

    /// Pause or seek the mpv on the socket, and say where it is now. Never starts one.
    pub fn playback(&mut self, p: Playback) -> Result<PlayState, String> {
        self.reap();
        let mut conn =
            Conn::connect(&self.socket).map_err(|_| "mpv is not open (o opens it)".to_string())?;
        // No position: no file open, or one still opening (seconds, from a disk that has to spin
        // up), which has its path already and refuses seeks.
        match conn.request(&json!(["get_property", "time-pos"])) {
            Err(e) if e == "mpv: property unavailable" => {
                return Err(match conn.request(&json!(["get_property", "path"])) {
                    Ok(_) => "mpv is still opening the file".into(),
                    Err(e) if e == "mpv: property unavailable" => {
                        "mpv has no file open (o opens one)".into()
                    }
                    Err(e) => e,
                });
            }
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        conn.request(&match p {
            Playback::Pause => json!(["cycle", "pause"]),
            // Exact: a plain relative seek lands on a keyframe, up to seconds away.
            Playback::Seek(s) => json!(["seek", s, "relative+exact"]),
        })?;
        let paused = conn.request(&json!(["get_property", "pause"]))?;
        // Read straight after an exact seek, this is already the new position (checked against
        // mpv 0.41).
        let secs = conn.request(&json!(["get_property", "time-pos"]))?;
        #[allow(clippy::cast_possible_truncation)]
        let at_ms = (secs.as_f64().unwrap_or(0.0) * 1000.0).round() as i64;
        Ok(PlayState {
            paused: paused.as_bool().unwrap_or(false),
            at_ms,
        })
    }

    /// Forget a started mpv that has exited (and collect its exit status).
    fn reap(&mut self) {
        if let Some(child) = &mut self.child {
            if !matches!(child.try_wait(), Ok(None)) {
                self.child = None;
            }
        }
    }

    /// Start an idle mpv on the socket and connect to it. A stale socket file left by an mpv that
    /// died is no obstacle: mpv replaces it.
    fn start(&mut self) -> Result<Conn, String> {
        let mut focus = self.focus;
        loop {
            match self.launch(focus) {
                Ok(conn) => {
                    self.focus = focus;
                    return Ok(conn);
                }
                // mpv exits at once on an option it does not know, and an older one lacks
                // --focus-on, older still --wayland-app-id: one fewer, down to none. What worked
                // is kept for the next start.
                Err(Start::Exited(_)) if focus > 0 => focus -= 1,
                Err(Start::Exited(e) | Start::Failed(e)) => return Err(e),
            }
        }
    }

    /// Start mpv with the first `focus` of [`focus_flags`], and connect to it.
    fn launch(&mut self, focus: usize) -> Result<Conn, Start> {
        if let Some(dir) = self.socket.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Start::Failed(format!("creating {}: {e}", dir.display())))?;
        }
        let mut server = OsString::from("--input-ipc-server=");
        server.push(&self.socket);
        let child = Command::new(&self.program)
            .arg(server)
            .args(["--idle=yes", "--force-window=yes", "--no-terminal"])
            .args(&focus_flags()[..focus])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own process group: the terminal's signals stay with the review, and the window
            // outlives it.
            .process_group(0)
            .spawn()
            .map_err(|e| {
                let program = self.program.to_string_lossy();
                Start::Failed(if e.kind() == io::ErrorKind::NotFound {
                    format!("{program} not found: install mpv to open findings")
                } else {
                    format!("starting {program}: {e}")
                })
            })?;
        self.child = Some(child);
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(conn) = Conn::connect(&self.socket) {
                return Ok(conn);
            }
            if let Some(child) = &mut self.child {
                if let Ok(Some(status)) = child.try_wait() {
                    self.child = None;
                    let e = format!("mpv exited before opening its socket ({status})");
                    // 1 is mpv's exit on an option it does not know; a launcher that exits 0 or a
                    // crash is no reason to drop a flag.
                    return Err(if status.code() == Some(1) {
                        Start::Exited(e)
                    } else {
                        Start::Failed(e)
                    });
                }
            }
            if Instant::now() >= deadline {
                return Err(Start::Failed(format!(
                    "mpv did not open {} within {} s",
                    self.socket.display(),
                    START_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The flags that keep focus in the terminal, on macOS by themselves and on KDE with the README's
/// rule: the app id the rule matches, and no focus for a new window. Newest last (mpv 0.38 added
/// `--focus-on`, and had `--wayland-app-id` on every platform already), so an older mpv can be
/// given the first of them only.
fn focus_flags() -> [String; 2] {
    [
        format!("--wayland-app-id={APP_ID}"),
        "--focus-on=never".into(),
    ]
}

/// Why a started mpv gave no socket.
enum Start {
    /// It exited first with status 1, as mpv does on an option it does not know.
    Exited(String),
    Failed(String),
}

/// One connection to mpv: each command is answered before the next is sent.
struct Conn {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Conn {
    fn connect(socket: &Path) -> io::Result<Conn> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Conn {
            stream,
            reader,
            next_id: 1,
        })
    }

    /// Send `command` and wait for mpv's reply to it: its `data`, or its error. Event lines may
    /// come first.
    fn request(&mut self, command: &Value) -> Result<Value, String> {
        let fail = |e: io::Error| format!("talking to mpv: {e}");
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({"command": command, "request_id": id}).to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).map_err(fail)?;
        let mut reply = String::new();
        loop {
            reply.clear();
            if self.reader.read_line(&mut reply).map_err(fail)? == 0 {
                return Err("mpv closed the connection without answering".into());
            }
            let Ok(v) = serde_json::from_str::<Value>(&reply) else {
                continue;
            };
            if v.get("request_id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return match v.get("error").and_then(Value::as_str) {
                Some("success") => Ok(v.get("data").cloned().unwrap_or(Value::Null)),
                Some(error) => Err(format!("mpv: {error}")),
                None => Err(format!("mpv: unexpected reply {reply}")),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn moment() -> Moment {
        Moment {
            file: "/m/K-On! - E02.mkv".into(),
            start_ms: 163_060,
            sid: Some(2),
        }
    }

    fn ok(data: impl Into<Value>) -> Value {
        let data: Value = data.into();
        json!({"data": data, "error": "success"})
    }

    fn error(e: &str) -> Value {
        json!({"error": e})
    }

    /// An mpv stand-in: on one connection to `listener`, answers each command with `answer`'s
    /// reply (after an event line, as mpv sends them unasked), and returns the commands it was
    /// sent once the connection closes.
    fn fake_mpv(
        listener: UnixListener,
        answer: impl Fn(&Value) -> Value + Send + 'static,
    ) -> thread::JoinHandle<Vec<Value>> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut sent = Vec::new();
            for line in BufReader::new(&stream).lines() {
                let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
                let mut reply = answer(&request["command"]);
                reply["request_id"] = request["request_id"].clone();
                let mut w = &stream;
                writeln!(w, "{{\"event\":\"idle\"}}\n{reply}").unwrap();
                sent.push(request["command"].clone());
            }
            sent
        })
    }

    /// An idle mpv: nothing open, every other command succeeds.
    fn idle(command: &Value) -> Value {
        if command[1] == "path" || command[1] == "time-pos" {
            error("property unavailable")
        } else {
            ok(Value::Null)
        }
    }

    /// A script standing in for the mpv program: it writes its arguments to `argv` and stays
    /// up for a while, as mpv would.
    fn fake_program(dir: &Path) -> (PathBuf, PathBuf) {
        let argv = dir.join("argv");
        let program = script(
            dir,
            "fake-mpv",
            &format!(
                "printf '%s\\n' \"$@\" > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nsleep 2\n",
                argv.display()
            ),
        );
        (program, argv)
    }

    fn unpause() -> Value {
        json!(["set_property", "pause", false])
    }

    #[test]
    fn the_socket_lives_in_the_runtime_dir_when_there_is_one() {
        let state = Path::new("/home/u/.local/state/subcheck");
        assert_eq!(
            socket_path_from(Some(Path::new("/run/user/1000")), state),
            Path::new("/run/user/1000/subcheck-mpv.sock")
        );
        assert_eq!(
            socket_path_from(Some(Path::new("relative")), state),
            state.join("mpv.sock")
        );
        assert_eq!(socket_path_from(None, state), state.join("mpv.sock"));
    }

    #[test]
    fn loadfile_names_its_arguments_and_starts_on_the_track() {
        assert_eq!(
            loadfile_command(&moment()),
            json!({
                "name": "loadfile",
                "url": "/m/K-On! - E02.mkv",
                "flags": "replace",
                "options": {"start": "0:02:43.06", "sid": "2", "sub-visibility": "yes"}
            })
        );
        let no_sid = Moment {
            sid: None,
            ..moment()
        };
        assert!(loadfile_command(&no_sid)["options"].get("sid").is_none());
    }

    #[test]
    fn a_running_mpv_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let mpv = fake_mpv(UnixListener::bind(&socket).unwrap(), idle);
        // A program that cannot start: reusing the window must not try.
        let mut player = Player::new(dir.path().join("no-such-mpv"), socket);
        assert_eq!(player.open(&moment()), Ok(()));
        assert_eq!(
            mpv.join().unwrap(),
            [
                json!(["get_property", "path"]),
                loadfile_command(&moment()),
                unpause()
            ]
        );
    }

    /// `moment()` at a file of 1234 bytes in `dir`.
    fn on_disk(dir: &Path) -> Moment {
        let file = dir.join("K-On! - E02.mkv");
        std::fs::write(&file, [0; 1234]).unwrap();
        Moment {
            file: file.display().to_string(),
            ..moment()
        }
    }

    /// An mpv with `file` open; `seek` is its answer to a seek.
    fn has_open(file: &str, seek: Value) -> impl Fn(&Value) -> Value + Send + 'static {
        let file = file.to_string();
        move |c| match (c[0].as_str(), c[1].as_str()) {
            (_, Some("path")) => ok(file.clone()),
            (Some("seek"), _) => seek.clone(),
            _ => ok(Value::Null),
        }
    }

    /// `player` loads `m` into an idle mpv on `listener`, as the first `o` does.
    fn load(player: &mut Player, listener: &UnixListener, m: &Moment) {
        let mpv = fake_mpv(listener.try_clone().unwrap(), idle);
        assert_eq!(player.open(m), Ok(()));
        assert_eq!(mpv.join().unwrap()[1], loadfile_command(m));
    }

    #[test]
    fn the_file_mpv_has_open_is_sought_not_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let m = on_disk(dir.path());
        let mut player = Player::new("mpv", socket);
        load(&mut player, &listener, &m);
        let mpv = fake_mpv(listener, has_open(&m.file, ok(Value::Null)));
        assert_eq!(player.open(&m), Ok(()));
        assert_eq!(
            mpv.join().unwrap(),
            [
                json!(["get_property", "path"]),
                json!(["seek", "0:02:43.06", "absolute+exact"]),
                json!(["set_property", "sid", 2]),
                json!(["set_property", "sub-visibility", true]),
                unpause()
            ]
        );
    }

    #[test]
    fn a_file_changed_on_disk_since_it_was_loaded_is_loaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let m = on_disk(dir.path());
        let mut player = Player::new("mpv", socket);
        load(&mut player, &listener, &m);
        // Remuxed in place after a fix of one letter: the same path and size, a newer mtime.
        std::fs::write(&m.file, [1; 1234]).unwrap();
        let later = std::time::SystemTime::now() + Duration::from_mins(1);
        std::fs::File::options()
            .write(true)
            .open(&m.file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let mpv = fake_mpv(listener, has_open(&m.file, ok(Value::Null)));
        assert_eq!(player.open(&m), Ok(()));
        assert_eq!(mpv.join().unwrap()[1..], [loadfile_command(&m), unpause()]);
    }

    #[test]
    fn a_file_this_review_did_not_load_is_loaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let m = on_disk(dir.path());
        // mpv's window kept from an earlier review: whatever it opened then may be stale.
        let mpv = fake_mpv(
            UnixListener::bind(&socket).unwrap(),
            has_open(&m.file, ok(Value::Null)),
        );
        let mut player = Player::new("mpv", socket);
        assert_eq!(player.open(&m), Ok(()));
        assert_eq!(mpv.join().unwrap()[1..], [loadfile_command(&m), unpause()]);
    }

    #[test]
    fn a_file_that_cannot_seek_yet_is_loaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let m = on_disk(dir.path());
        let mut player = Player::new("mpv", socket);
        load(&mut player, &listener, &m);
        // Still loading: mpv names the file but refuses the seek.
        let mpv = fake_mpv(listener, has_open(&m.file, error("error running command")));
        assert_eq!(player.open(&m), Ok(()));
        let sent = mpv.join().unwrap();
        assert_eq!(sent[1][0], "seek");
        assert_eq!(sent[2..], [loadfile_command(&m), unpause()]);
    }

    /// Stands in for the started mpv: an idle one on `socket`, once the program has written its
    /// `argv`.
    fn mpv_once_started(socket: &Path, argv: &Path) -> thread::JoinHandle<Vec<Value>> {
        let (socket, argv) = (socket.to_path_buf(), argv.to_path_buf());
        thread::spawn(move || {
            while !argv.exists() {
                thread::sleep(Duration::from_millis(10));
            }
            fake_mpv(UnixListener::bind(&socket).unwrap(), idle)
                .join()
                .unwrap()
        })
    }

    /// A stand-in for an older mpv: it counts its starts in `starts`, exits on an option that
    /// matches `unknown` (a `case` pattern) as mpv does on one it does not know, and else writes
    /// its arguments to `argv` and stays up for a while.
    fn older_mpv(dir: &Path, unknown: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (argv, starts) = (dir.join("argv"), dir.join("starts"));
        let program = script(
            dir,
            "older-mpv",
            &format!(
                "echo >> '{starts}'\nfor a; do case \"$a\" in {unknown}) exit 1;; esac; done\n\
                 printf '%s\\n' \"$@\" > '{argv}.tmp'\nmv '{argv}.tmp' '{argv}'\nsleep 2\n",
                starts = starts.display(),
                argv = argv.display()
            ),
        );
        (program, argv, starts)
    }

    /// The arguments every mpv this review starts gets, before the focus flags.
    fn base_args(socket: &Path) -> String {
        format!(
            "--input-ipc-server={}\n--idle=yes\n--force-window=yes\n--no-terminal\n",
            socket.display()
        )
    }

    #[test]
    fn an_mpv_from_before_focus_on_is_started_again_with_the_app_id_alone() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let (program, argv, _) = older_mpv(dir.path(), "--focus-on=*");
        let server = mpv_once_started(&socket, &argv);
        let mut player = Player::new(&program, socket.clone());
        assert_eq!(player.open(&moment()), Ok(()));
        server.join().unwrap();
        // The app id stays, so the KWin rule still matches the window.
        assert_eq!(
            std::fs::read_to_string(&argv).unwrap(),
            format!("{}--wayland-app-id=subcheck-mpv\n", base_args(&socket))
        );
    }

    #[test]
    fn an_mpv_without_either_flag_is_started_with_neither_from_then_on() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let (program, argv, starts) = older_mpv(dir.path(), "--focus-on=*|--wayland-app-id=*");
        let server = mpv_once_started(&socket, &argv);
        let mut player = Player::new(&program, socket.clone());
        assert_eq!(player.open(&moment()), Ok(()));
        server.join().unwrap();
        assert_eq!(std::fs::read_to_string(&argv).unwrap(), base_args(&socket));
        let count = || std::fs::read_to_string(&starts).unwrap().lines().count();
        assert_eq!(count(), 3, "both flags, the app id, neither");
        // That mpv gone, socket and all, the next o starts another straight away without them.
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_file(&argv).unwrap();
        let server = mpv_once_started(&socket, &argv);
        assert_eq!(player.open(&moment()), Ok(()));
        server.join().unwrap();
        assert_eq!(count(), 4);
    }

    /// An executable script `name` in `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        path
    }

    #[test]
    fn only_an_exit_on_an_option_starts_mpv_again() {
        let dir = tempfile::tempdir().unwrap();
        let starts = dir.path().join("starts");
        // A launcher that puts mpv in the background and exits 0: starting it again would only
        // start more of them.
        let program = script(
            dir.path(),
            "launcher",
            &format!("echo >> '{}'\nexit 0\n", starts.display()),
        );
        let mut player = Player::new(&program, dir.path().join("mpv.sock"));
        let err = player.open(&moment()).unwrap_err();
        assert!(err.contains("exited before opening its socket"), "{err}");
        assert_eq!(std::fs::read_to_string(&starts).unwrap().lines().count(), 1);
    }

    #[test]
    fn mpv_installed_after_a_first_o_failed_still_gets_the_focus_flags() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let mut player = Player::new(dir.path().join("fake-mpv"), socket.clone());
        let err = player.open(&moment()).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        let (_, argv) = fake_program(dir.path());
        let server = mpv_once_started(&socket, &argv);
        assert_eq!(player.open(&moment()), Ok(()));
        server.join().unwrap();
        let argv = std::fs::read_to_string(&argv).unwrap();
        assert!(argv.ends_with("--focus-on=never\n"), "{argv}");
    }

    #[test]
    fn with_no_mpv_running_one_is_started_and_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sock/mpv.sock");
        let (program, argv) = fake_program(dir.path());
        let server = mpv_once_started(&socket, &argv);
        let mut player = Player::new(&program, socket.clone());
        assert_eq!(player.open(&moment()), Ok(()));
        let sent = server.join().unwrap();
        assert_eq!(sent[1], loadfile_command(&moment()));
        assert_eq!(
            std::fs::read_to_string(&argv).unwrap(),
            format!(
                "--input-ipc-server={}\n--idle=yes\n--force-window=yes\n--no-terminal\n\
                 --wayland-app-id=subcheck-mpv\n--focus-on=never\n",
                socket.display()
            )
        );
    }

    #[test]
    fn a_missing_program_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut player = Player::new(dir.path().join("no-such-mpv"), dir.path().join("mpv.sock"));
        let err = player.open(&moment()).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn an_mpv_that_exits_without_a_socket_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "false-mpv", "exit 3\n");
        let mut player = Player::new(&program, dir.path().join("mpv.sock"));
        let err = player.open(&moment()).unwrap_err();
        assert!(err.contains("exited before opening its socket"), "{err}");
    }

    #[test]
    fn an_mpv_error_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let mpv = fake_mpv(UnixListener::bind(&socket).unwrap(), |c| {
            if c["name"] == "loadfile" {
                error("invalid parameter")
            } else {
                idle(c)
            }
        });
        let mut player = Player::new("mpv", socket);
        assert_eq!(player.open(&moment()), Err("mpv: invalid parameter".into()));
        mpv.join().unwrap();
    }

    #[test]
    fn playback_keys_send_their_command_and_read_back_where_mpv_is() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let playing = |c: &Value| match c[1].as_str() {
            Some("path") => ok(moment().file),
            Some("pause") if c[0] == "get_property" => ok(true),
            Some("time-pos") => ok(171.06),
            _ => ok(Value::Null),
        };
        let mut player = Player::new("mpv", socket.clone());
        let listener = UnixListener::bind(&socket).unwrap();
        let mpv = fake_mpv(listener.try_clone().unwrap(), playing);
        assert_eq!(
            player.playback(Playback::Pause),
            Ok(PlayState {
                paused: true,
                at_ms: 171_060
            })
        );
        assert_eq!(
            mpv.join().unwrap(),
            [
                json!(["get_property", "time-pos"]),
                json!(["cycle", "pause"]),
                json!(["get_property", "pause"]),
                json!(["get_property", "time-pos"])
            ]
        );
        let mpv = fake_mpv(listener, playing);
        player.playback(Playback::Seek(-5)).unwrap();
        assert_eq!(
            mpv.join().unwrap()[1],
            json!(["seek", -5, "relative+exact"])
        );
    }

    #[test]
    fn playback_never_starts_mpv_and_needs_a_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let (program, argv) = fake_program(dir.path());
        let mut player = Player::new(&program, socket.clone());
        assert_eq!(
            player.playback(Playback::Pause),
            Err("mpv is not open (o opens it)".into())
        );
        assert!(!argv.exists(), "no mpv started");
        let mpv = fake_mpv(UnixListener::bind(&socket).unwrap(), idle);
        assert_eq!(
            player.playback(Playback::Seek(5)),
            Err("mpv has no file open (o opens one)".into())
        );
        assert_eq!(
            mpv.join().unwrap(),
            [
                json!(["get_property", "time-pos"]),
                json!(["get_property", "path"])
            ]
        );
    }

    #[test]
    fn playback_waits_for_a_file_still_opening() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        // A file from a disk that has to spin up: mpv names it but has no position yet.
        let mpv = fake_mpv(UnixListener::bind(&socket).unwrap(), |c| {
            match c[1].as_str() {
                Some("path") => ok(moment().file),
                Some("time-pos") => error("property unavailable"),
                _ => ok(Value::Null),
            }
        });
        let mut player = Player::new("mpv", socket);
        assert_eq!(
            player.playback(Playback::Pause),
            Err("mpv is still opening the file".into())
        );
        assert_eq!(mpv.join().unwrap().len(), 2, "no pause sent");
    }

    #[test]
    fn another_error_asking_for_the_file_is_reported_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let mpv = fake_mpv(UnixListener::bind(&socket).unwrap(), |c| {
            match c[1].as_str() {
                Some("time-pos") => error("property unavailable"),
                _ => error("invalid parameter"),
            }
        });
        let mut player = Player::new("mpv", socket);
        assert_eq!(
            player.playback(Playback::Pause),
            Err("mpv: invalid parameter".into())
        );
        mpv.join().unwrap();
    }
}

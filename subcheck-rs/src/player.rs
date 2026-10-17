//! Opening a moment in mpv. One window serves a whole review: the first `o` starts mpv with an IPC
//! socket, and every later one loads the next file into that window.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::text::timestamp;

/// How long a new mpv gets to open its socket.
const START_TIMEOUT: Duration = Duration::from_secs(3);

/// A moment to open: a file, where to start, and which subtitle track to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moment {
    pub file: String,
    pub start_ms: i64,
    /// mpv's `--sid`.
    pub sid: Option<usize>,
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

/// The JSON IPC command that loads `m` in place of whatever is playing. Named arguments, because
/// mpv 0.38 put an `index` argument before `options`; `options` as an object, so no value needs
/// escaping (both checked against mpv 0.41).
pub fn loadfile_command(m: &Moment) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    options.insert("start".into(), timestamp(m.start_ms).into());
    if let Some(sid) = m.sid {
        options.insert("sid".into(), sid.to_string().into());
    }
    options.insert("sub-visibility".into(), "yes".into());
    serde_json::json!({
        "command": {"name": "loadfile", "url": m.file, "flags": "replace", "options": options}
    })
}

pub struct Player {
    program: OsString,
    socket: PathBuf,
    /// The mpv this review started, until it exits.
    child: Option<Child>,
}

impl Player {
    pub fn new(program: impl Into<OsString>, socket: PathBuf) -> Player {
        Player {
            program: program.into(),
            socket,
            child: None,
        }
    }

    /// Show `m` in the mpv listening on the socket, or start one and show it there.
    pub fn open(&mut self, m: &Moment) -> Result<(), String> {
        self.reap();
        let stream = match UnixStream::connect(&self.socket) {
            Ok(stream) => stream,
            Err(_) => self.start()?,
        };
        send(&stream, m)
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
    fn start(&mut self) -> Result<UnixStream, String> {
        if let Some(dir) = self.socket.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let mut server = OsString::from("--input-ipc-server=");
        server.push(&self.socket);
        let child = Command::new(&self.program)
            .arg(server)
            .args(["--idle=yes", "--force-window=yes", "--no-terminal"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own process group: the terminal's signals stay with the review, and the window
            // outlives it.
            .process_group(0)
            .spawn()
            .map_err(|e| {
                let program = self.program.to_string_lossy();
                if e.kind() == io::ErrorKind::NotFound {
                    format!("{program} not found: install mpv to open findings")
                } else {
                    format!("starting {program}: {e}")
                }
            })?;
        self.child = Some(child);
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(stream) = UnixStream::connect(&self.socket) {
                return Ok(stream);
            }
            if let Some(child) = &mut self.child {
                if let Ok(Some(status)) = child.try_wait() {
                    self.child = None;
                    return Err(format!("mpv exited before opening its socket ({status})"));
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "mpv did not open {} within {} s",
                    self.socket.display(),
                    START_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Send the `loadfile` and wait for mpv's answer to it. Event lines may come first.
fn send(stream: &UnixStream, m: &Moment) -> Result<(), String> {
    let fail = |e: io::Error| format!("talking to mpv: {e}");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(fail)?;
    let mut line = loadfile_command(m).to_string();
    line.push('\n');
    let mut writer = stream;
    writer.write_all(line.as_bytes()).map_err(fail)?;
    for reply in BufReader::new(stream).lines() {
        let reply = reply.map_err(fail)?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&reply) else {
            continue;
        };
        if let Some(error) = v.get("error").and_then(serde_json::Value::as_str) {
            return if error == "success" {
                Ok(())
            } else {
                Err(format!("mpv: {error}"))
            };
        }
    }
    Err("mpv closed the connection without answering".into())
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

    /// An mpv stand-in: answers one connection on `listener` with an event and then success, and
    /// returns the line it was sent.
    fn fake_mpv(listener: UnixListener) -> thread::JoinHandle<String> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let mut w = &stream;
            w.write_all(b"{\"event\":\"idle\"}\n{\"data\":{\"playlist_entry_id\":1},\"request_id\":0,\"error\":\"success\"}\n")
                .unwrap();
            line
        })
    }

    /// A script standing in for the mpv program: it writes its arguments to `argv` and stays
    /// up for a while, as mpv would.
    fn fake_program(dir: &Path) -> (PathBuf, PathBuf) {
        let argv = dir.join("argv");
        let script = dir.join("fake-mpv");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}.tmp'\nmv '{0}.tmp' '{0}'\nsleep 2\n",
                argv.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        (script, argv)
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
            serde_json::json!({"command": {
                "name": "loadfile",
                "url": "/m/K-On! - E02.mkv",
                "flags": "replace",
                "options": {"start": "0:02:43.06", "sid": "2", "sub-visibility": "yes"}
            }})
        );
        let no_sid = Moment {
            sid: None,
            ..moment()
        };
        assert!(loadfile_command(&no_sid)["command"]["options"]
            .get("sid")
            .is_none());
    }

    #[test]
    fn a_running_mpv_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let mpv = fake_mpv(UnixListener::bind(&socket).unwrap());
        // A program that cannot start: reusing the window must not try.
        let mut player = Player::new(dir.path().join("no-such-mpv"), socket);
        assert_eq!(player.open(&moment()), Ok(()));
        let sent: serde_json::Value = serde_json::from_str(&mpv.join().unwrap()).unwrap();
        assert_eq!(sent, loadfile_command(&moment()));
    }

    #[test]
    fn with_no_mpv_running_one_is_started_and_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sock/mpv.sock");
        let (program, argv) = fake_program(dir.path());
        // Stands in for the started mpv: opens the socket once the program has run.
        let server = {
            let (socket, argv) = (socket.clone(), argv.clone());
            thread::spawn(move || {
                while !argv.exists() {
                    thread::sleep(Duration::from_millis(10));
                }
                fake_mpv(UnixListener::bind(&socket).unwrap())
                    .join()
                    .unwrap()
            })
        };
        let mut player = Player::new(&program, socket.clone());
        assert_eq!(player.open(&moment()), Ok(()));
        let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(sent, loadfile_command(&moment()));
        assert_eq!(
            std::fs::read_to_string(&argv).unwrap(),
            format!(
                "--input-ipc-server={}\n--idle=yes\n--force-window=yes\n--no-terminal\n",
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
        let program = dir.path().join("false-mpv");
        std::fs::write(&program, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let mut player = Player::new(&program, dir.path().join("mpv.sock"));
        let err = player.open(&moment()).unwrap_err();
        assert!(err.contains("exited before opening its socket"), "{err}");
    }

    #[test]
    fn an_mpv_error_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let mut w = &stream;
            w.write_all(b"{\"request_id\":0,\"error\":\"invalid parameter\"}\n")
                .unwrap();
        });
        let mut player = Player::new("mpv", socket);
        assert_eq!(player.open(&moment()), Err("mpv: invalid parameter".into()));
        server.join().unwrap();
    }
}

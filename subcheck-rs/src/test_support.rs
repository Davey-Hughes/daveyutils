//! Helpers the library's tests share.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// An executable script `name` in `dir`: `#!/bin/sh` and `body`. A child writes it, not this
/// process. Tests run on threads that start processes, and a child forked while this process held
/// the script open for writing keeps it open until that child execs. Running the script then fails
/// with "Text file busy" (ETXTBSY).
pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut sh = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin
        .take()
        .unwrap()
        .write_all(format!("#!/bin/sh\n{body}").as_bytes())
        .unwrap();
    assert!(sh.wait().unwrap().success(), "writing {}", path.display());
    path
}

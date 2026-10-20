use std::path::{Path, PathBuf};

/// A copy of fixture `name`, in a temporary directory, with `edit` applied to the first of the
/// `count` places `pattern` occurs (the count is checked, so a regenerated fixture that moves the
/// bytes fails loudly instead of patching the wrong place).
#[allow(dead_code)] // only some test files patch fixtures
pub fn patched(
    name: &str,
    pattern: &[u8],
    count: usize,
    edit: impl Fn(&mut [u8]),
) -> (tempfile::TempDir, PathBuf) {
    let mut bytes = std::fs::read(fixture(name)).unwrap();
    let at = first_of(&bytes, pattern, count, name);
    edit(&mut bytes[at..at + pattern.len()]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

/// `bytes` with the first of the `count` places `from` occurs overwritten by `to`, which is as
/// long (both checked, as in [`patched`]).
#[allow(dead_code)] // only some test files patch files
pub fn patch(bytes: &mut [u8], from: &[u8], count: usize, to: &[u8]) {
    assert_eq!(from.len(), to.len(), "a patch keeps the length");
    let at = first_of(bytes, from, count, "the bytes");
    bytes[at..at + to.len()].copy_from_slice(to);
}

/// Where the first of the `count` places `pattern` occurs in `bytes` (of `what`, for the message)
/// is; the count is checked.
fn first_of(bytes: &[u8], pattern: &[u8], count: usize, what: &str) -> usize {
    let at: Vec<usize> = (0..bytes.len())
        .filter(|&i| bytes[i..].starts_with(pattern))
        .collect();
    assert_eq!(at.len(), count, "{pattern:02X?} in {what}");
    at[0]
}

/// A copy of fixture `name`, in a temporary directory.
#[allow(dead_code)] // only some test files copy fixtures
pub fn copied(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::copy(fixture(name), &path).unwrap();
    (dir, path)
}

/// Cut the file at `path` to its first `len` bytes, as an interrupted copy leaves one.
#[allow(dead_code)] // only some test files cut fixtures
pub fn cut(path: &Path, len: u64) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

/// Whether mkvmerge can be run. The tests that need it pass without checking anything when it
/// cannot (macOS CI has none), and say so; with SUBCHECK_NEED_MKVMERGE set (CI on Linux, and
/// dev/check-all.sh) they fail instead, so a broken install cannot pass unseen.
#[allow(dead_code)] // only the apply tests run mkvmerge
pub fn have_mkvmerge() -> bool {
    let ok = std::process::Command::new("mkvmerge")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let needed = std::env::var_os("SUBCHECK_NEED_MKVMERGE").is_some_and(|v| !v.is_empty());
    assert!(
        ok || !needed,
        "mkvmerge not found, and SUBCHECK_NEED_MKVMERGE is set"
    );
    if !ok {
        eprintln!("skipped: mkvmerge not found");
    }
    ok
}

/// Whether ffmpeg can be run, as [`have_mkvmerge`] asks of mkvmerge: the `audio` tests skip
/// without it, and fail instead with SUBCHECK_NEED_FFMPEG set.
#[allow(dead_code)] // only the audio tests run ffmpeg
pub fn have_ffmpeg() -> bool {
    let ok = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    let needed = std::env::var_os("SUBCHECK_NEED_FFMPEG").is_some_and(|v| !v.is_empty());
    assert!(
        ok || !needed,
        "ffmpeg not found, and SUBCHECK_NEED_FFMPEG is set"
    );
    if !ok {
        eprintln!("skipped: ffmpeg not found");
    }
    ok
}

/// A file under tests/fixtures/, built by dev/make-subcheck-fixtures.sh.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

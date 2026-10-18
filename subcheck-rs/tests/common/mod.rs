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
    let at: Vec<usize> = (0..bytes.len())
        .filter(|&i| bytes[i..].starts_with(pattern))
        .collect();
    assert_eq!(at.len(), count, "{pattern:02X?} in {name}");
    edit(&mut bytes[at[0]..at[0] + pattern.len()]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
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

/// A file under tests/fixtures/, built by dev/make-subcheck-fixtures.sh.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

//! `subcheck extract` end to end: the real binary, run in a temporary directory.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::fixture;

fn subcheck(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

#[test]
fn writes_the_default_name_in_the_current_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture("ass-plain.mkv");
    let out = subcheck(dir.path(), &["extract", file.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(dir.path().join("ass-plain.track0.ass")).unwrap(),
        std::fs::read(fixture("expected/ass-plain.track0.ass")).unwrap()
    );
}

#[test]
fn dash_writes_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture("multi.mkv");
    let out = subcheck(
        dir.path(),
        &["extract", file.to_str().unwrap(), "--track", "3", "-o", "-"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("expected/multi.track3.srt")).unwrap()
    );
}

#[test]
fn refuses_to_overwrite_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture("srt.mkv");
    std::fs::write(dir.path().join("srt.track0.srt"), "keep me").unwrap();
    let out = subcheck(dir.path(), &["extract", file.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--force"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("srt.track0.srt")).unwrap(),
        "keep me"
    );

    let out = subcheck(dir.path(), &["extract", file.to_str().unwrap(), "--force"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(dir.path().join("srt.track0.srt")).unwrap(),
        std::fs::read(fixture("expected/srt.track0.srt")).unwrap()
    );
}

#[test]
fn several_text_tracks_need_a_track_id() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture("multi.mkv");
    let out = subcheck(dir.path(), &["extract", file.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("3 text subtitle tracks"), "{err}");
    assert!(err.contains("1: S_TEXT/ASS eng English Full"), "{err}");
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "wrote a file anyway"
    );
}

#[test]
fn a_video_track_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture("multi.mkv");
    let out = subcheck(
        dir.path(),
        &["extract", file.to_str().unwrap(), "--track", "0"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a text subtitle track"));
}

#[test]
fn verbose_says_when_the_cues_could_not_be_checked() {
    let dir = tempfile::tempdir().unwrap();
    let unchecked = subcheck(
        dir.path(),
        &[
            "extract",
            fixture("ass-ffmpeg.mkv").to_str().unwrap(),
            "-o",
            "-",
            "-v",
        ],
    );
    let checked = subcheck(
        dir.path(),
        &[
            "extract",
            fixture("ass-plain.mkv").to_str().unwrap(),
            "-o",
            "-",
            "-v",
        ],
    );
    // ffmpeg writes no statistics tags, so nothing says how many blocks the Cues should cover.
    assert!(String::from_utf8_lossy(&unchecked.stderr).contains("no NUMBER_OF_FRAMES tag"));
    assert!(!String::from_utf8_lossy(&checked.stderr).contains("NUMBER_OF_FRAMES"));
}

#[test]
fn verbose_reports_the_read_path() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture("ass-nocues.mkv");
    let out = subcheck(
        dir.path(),
        &["extract", file.to_str().unwrap(), "-o", "-", "-v"],
    );
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("5 events via Walk"), "{err}");
}

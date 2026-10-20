//! `subcheck timing` end to end: the real binary, run in the fixtures directory.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::{fixture, patched};

fn subcheck(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .args(args)
        .current_dir(fixture(""))
        .output()
        .unwrap()
}

#[test]
fn reports_every_track_the_fix_for_a_shifted_one_and_what_shared_lines_give() {
    let out = subcheck(&["timing", "timing.mkv"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "timing.mkv
  track 1  Dialogue in sync       in sync          (85 times, 0 in 2 of 2 windows)
  track 2  Dialogue a frame late  1 frame late     (85 times, +1 in 2 of 2 windows)
           fix: mkvmerge --sync 2:-500
  track 3  Karaoke and effects    too few lines    (0 times)
  track 4  Signs a frame late     too few lines    (0 times)
           timed as track 2 (34 of 34 shared times): 1 frame late
           fix: mkvmerge --sync 4:-500
"
    );
}

#[test]
fn lang_narrows_the_tracks_and_a_video_without_enough_keyframes_is_unsupported() {
    // multi.mkv's video has a keyframe every 50 s: two in all.
    let out = subcheck(&["timing", "--lang", "jpn", "multi.mkv"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "multi.mkv\n  track 3  (ja)  unsupported: only 2 video keyframes in the Cues\n"
    );
}

#[test]
fn a_missing_path_or_an_unreadable_file_fails_the_run_but_not_the_other_files() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.mkv");
    std::fs::write(&junk, "not a Matroska file").unwrap();
    let out = subcheck(&[
        "timing",
        "missing.mkv",
        junk.to_str().unwrap(),
        "timing.mkv",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("missing.mkv: no such file or directory"),
        "{err}"
    );
    assert!(
        err.contains("junk.mkv: parse error at byte 0: not a Matroska file"),
        "{err}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("timing.mkv\n"), "{stdout}");
    assert!(stdout.contains("1 frame late"), "{stdout}");
}

/// Two seasons whose first episodes share a name: `Season 1/01.mkv`, `Season 2/01.mkv` and
/// `Season 2/02.mkv`, each a copy of timing.mkv.
fn seasons() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (season, episode) in [("Season 1", "01"), ("Season 2", "01"), ("Season 2", "02")] {
        std::fs::create_dir_all(dir.path().join(season)).unwrap();
        let to = dir.path().join(season).join(format!("{episode}.mkv"));
        std::fs::copy(fixture("timing.mkv"), to).unwrap();
    }
    dir
}

/// The block headers `subcheck timing <args>` prints, run in `dir`.
fn headers(dir: &Path, args: &[&str]) -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .arg("timing")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with(' '))
        .map(String::from)
        .collect()
}

#[test]
fn files_found_in_a_directory_are_headed_by_their_path_below_it() {
    let dir = seasons();
    assert_eq!(
        headers(dir.path(), &[dir.path().to_str().unwrap()]),
        ["Season 1/01.mkv", "Season 2/01.mkv", "Season 2/02.mkv"]
    );
}

#[test]
fn files_named_alike_on_the_command_line_are_headed_by_their_paths_as_given() {
    // As `subcheck timing */01.mkv` names them; 02.mkv shares its name with nothing.
    let dir = seasons();
    assert_eq!(
        headers(
            dir.path(),
            &["Season 1/01.mkv", "Season 2/01.mkv", "Season 2/02.mkv"]
        ),
        ["Season 1/01.mkv", "Season 2/01.mkv", "02.mkv"]
    );
}

#[test]
fn a_file_reached_through_two_arguments_is_headed_the_same_whatever_their_order() {
    let dir = seasons();
    assert_eq!(
        headers(dir.path(), &["Season 1/01.mkv", "."]),
        headers(dir.path(), &[".", "Season 1/01.mkv"])
    );
}

#[test]
fn a_track_that_cannot_be_read_fails_the_run_and_the_others_are_still_checked() {
    // The first block of track 1 (`A0 B0 A1 AA 82 …`, track number 2) gets track number 0, which is
    // no number at all.
    let header = [0xA0, 0xB0, 0xA1, 0xAA, 0x82, 0x00, 0x00, 0x00];
    let (_dir, path) = patched("timing.mkv", &header, 5, |b| b[4] = 0x00);
    let out = subcheck(&["timing", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("track 1  Dialogue in sync       cannot read: parse error at byte"),
        "{stdout}"
    );
    assert!(stdout.contains("1 frame late"), "{stdout}");
}

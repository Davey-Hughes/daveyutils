//! `subcheck audio` end to end: the real binary and the real ffmpeg, run in the fixtures
//! directory. Skipped (and said so) without ffmpeg.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use common::{fixture, have_ffmpeg};

fn subcheck(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .args(args)
        .current_dir(fixture(""))
        .output()
        .unwrap()
}

#[test]
fn reports_each_track_against_the_one_it_matches_with_each_fix() {
    if !have_ffmpeg() {
        return;
    }
    let out = subcheck(&["audio", "audio.mkv", "audio-ac3.mkv"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "audio-ac3.mkv
  track 0  FLAC            reference
  track 1  AC-3 by ffmpeg  5.3 ms late against track 0
           fix: mkvmerge --sync 1:-5, or --sync 0:5 if track 0 is the one off
audio.mkv
  track 0  Main                  reference
  track 1  27 ms late            27.0 ms late against track 0
           fix: mkvmerge --sync 1:-27
           tagged ja, track 0 en
  track 2  20 ms late from 0:16  drifts against track 0
           no one --sync fixes a drift; ms late by window: 0:06 +0.0  0:18 +20.0
  track 3  Other sound           no other track holds this audio
"
    );
}

#[test]
fn verbose_adds_every_pair_in_every_window() {
    if !have_ffmpeg() {
        return;
    }
    let out = subcheck(&["audio", "-v", "audio.mkv"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pairs: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("  tracks "))
        .collect();
    assert_eq!(pairs.len(), 6, "{stdout}");
    assert!(
        pairs[0].starts_with("  tracks 0 and 1: 0:06 +27.00 ms 1.00  0:18 +27.00 ms"),
        "{stdout}"
    );
}

#[test]
fn files_with_fewer_than_two_audio_tracks_say_so() {
    if !have_ffmpeg() {
        return;
    }
    // apply.mkv has one FLAC track; srt.mkv has none.
    let out = subcheck(&["audio", "apply.mkv", "srt.mkv"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "apply.mkv\n  one audio track, nothing to compare\nsrt.mkv\n  no audio tracks\n"
    );
}

#[test]
fn a_missing_ffmpeg_stops_it_before_the_first_file() {
    let out = subcheck(&["audio", "--ffmpeg", "/nonexistent/ffmpeg", "audio.mkv"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "subcheck: /nonexistent/ffmpeg not found: install ffmpeg, or name it with --ffmpeg\n"
    );
}

#[test]
fn an_ffmpeg_that_fails_fails_its_file_and_the_others_are_still_checked() {
    // Answers -version, refuses everything else.
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("ffmpeg");
    std::fs::write(
        &stub,
        "#!/bin/sh\n[ \"$1\" = -version ] && exit 0\necho 'first line' >&2\necho 'Invalid data found' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = subcheck(&[
        "audio",
        "--ffmpeg",
        stub.to_str().unwrap(),
        "audio.mkv",
        "srt.mkv",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(
            "audio.mkv: decoding the window at 6 s: ffmpeg failed (exit status: 1): Invalid data found"
        ),
        "{err}"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "srt.mkv\n  no audio tracks\n"
    );
}

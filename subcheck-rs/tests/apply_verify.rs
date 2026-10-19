//! `apply::verify` on real remuxes of the apply fixture: a good remux passes, and each kind of
//! difference is caught on its own. Needs mkvmerge; skipped (and said so) without it.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use common::{fixture, have_mkvmerge};
use subcheck::apply::remux::Replacement;
use subcheck::apply::{patch, remux, verify};
use subcheck::blocks::{self, Block};
use subcheck::{mkv, subs};

/// Patched blocks by track ID.
type Fixed = BTreeMap<usize, Vec<Block>>;

/// The fixture's Signs/Songs (3) and SDH (4) tracks with a fix each, written to `dir`: the
/// patched blocks, and the files for mkvmerge.
fn fixed(dir: &Path) -> (Fixed, Vec<Replacement>) {
    fix(
        &fixture("apply.mkv"),
        dir,
        &[(3, "Libary", "Library"), (4, "tehre", "there")],
    )
}

/// The tracks of `original` with each `(id, from, to)` typo fixed, written to `dir` as [`fixed`]
/// writes them.
fn fix(original: &Path, dir: &Path, typos: &[(usize, &str, &str)]) -> (Fixed, Vec<Replacement>) {
    let (r, layout) = mkv::open(original).unwrap();
    let mut blocks = BTreeMap::new();
    let mut files = Vec::new();
    for &(id, from, to) in typos {
        let track = &layout.tracks[id];
        let read = blocks::read_tracks(&r, &layout, &[track.number], false)
            .unwrap()
            .pop()
            .unwrap();
        let edits: BTreeMap<usize, Vec<u8>> = read
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let text = String::from_utf8(b.payload.clone()).unwrap();
                text.contains(from)
                    .then(|| (i, text.replace(from, to).into_bytes()))
            })
            .collect();
        assert!(!edits.is_empty());
        let kind = subs::Kind::of(track).unwrap();
        let patched = patch::edited(
            kind,
            &read.blocks,
            &patch::Edits {
                changed: edits,
                ..Default::default()
            },
        );
        let stem = original.file_stem().unwrap().to_string_lossy();
        let file = dir.join(format!("{stem}.track{id}.{}", kind.extension()));
        let mut out = Vec::new();
        subs::write_track(track, kind, &patched, layout.timestamp_scale, &mut out).unwrap();
        std::fs::write(&file, out).unwrap();
        blocks.insert(id, patched);
        files.push(Replacement {
            id,
            file,
            tags: None,
        });
    }
    (blocks, files)
}

/// Remux the fixture with `files` replacing their tracks into `dir`, with `extra` options for the
/// original (mkvmerge takes `--no-chapters` and the like before the file they apply to).
fn remuxed(dir: &Path, files: &[Replacement], extra: &[&str]) -> PathBuf {
    let (_, layout) = mkv::open(&fixture("apply.mkv")).unwrap();
    let out = dir.join("out.mkv");
    let mut args = remux::args(&fixture("apply.mkv"), &layout, files, &out);
    let original = args
        .iter()
        .position(|a| Path::new(a) == fixture("apply.mkv"))
        .unwrap();
    args.splice(original..original, extra.iter().map(OsString::from));
    remux::run("mkvmerge".as_ref(), &args).unwrap();
    out
}

fn compare(remuxed: &Path, fixed: &Fixed, full: bool) -> Vec<String> {
    compare_with(&fixture("apply.mkv"), remuxed, fixed, full)
}

fn compare_with(original: &Path, remuxed: &Path, fixed: &Fixed, full: bool) -> Vec<String> {
    let (r, layout) = mkv::open(original).unwrap();
    let (r2, layout2) = mkv::open(remuxed).unwrap();
    let fixed: BTreeMap<usize, &[Block]> = fixed.iter().map(|(k, v)| (*k, v.as_slice())).collect();
    verify::compare(
        verify::File {
            r: &r,
            layout: &layout,
        },
        verify::File {
            r: &r2,
            layout: &layout2,
        },
        &fixed,
        &BTreeMap::new(),
        full,
    )
    .unwrap()
}

#[test]
fn a_good_remux_passes_quick_and_full() {
    if !have_mkvmerge() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (blocks, files) = fixed(dir.path());
    let out = remuxed(dir.path(), &files, &[]);
    assert_eq!(compare(&out, &blocks, false), [] as [String; 0]);
    assert_eq!(compare(&out, &blocks, true), [] as [String; 0]);
}

#[test]
fn tracks_timed_by_their_default_duration_pass() {
    if !have_mkvmerge() {
        return;
    }
    // Two text tracks whose lines take their duration from the track's DefaultDuration, 2 s and
    // a 23.976 fps frame; the first gets a fix. mkvmerge (74, 82, 102 and 103) leaves a text track's
    // DefaultDuration out of any remux, fixed or not, and writes each line's duration instead,
    // rounded to the millisecond: no difference.
    let dir = tempfile::tempdir().unwrap();
    let original = fixture("timed.mkv");
    let (r, layout) = mkv::open(&original).unwrap();
    for (t, ns) in layout.tracks.iter().zip([2_000_000_000, 41_708_333]) {
        assert_eq!(t.default_duration, Some(ns));
        let read = blocks::read_tracks(&r, &layout, &[t.number], false)
            .unwrap()
            .pop()
            .unwrap();
        assert!(read.blocks.iter().all(|b| b.duration.is_none()), "{read:?}");
    }
    let remux = |typos: &[(usize, &str, &str)]| {
        let (blocks, files) = fix(&original, dir.path(), typos);
        let out = dir.path().join("out.mkv");
        remux::run(
            "mkvmerge".as_ref(),
            &remux::args(&original, &layout, &files, &out),
        )
        .unwrap();
        (out, blocks)
    };
    let (out, blocks) = remux(&[(0, "tehre", "there")]);
    assert_eq!(
        compare_with(&original, &out, &blocks, false),
        [] as [String; 0]
    );
    assert_eq!(
        compare_with(&original, &out, &blocks, true),
        [] as [String; 0]
    );
    // A fix on the frame-timed track itself: its lines go through the .ass file, which holds
    // their 42 ms only as 40.
    let (out, blocks) = remux(&[(1, "tehre", "there")]);
    assert_eq!(
        compare_with(&original, &out, &blocks, false),
        ["track 1: the event at 0:00:00.00 differs"]
    );
    // A remux that keeps the DefaultDuration, as no mkvmerge here does: the file as it is, and
    // the remux with the field put back beside the durations mkvmerge wrote.
    assert_eq!(
        compare_with(&original, &original, &Fixed::new(), false),
        [] as [String; 0]
    );
    let (out, blocks) = remux(&[(0, "tehre", "there")]);
    let o = std::process::Command::new("mkvpropedit")
        .arg(&out)
        .args(["--edit", "track:1", "--set", "default-duration=2000000000"])
        .args(["--edit", "track:2", "--set", "default-duration=41708333"])
        .output()
        .expect("running mkvpropedit, which comes with mkvmerge in mkvtoolnix");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let (_, kept) = mkv::open(&out).unwrap();
    assert_eq!(kept.tracks[1].default_duration, Some(41_708_333));
    assert_eq!(
        compare_with(&original, &out, &blocks, false),
        [] as [String; 0]
    );
}

#[test]
fn each_difference_is_caught_on_its_own() {
    if !have_mkvmerge() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (blocks, files) = fixed(dir.path());
    let one = |extra: &[&str]| compare(&remuxed(dir.path(), &files, extra), &blocks, false);
    assert_eq!(one(&["--no-attachments"]), ["attachments differ"]);
    assert_eq!(one(&["--no-chapters"]), ["chapters differ"]);
    assert_eq!(
        one(&["--title", "Other"]),
        ["title differs: Some(\"Other\"), was Some(\"Apply fixture\")"]
    );
    // A frame lost, as the statistics would say: the video's NUMBER_OF_FRAMES of 12 made 11.
    let out = remuxed(dir.path(), &files, &[]);
    let mut bytes = std::fs::read(&out).unwrap();
    let tag = b"NUMBER_OF_FRAMES\x44\x87\x8212";
    let at: Vec<usize> = (0..bytes.len())
        .filter(|&i| bytes[i..].starts_with(tag))
        .collect();
    assert_eq!(at.len(), 1);
    bytes[at[0] + tag.len() - 1] = b'1';
    std::fs::write(&out, bytes).unwrap();
    let lost = compare(&out, &blocks, false);
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(lost[0].starts_with("track 0: 11 frames of "), "{lost:?}");
    // An event fixed in the file that the expectation does not have.
    let out = remuxed(dir.path(), &files, &[]);
    let mut unfixed = blocks.clone();
    let (r, layout) = mkv::open(&fixture("apply.mkv")).unwrap();
    let signs = &layout.tracks[3];
    unfixed.insert(
        3,
        blocks::read_tracks(&r, &layout, &[signs.number], false)
            .unwrap()
            .pop()
            .unwrap()
            .blocks,
    );
    assert_eq!(
        compare(&out, &unfixed, false),
        ["track 3: the event at 0:00:02.00 differs"]
    );
}

#[test]
fn a_changed_track_name_or_a_dropped_track_is_caught() {
    if !have_mkvmerge() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (blocks, mut files) = fixed(dir.path());
    // The fixed Signs track named otherwise: its options come from the original, so rename the
    // original's view of it by remuxing with an extra option on the patched file.
    let (_, layout) = mkv::open(&fixture("apply.mkv")).unwrap();
    let out = dir.path().join("renamed.mkv");
    let mut args = remux::args(&fixture("apply.mkv"), &layout, &files, &out);
    let at = args
        .iter()
        .position(|a| a == "0:English Signs/Songs")
        .unwrap();
    args[at] = "0:Renamed".into();
    remux::run("mkvmerge".as_ref(), &args).unwrap();
    assert_eq!(compare(&out, &blocks, false), ["track 3: name differs"]);
    // Track 4 left out of the remux altogether.
    files.pop();
    let out = dir.path().join("dropped.mkv");
    let mut args = remux::args(&fixture("apply.mkv"), &layout, &files, &out);
    let tracks = args.iter().position(|a| a == "!3").unwrap();
    args[tracks] = "!3,4".into();
    let order = args.len() - 1;
    args[order] = "0:0,0:1,0:2,1:0".into();
    remux::run("mkvmerge".as_ref(), &args).unwrap();
    assert_eq!(compare(&out, &blocks, false), ["4 tracks, was 5"]);
}

#[test]
fn a_changed_video_frame_is_caught_by_the_full_check_only() {
    if !have_mkvmerge() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (blocks, files) = fixed(dir.path());
    let out = remuxed(dir.path(), &files, &[]);
    // One byte inside the first video frame: same sizes and counts, so the statistics agree.
    let (r, layout) = mkv::open(&out).unwrap();
    let mut first = None;
    blocks::each_frame(&r, &layout, |track, frame| {
        if track == layout.tracks[0].number && first.is_none() {
            first = Some(frame.to_vec());
        }
    })
    .unwrap();
    let frame = first.unwrap();
    let mut bytes = std::fs::read(&out).unwrap();
    let at = bytes.windows(frame.len()).position(|w| w == frame).unwrap() + frame.len() - 1;
    bytes[at] ^= 0xFF;
    std::fs::write(&out, bytes).unwrap();
    assert_eq!(compare(&out, &blocks, false), [] as [String; 0]);
    assert_eq!(compare(&out, &blocks, true), ["track 0: frames differ"]);
}

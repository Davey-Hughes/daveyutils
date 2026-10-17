//! `subs::extract` against mkvextract's own output for every text track in the fixtures
//! (`tests/fixtures/expected/`, made by dev/make-subcheck-fixtures.sh), through both read paths.

mod common;

use common::{fixture, patched};
use subcheck::{mkv, subs};

/// (fixture, mkvmerge track ID, expected file) for every text track in the fixtures.
const ORACLE: &[(&str, usize, &str)] = &[
    ("ass-plain.mkv", 0, "ass-plain.track0.ass"),
    ("ass-zlib.mkv", 0, "ass-zlib.track0.ass"),
    ("ass-nocues.mkv", 0, "ass-nocues.track0.ass"),
    ("ass-sync7.mkv", 0, "ass-sync7.track0.ass"),
    ("ass-ffmpeg.mkv", 0, "ass-ffmpeg.track0.ass"),
    ("ssa.mkv", 0, "ssa.track0.ssa"),
    ("srt.mkv", 0, "srt.track0.srt"),
    ("multi.mkv", 1, "multi.track1.ass"),
    ("multi.mkv", 2, "multi.track2.ass"),
    ("multi.mkv", 3, "multi.track3.srt"),
    ("empty.mkv", 1, "empty.track1.ass"),
    ("typos.mkv", 0, "typos.track0.ass"),
];

#[test]
fn extract_matches_mkvextract_byte_for_byte() {
    for &(file, id, expected) in ORACLE {
        let (r, layout) = mkv::open(&fixture(file)).unwrap();
        let track = layout.tracks.iter().find(|t| t.id == id).unwrap();
        let want = std::fs::read(fixture("expected").join(expected)).unwrap();
        for walk in [false, true] {
            let mut got = Vec::new();
            subs::extract(&r, &layout, track, walk, &mut got).unwrap();
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want),
                "{file} track {id} (walk: {walk})"
            );
        }
    }
}

/// mkvextract skips a block whose ReadOrder is not a number; so does extract, and its count of
/// events says so.
#[test]
fn a_block_whose_read_order_is_not_a_number_is_skipped_and_not_counted() {
    let (_dir, path) = patched("ass-plain.mkv", b"0,0,Default,amy", 1, |b| b[0] = b'x');
    let (r, layout) = mkv::open(&path).unwrap();
    let mut out = Vec::new();
    let done = subs::extract(&r, &layout, &layout.tracks[0], false, &mut out).unwrap();
    assert_eq!(done.events, 4);
    assert!(!String::from_utf8_lossy(&out).contains("amy"));
}

/// A fixture added to the script without an ORACLE row would go untested; this catches it.
#[test]
fn every_text_track_in_the_fixtures_is_in_the_oracle() {
    let dir = fixture("");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "mkv") {
            continue;
        }
        let file = path.file_name().unwrap().to_str().unwrap().to_string();
        let (_, layout) = mkv::open(&path).unwrap();
        for t in layout.tracks.iter().filter(|t| t.is_text_subtitle()) {
            assert!(
                ORACLE.iter().any(|&(f, id, _)| f == file && id == t.id),
                "{file} track {} has no ORACLE row",
                t.id
            );
        }
    }
}

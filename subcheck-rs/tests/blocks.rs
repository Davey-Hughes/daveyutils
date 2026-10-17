//! `blocks::read_tracks` on the committed fixtures: the Cues path, the walk, and their agreement.

mod common;

use common::{fixture, patched};
use subcheck::blocks::{self, ReadPath};
use subcheck::mkv;

#[test]
fn cues_path_is_taken_when_the_index_covers_the_track() {
    for file in [
        "ass-plain.mkv",
        "ass-zlib.mkv",
        "ass-sync7.mkv",
        "ass-ffmpeg.mkv",
        "ssa.mkv",
        "srt.mkv",
    ] {
        let (r, layout) = mkv::open(&fixture(file)).unwrap();
        let read = blocks::read_tracks(&r, &layout, &[1], false).unwrap();
        assert_eq!(read[0].path, ReadPath::Cues, "{file}");
        assert!(!read[0].blocks.is_empty(), "{file}");
    }
}

#[test]
fn walk_is_taken_when_the_track_has_no_cues() {
    let (r, layout) = mkv::open(&fixture("ass-nocues.mkv")).unwrap();
    let read = blocks::read_tracks(&r, &layout, &[1], false).unwrap();
    assert_eq!(read[0].path, ReadPath::Walk);
    assert_eq!(read[0].blocks.len(), 5);
}

#[test]
fn a_cue_pointing_at_the_wrong_byte_falls_back_to_the_walk() {
    // ass-plain's four cues with relative position 4 are `F0 81 04`; send the first one a byte
    // late, where it lands on no block at all.
    let (_dir, path) = patched("ass-plain.mkv", &[0xF0, 0x81, 0x04], 4, |b| b[2] = 0x05);
    let (r, layout) = mkv::open(&path).unwrap();
    let read = blocks::read_tracks(&r, &layout, &[1], false).unwrap();
    assert_eq!(read[0].path, ReadPath::Walk);
    assert_eq!(read[0].blocks.len(), 5);
}

#[test]
fn a_track_its_tags_call_empty_is_not_walked() {
    // Video plus an English track with no events: no Cues for it, and NUMBER_OF_FRAMES 0.
    let (r, layout) = mkv::open(&fixture("empty.mkv")).unwrap();
    let before = r.reads();
    let read = blocks::read_tracks(&r, &layout, &[2], false).unwrap();
    assert_eq!(read[0].path, ReadPath::Empty);
    assert_eq!(read[0].blocks.len(), 0);
    // A real 5.9 GB file took 372 s to walk, to learn the same thing.
    assert_eq!(r.reads(), before, "read the file to learn nothing");
}

#[test]
fn cues_and_walk_find_the_same_blocks_among_video() {
    let (r, layout) = mkv::open(&fixture("multi.mkv")).unwrap();
    let fast = blocks::read_tracks(&r, &layout, &[2, 3, 4], false).unwrap();
    let slow = blocks::read_tracks(&r, &layout, &[2, 3, 4], true).unwrap();
    for (f, s) in fast.iter().zip(&slow) {
        assert_eq!(
            (f.track, f.path, s.path),
            (s.track, ReadPath::Cues, ReadPath::Walk)
        );
        assert_eq!(f.blocks, s.blocks, "track {}", f.track);
    }
    assert_eq!(
        fast.iter().map(|t| t.blocks.len()).collect::<Vec<_>>(),
        [5, 5, 2]
    );
}

#[test]
fn zlib_payloads_come_out_decompressed() {
    let read = |file| {
        let (r, layout) = mkv::open(&fixture(file)).unwrap();
        let mut tracks = blocks::read_tracks(&r, &layout, &[1], false).unwrap();
        tracks.remove(0).blocks
    };
    let plain: Vec<Vec<u8>> = read("ass-plain.mkv")
        .into_iter()
        .map(|b| b.payload)
        .collect();
    let zlib: Vec<Vec<u8>> = read("ass-zlib.mkv")
        .into_iter()
        .map(|b| b.payload)
        .collect();
    assert_eq!(plain, zlib);
    assert!(plain[0].starts_with(b"0,0,Default,amy,"), "{:?}", plain[0]);
}

#[test]
fn cues_read_less_than_the_whole_file() {
    let (r, layout) = mkv::open(&fixture("multi.mkv")).unwrap();
    let before = r.bytes_read();
    blocks::read_tracks(&r, &layout, &[2], false).unwrap();
    // Five subtitle blocks in a 15 KB file of video: the fast path must not read it all.
    assert!(
        r.bytes_read() - before < r.file_size() / 2,
        "read {} of {}",
        r.bytes_read() - before,
        r.file_size()
    );
}

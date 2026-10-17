//! `mkv::read_layout` on the committed fixtures.

mod common;

use common::fixture;
use subcheck::mkv::{self, Encoding};

#[test]
fn layout_of_a_mkvmerge_file() {
    let (_, layout) = mkv::open(&fixture("multi.mkv")).unwrap();
    assert_eq!(layout.timestamp_scale, 1_000_000);
    let tracks: Vec<String> = layout
        .tracks
        .iter()
        .map(|t| {
            format!(
                "{} #{} {} {} {:?} default={}",
                t.id, t.number, t.codec, t.language, t.name, t.default
            )
        })
        .collect();
    assert_eq!(
        tracks,
        [
            // ffmpeg wrote the video with FlagDefault 0 and mkvmerge kept it.
            "0 #1 V_MPEG4/ISO/AVC und None default=false",
            "1 #2 S_TEXT/ASS eng Some(\"English Full\") default=true",
            "2 #3 S_TEXT/ASS eng Some(\"English Signs\") default=false",
            "3 #4 S_TEXT/UTF8 jpn None default=true",
        ]
    );
    let subtitle_uid = layout.tracks[1].uid;
    assert_eq!(layout.frame_counts.get(&subtitle_uid), Some(&5));
    assert!(layout.first_cluster.is_some());
}

#[test]
fn layout_of_an_ffmpeg_file() {
    let (_, layout) = mkv::open(&fixture("ass-ffmpeg.mkv")).unwrap();
    // One CuePoint holds both 1.00 s events: both positions must survive.
    assert_eq!(layout.cues.iter().filter(|c| c.time == 1000).count(), 2);
    assert!(
        layout.frame_counts.is_empty(),
        "ffmpeg writes no NUMBER_OF_FRAMES"
    );
}

#[test]
fn zlib_track_records_its_encoding() {
    let (_, layout) = mkv::open(&fixture("ass-zlib.mkv")).unwrap();
    let encodings = &layout.tracks[0].encodings;
    assert_eq!(encodings.len(), 1);
    assert_eq!(encodings[0].encoding, Encoding::Zlib);
    assert_eq!(encodings[0].scope, mkv::SCOPE_FRAMES);
}

#[test]
fn a_track_muxed_without_cues_has_none() {
    let (_, layout) = mkv::open(&fixture("ass-nocues.mkv")).unwrap();
    assert_eq!(layout.cues, [] as [mkv::Cue; 0]);
}

#[test]
fn not_matroska_is_a_parse_error() {
    let path = fixture("expected/srt.track0.srt");
    let err = mkv::open(&path).unwrap_err();
    assert!(err.to_string().contains("not a Matroska file"), "{err}");
}

/// ass-plain.mkv's Segment header: the ID, then the size as an 8-byte vint (0x19E1).
const SEGMENT_HEADER: [u8; 12] = [
    0x18, 0x53, 0x80, 0x67, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x19, 0xE1,
];

fn size_of(name: &str) -> u64 {
    std::fs::metadata(fixture(name)).unwrap().len()
}

/// Where ass-plain.mkv's first Cluster starts: a cut there keeps every element before it.
fn first_cluster() -> u64 {
    let (_, layout) = mkv::open(&fixture("ass-plain.mkv")).unwrap();
    layout.first_cluster.unwrap()
}

#[test]
fn a_whole_file_is_not_truncated() {
    let (_, layout) = mkv::open(&fixture("ass-plain.mkv")).unwrap();
    assert_eq!(layout.truncated, None);
}

#[test]
fn a_file_cut_short_is_truncated() {
    let (_dir, path) = common::copied("ass-plain.mkv");
    common::cut(&path, first_cluster());
    let (_, layout) = mkv::open(&path).unwrap();
    assert_eq!(
        layout.truncated,
        Some(mkv::Truncated {
            declared_end: size_of("ass-plain.mkv"),
            file_size: first_cluster(),
        })
    );
    assert_eq!(layout.tracks.len(), 1, "everything before the cut is read");
}

#[test]
fn a_segment_of_unknown_size_cut_short_is_not_truncated() {
    // The same cut, with only the Segment's size changed to "unknown": such a Segment ends
    // wherever the file does.
    let (_dir, path) = common::patched("ass-plain.mkv", &SEGMENT_HEADER, 1, |b| {
        b[4..].copy_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    });
    common::cut(&path, first_cluster());
    let (_, layout) = mkv::open(&path).unwrap();
    assert_eq!(layout.truncated, None);
    assert_eq!(layout.tracks.len(), 1);
}

#[test]
fn a_cut_inside_the_last_element_loses_only_that_element() {
    // One byte short cuts into the Tags at the end. They are lost, like anything past a cut, and
    // the rest of the file still reads.
    let (_, whole) = mkv::open(&fixture("ass-plain.mkv")).unwrap();
    assert!(!whole.frame_counts.is_empty());
    let (_dir, path) = common::copied("ass-plain.mkv");
    common::cut(&path, size_of("ass-plain.mkv") - 1);
    let (_, layout) = mkv::open(&path).unwrap();
    assert!(layout.truncated.is_some());
    assert!(layout.frame_counts.is_empty(), "the Tags were cut");
    assert_eq!(layout.cues, whole.cues);
    assert_eq!(layout.tracks.len(), 1);
}

/// ass-plain.mkv's Tracks header: the ID, then a 2-byte size.
const TRACKS_HEADER: [u8; 6] = [0x16, 0x54, 0xAE, 0x6B, 0x42, 0x75];
/// ass-plain.mkv's Tags header, the last element in the file: the ID, then a 2-byte size.
const TAGS_HEADER: [u8; 6] = [0x12, 0x54, 0xC3, 0x67, 0x41, 0x06];

/// Where `pattern`, found exactly once, starts in fixture `name`.
fn offset_of(name: &str, pattern: &[u8]) -> u64 {
    let bytes = std::fs::read(fixture(name)).unwrap();
    let at: Vec<usize> = (0..bytes.len())
        .filter(|&i| bytes[i..].starts_with(pattern))
        .collect();
    assert_eq!(at.len(), 1, "{pattern:02X?} in {name}");
    at[0] as u64
}

#[test]
fn a_cut_before_the_first_cluster_fails_and_says_the_file_is_truncated() {
    // Cut inside Tracks: nothing can be read, but the error says why.
    let cut = offset_of("ass-plain.mkv", &TRACKS_HEADER) + 16;
    let (_dir, path) = common::copied("ass-plain.mkv");
    common::cut(&path, cut);
    let err = mkv::open(&path).unwrap_err().to_string();
    assert!(
        err.starts_with(&format!(
            "truncated: file has {cut} of the {} bytes its Segment declares",
            size_of("ass-plain.mkv")
        )),
        "{err}"
    );
    assert!(
        err.contains("element runs past the end of the file"),
        "{err}"
    );

    // The same cut with a Segment of unknown size: the same failure, with no truncation to blame.
    let (_dir, path) = common::patched("ass-plain.mkv", &SEGMENT_HEADER, 1, |b| {
        b[4..].copy_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    });
    common::cut(&path, cut);
    let err = mkv::open(&path).unwrap_err().to_string();
    assert!(!err.contains("truncated"), "{err}");
    assert!(
        err.contains("element runs past the end of the file"),
        "{err}"
    );
}

#[test]
fn a_bad_header_before_the_cut_still_fails() {
    // The cut excuses only what it cuts: a whole header that does not parse is still an error.
    let (_dir, path) = common::patched("ass-plain.mkv", &TAGS_HEADER, 1, |b| b[0] = 0x00);
    common::cut(&path, size_of("ass-plain.mkv") - 1);
    let err = mkv::open(&path).unwrap_err().to_string();
    assert!(err.contains("bad element ID"), "{err}");
}

#[test]
fn an_unknown_size_index_element_fails_whether_the_file_is_cut_or_not() {
    // Tags of unknown size (a 2-byte size of all ones): unsupported in a whole file, and a cut
    // must not hide it.
    let unknown = |b: &mut [u8]| b[4..].copy_from_slice(&[0x7F, 0xFF]);
    let (_whole, path) = common::patched("ass-plain.mkv", &TAGS_HEADER, 1, unknown);
    let err = mkv::open(&path).unwrap_err().to_string();
    assert!(err.contains("unknown-size element"), "{err}");
    let (_cut, path) = common::patched("ass-plain.mkv", &TAGS_HEADER, 1, unknown);
    common::cut(&path, size_of("ass-plain.mkv") - 1);
    let err = mkv::open(&path).unwrap_err().to_string();
    assert!(err.contains("unknown-size element"), "{err}");
}

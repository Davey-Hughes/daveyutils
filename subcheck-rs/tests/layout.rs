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

//! `text::lines` on the typos fixture: what `scan` would send for a file.

mod common;

use common::fixture;
use subcheck::subs::Kind;
use subcheck::{blocks, mkv, text};

#[test]
fn typos_fixture_comes_out_as_four_clean_lines() {
    let (r, layout) = mkv::open(&fixture("typos.mkv")).unwrap();
    let track = &layout.tracks[0];
    let read = blocks::read_tracks(&r, &layout, &[track.number], false).unwrap();
    let events = text::events(
        track.id,
        Kind::of(track).unwrap(),
        &read[0].blocks,
        layout.timestamp_scale,
        track.default_duration,
    );
    let got: Vec<String> = text::lines(events)
        .iter()
        .map(|l| {
            let times: Vec<String> = l
                .occurrences
                .iter()
                .map(|o| text::timestamp(o.start_ms))
                .collect();
            format!("{} ({}) {}", times.join(","), l.style, l.text)
        })
        .collect();
    assert_eq!(
        got,
        [
            "0:00:01.00 (Default) We should leave befor it rains.",
            // Italics stripped; the later copy merged in as a second occurrence.
            "0:00:04.00,0:00:10.00 (Default) Nothing wrong here.",
            // Twelve per-letter events, written right to left, rebuilt by \pos.
            "0:00:20.00 (Song) go trough it",
            "0:00:30.00 (Default) The last line.",
        ]
    );
}

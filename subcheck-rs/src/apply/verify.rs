//! The check before a swap: whether a remuxed file is its original with the fixed tracks replaced
//! and nothing else changed. Every difference found is listed, so a failure says what differs.

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::blocks::{self, Block};
use crate::ebml::{self, Reader, Result};
use crate::mkv::{self, Flags, Layout, Track};
use crate::subs::{self, Kind};
use crate::text::timestamp;

/// An open file.
#[derive(Clone, Copy)]
pub struct File<'a> {
    pub r: &'a Reader,
    pub layout: &'a Layout,
}

/// What differs between `original`, with the blocks of `fixed` (by track ID) in place of its
/// own, and `remuxed`; empty when nothing does. `read` holds the original's text tracks as already
/// read, by track ID; any other is read here. `full` adds every frame of every other track,
/// compared by hash: it reads both files end to end.
pub fn compare(
    original: File,
    remuxed: File,
    fixed: &BTreeMap<usize, &[Block]>,
    read: &BTreeMap<usize, &[Block]>,
    full: bool,
) -> Result<Vec<String>> {
    let (a, b) = (original.layout, remuxed.layout);
    let mut diffs = Vec::new();
    // mkvmerge always writes one: a new UID where the original had none (ffmpeg's bitexact
    // output) is no difference, since nothing could have referred to it.
    if a.segment_uid.is_some() && a.segment_uid != b.segment_uid {
        diffs.push("segment UID differs".into());
    }
    if a.title != b.title {
        diffs.push(format!("title differs: {:?}, was {:?}", b.title, a.title));
    }
    let chapters_of = |f: File| {
        f.layout
            .chapters_at
            .map_or(Ok(None), |at| chapters(f.r, at))
    };
    if chapters_of(original)? != chapters_of(remuxed)? {
        diffs.push("chapters differ".into());
    }
    if attachments(original)? != attachments(remuxed)? {
        diffs.push("attachments differ".into());
    }
    if a.tracks.len() != b.tracks.len() {
        diffs.push(format!("{} tracks, was {}", b.tracks.len(), a.tracks.len()));
        return Ok(diffs);
    }
    for (t, u) in a.tracks.iter().zip(&b.tracks) {
        header(t, u, fixed.contains_key(&t.id), &mut diffs)?;
    }
    if !diffs.is_empty() {
        return Ok(diffs);
    }
    subtitles(original, remuxed, fixed, read, &mut diffs)?;
    others(original, remuxed, full, &mut diffs)?;
    Ok(diffs)
}

/// The Chapters element at `at`, as compared. Chapters that do not parse are an error, not "no
/// chapters": mkvmerge may drop what it cannot read.
fn chapters(r: &Reader, at: u64) -> Result<Option<Vec<(u32, Value)>>> {
    values(&r.data(&r.header_at(at)?)?, CHAPTER_MASTERS)
        .map(Some)
        .ok_or_else(|| ebml::Error::Unsupported("chapters that do not parse".into()))
}

/// One attached file as its elements, in order: (ID, data).
type Attached = Vec<(u32, Vec<u8>)>;

/// The attached files, each as its elements in order, the MIME type left out (mkvmerge 102
/// rewrites `application/x-truetype-font` as `font/ttf`) and the data as its length and hash.
fn attachments(f: File) -> Result<Vec<Attached>> {
    f.layout
        .attachments_at
        .map_or(Ok(Vec::new()), |at| attached_files(f.r, at))
}

/// The attached files of the Attachments element at `at`, one at a time: together they can
/// outgrow what one read may hold. Its CRC-32 or Void (ffmpeg writes a CRC-32 into each
/// top-level element) is no attached file.
fn attached_files(r: &Reader, at: u64) -> Result<Vec<Attached>> {
    let h = r.header_at(at)?;
    let end = h.end().unwrap_or(h.data_start);
    let mut out = Vec::new();
    let mut pos = h.data_start;
    while pos < end {
        let file = r.header_at(pos)?;
        if file.id == ATTACHED_FILE {
            out.push(attached(&r.data(&file)?, file.data_start)?);
        }
        pos = file.end().unwrap_or(end);
    }
    Ok(out)
}

/// One AttachedFile's data (at `base`) as compared: its elements but the MIME type, the file's
/// bytes as their length and hash.
fn attached(data: &[u8], base: u64) -> Result<Attached> {
    let mut elements = Vec::new();
    for (id, _, v) in ebml::children(data, base)? {
        match id {
            FILE_MIME_TYPE | mkv::CRC32 | mkv::VOID => {}
            FILE_DATA => elements.push((id, digest_of(v))),
            _ => elements.push((id, v.to_vec())),
        }
    }
    Ok(elements)
}

const ATTACHED_FILE: u32 = 0x61A7;
const FILE_MIME_TYPE: u32 = 0x4660;
const FILE_DATA: u32 = 0x465C;

fn digest_of(data: &[u8]) -> Vec<u8> {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    [data.len() as u64, h.finish()]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect()
}

/// A track's header against its original's. `fixed`: the track was replaced, so its UID is new
/// and its CodecPrivate rebuilt from the `.ass` header.
fn header(t: &Track, u: &Track, fixed: bool, diffs: &mut Vec<String>) -> Result<()> {
    let mut differs = |what: &str| diffs.push(format!("track {}: {what} differs", t.id));
    if t.kind != u.kind || t.codec != u.codec {
        differs("codec");
    }
    if t.name != u.name {
        differs("name");
    }
    // A new mkvmerge adds the BCP 47 tag an old one left out.
    if t.language != u.language
        || t.language_bcp47.is_some() && t.language_bcp47 != u.language_bcp47
    {
        differs("language");
    }
    if t.default != u.default || t.forced != u.forced || flags(&t.flags) != flags(&u.flags) {
        differs("flags");
    }
    if t.default_duration != u.default_duration {
        differs("default duration");
    }
    if t.encodings != u.encodings {
        differs("compression");
    }
    let private = |track: &Track| {
        blocks::decode(
            track.codec_private.clone(),
            &track.encodings,
            mkv::SCOPE_CODEC_PRIVATE,
        )
    };
    let (p, q) = (private(t)?, private(u)?);
    if if fixed {
        script(&p) != script(&q)
    } else {
        p != q
    } {
        differs("codec private data");
    }
    if !same_properties(t.video.as_deref(), u.video.as_deref())
        || !same_properties(t.audio.as_deref(), u.audio.as_deref())
    {
        differs("video or audio properties");
    }
    Ok(())
}

/// The flags with Matroska's defaults filled in: a file that leaves FlagEnabled out says the same
/// as one that sets it to 1.
fn flags(f: &Flags) -> [bool; 6] {
    [
        f.enabled.unwrap_or(true),
        f.hearing_impaired.unwrap_or(false),
        f.visual_impaired.unwrap_or(false),
        f.text_descriptions.unwrap_or(false),
        f.original.unwrap_or(false),
        f.commentary.unwrap_or(false),
    ]
}

/// An ASS header as mkvextract writes it and mkvmerge reads it back: LF line ends, no trailing
/// blank lines. Bytes, not text: a rewrite that changed bytes that are not UTF-8 must show.
fn script(private: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = subs::c_bytes(private);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    while out
        .last()
        .is_some_and(|b| matches!(b, b'\n' | b' ' | b'\t' | b'\r'))
    {
        out.pop();
    }
    out
}

/// Video and Audio elements whose children are floats; any other child of 8 bytes or fewer is an
/// unsigned integer. mkvmerge may write an integer wider than the original did (DisplayWidth in
/// 4 bytes where it had 2), so they are compared by value.
const FLOATS: &[u32] = &[
    0xB5,
    0x78B5,
    0x0023_83E3,
    0x55D1,
    0x55D2,
    0x55D3,
    0x55D4,
    0x55D5,
    0x55D6,
    0x55D7,
    0x55D8,
    0x55D9,
    0x55DA,
    0x7673,
    0x7674,
    0x7675,
];
/// Masters inside Video: Colour, MasteringMetadata, Projection.
const VIDEO_MASTERS: &[u32] = &[0x55B0, 0x55D0, 0x7670];
/// Masters inside Chapters: EditionEntry, ChapterAtom, ChapterTrack, ChapterDisplay, ChapProcess,
/// ChapProcessCommand, EditionDisplay.
const CHAPTER_MASTERS: &[u32] = &[0x45B9, 0xB6, 0x8F, 0x80, 0x6944, 0x6911, 0x4520];

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Value {
    Uint(u64),
    Float(u64),
    Raw(Vec<u8>),
    Master(Vec<(u32, Value)>),
}

fn same_properties(a: Option<&[u8]>, b: Option<&[u8]>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => values(a, VIDEO_MASTERS) == values(b, VIDEO_MASTERS),
        _ => false,
    }
}

/// A master's children as values, sorted, its CRC-32 and Void left out at every level, `masters`
/// parsed as masters in turn; `None` when they do not parse.
fn values(data: &[u8], masters: &[u32]) -> Option<Vec<(u32, Value)>> {
    let mut out = Vec::new();
    for (id, _, v) in ebml::children(data, 0).ok()? {
        if id == mkv::CRC32 || id == mkv::VOID {
            continue;
        }
        let value = if masters.contains(&id) {
            Value::Master(values(v, masters)?)
        } else {
            value(id, v)
        };
        out.push((id, value));
    }
    // By ID only, and stably: siblings of one ID (chapter atoms, editions) keep their order,
    // which is playback order.
    out.sort_by_key(|(id, _)| *id);
    Some(out)
}

/// A leaf as a value: a float by its value, anything else of 8 bytes or fewer as an unsigned
/// integer (mkvmerge may write one wider than the original did), longer data as it is.
fn value(id: u32, v: &[u8]) -> Value {
    if FLOATS.contains(&id) {
        let f = match v.len() {
            4 => v.try_into().map(|b| f64::from(f32::from_be_bytes(b))).ok(),
            8 => v.try_into().map(f64::from_be_bytes).ok(),
            _ => None,
        };
        if let Some(f) = f {
            return Value::Float(f.to_bits());
        }
    }
    if v.len() <= 8 {
        Value::Uint(ebml::uint(v))
    } else {
        Value::Raw(v.to_vec())
    }
}

/// Every text subtitle track, event by event: start, duration and text, against the original's
/// (or, for a fixed track, its patched blocks).
fn subtitles(
    original: File,
    remuxed: File,
    fixed: &BTreeMap<usize, &[Block]>,
    read: &BTreeMap<usize, &[Block]>,
    diffs: &mut Vec<String>,
) -> Result<()> {
    let text: Vec<(&Track, &Track)> = original
        .layout
        .tracks
        .iter()
        .zip(&remuxed.layout.tracks)
        .filter(|(t, _)| t.is_text_subtitle())
        .collect();
    let reread = |f: File, tracks: Vec<&Track>| {
        let numbers: Vec<u64> = tracks.iter().map(|t| t.number).collect();
        blocks::read_tracks(f.r, f.layout, &numbers, must_walk(f.layout, &tracks))
    };
    // What the caller has read already is not read again, nor are the fixed tracks: their patched
    // blocks are what to expect.
    let unread: Vec<&Track> = text
        .iter()
        .map(|(t, _)| *t)
        .filter(|t| !fixed.contains_key(&t.id) && !read.contains_key(&t.id))
        .collect();
    let before = if unread.is_empty() {
        Vec::new()
    } else {
        reread(original, unread)?
    };
    let after = reread(remuxed, text.iter().map(|(_, u)| *u).collect())?;
    for ((t, u), now) in text.iter().zip(&after) {
        let kind = Kind::of(t).expect("a text subtitle track");
        let expected: &[Block] = match fixed.get(&t.id).or(read.get(&t.id)) {
            Some(blocks) => blocks,
            None => {
                &before
                    .iter()
                    .find(|b| b.track == t.number)
                    .expect("every other text track was read here")
                    .blocks
            }
        };
        let want = events(
            kind,
            expected,
            original.layout.timestamp_scale,
            t,
            fixed.contains_key(&t.id),
        );
        let got = events(
            kind,
            &now.blocks,
            remuxed.layout.timestamp_scale,
            u,
            fixed.contains_key(&t.id),
        );
        if want.len() != got.len() {
            diffs.push(format!(
                "track {}: {} events, was {}",
                t.id,
                got.len(),
                want.len()
            ));
        } else if let Some(i) = (0..want.len()).find(|&i| want[i] != got[i]) {
            let at = timestamp(i64::try_from(want[i].0 / 1_000_000).unwrap_or(0));
            diffs.push(format!("track {}: the event at {at} differs", t.id));
        }
    }
    Ok(())
}

/// Whether reading `tracks` of a file must walk its Clusters: its Cues are trusted only where its
/// statistics say how many blocks they should lead to, and a track with Cues for only some of its
/// blocks would otherwise lose the rest unseen (mkvmerge writes the statistics; ffmpeg does not).
pub fn must_walk(layout: &Layout, tracks: &[&Track]) -> bool {
    tracks
        .iter()
        .any(|t| !layout.frame_counts.contains_key(&t.uid))
}

/// Each block as (start ns, duration ns, bytes up to its first NUL): ASS's ReadOrder as its rank
/// in the track (mkvmerge numbers events afresh, in file order), and for a rewritten SRT track no
/// CRs (mkvmerge puts CRLF between an event's rows whatever its source had; ffmpeg puts LF). Bytes,
/// not text: a rewrite that changed bytes that are not UTF-8 must show.
fn events(
    kind: Kind,
    blocks: &[Block],
    scale: u64,
    track: &Track,
    rewritten: bool,
) -> Vec<(i128, Option<i128>, Vec<u8>)> {
    let scale = i128::from(scale);
    let mut texts: Vec<Vec<u8>> = blocks
        .iter()
        .map(|b| subs::c_bytes(&b.payload).to_vec())
        .collect();
    if kind != Kind::Srt {
        let order = |t: &[u8]| {
            let digits = t.split(|&b| b == b',').next().unwrap_or_default();
            std::str::from_utf8(digits)
                .ok()
                .and_then(|o| o.parse::<i64>().ok())
                .unwrap_or(i64::MAX)
        };
        let mut orders: Vec<(i64, usize)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (order(t), i))
            .collect();
        orders.sort_unstable();
        for (rank, &(_, i)) in orders.iter().enumerate() {
            let rest = texts[i]
                .iter()
                .position(|&b| b == b',')
                .map_or(Vec::new(), |c| texts[i][c..].to_vec());
            texts[i] = [rank.to_string().into_bytes(), rest].concat();
        }
    } else if rewritten {
        for t in &mut texts {
            t.retain(|&b| b != b'\r');
        }
    }
    blocks
        .iter()
        .zip(texts)
        .map(|(b, text)| {
            let duration = b
                .duration
                .map(|d| i128::from(d) * scale)
                .or(track.default_duration.map(i128::from));
            (i128::from(b.timestamp) * scale, duration, text)
        })
        .collect()
}

/// Every other track (video, audio, image subtitles): its frame count and byte total, from both
/// files' statistics where both have them; else, and always when `full`, every frame by hash.
fn others(original: File, remuxed: File, full: bool, diffs: &mut Vec<String>) -> Result<()> {
    let (a, b) = (original.layout, remuxed.layout);
    let mut walk = Vec::new();
    for (t, u) in a.tracks.iter().zip(&b.tracks) {
        if t.is_text_subtitle() {
            continue;
        }
        let stats =
            |l: &Layout, uid: u64| Some((*l.frame_counts.get(&uid)?, *l.byte_counts.get(&uid)?));
        match (stats(a, t.uid), stats(b, u.uid)) {
            (Some(x), Some(y)) if !full => {
                if x != y {
                    diffs.push(format!(
                        "track {}: {} frames of {} bytes, was {} of {}",
                        t.id, y.0, y.1, x.0, x.1
                    ));
                }
            }
            _ => walk.push((t, u)),
        }
    }
    if walk.is_empty() {
        return Ok(());
    }
    let (x, y) = (digests(original)?, digests(remuxed)?);
    for (t, u) in walk {
        if x.get(&t.number) != y.get(&u.number) {
            diffs.push(format!("track {}: frames differ", t.id));
        }
    }
    Ok(())
}

/// Per track number: frame count, byte total and a hash of every frame in order.
fn digests(f: File) -> Result<HashMap<u64, (u64, u64, u64)>> {
    let mut state: HashMap<u64, (u64, u64, DefaultHasher)> = HashMap::new();
    blocks::each_frame(f.r, f.layout, |track, frame| {
        let (count, bytes, h) = state.entry(track).or_default();
        *count += 1;
        *bytes += frame.len() as u64;
        frame.hash(h);
    })?;
    Ok(state
        .into_iter()
        .map(|(track, (count, bytes, h))| (track, (count, bytes, h.finish())))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_compare_by_value_and_floats_as_floats() {
        // DisplayWidth (54B0) 1920 in 2 bytes and in 4.
        let narrow = [0x54, 0xB0, 0x82, 0x07, 0x80];
        let wide = [0x54, 0xB0, 0x84, 0x00, 0x00, 0x07, 0x80];
        assert!(same_properties(Some(&narrow), Some(&wide)));
        let other = [0x54, 0xB0, 0x82, 0x07, 0x81];
        assert!(!same_properties(Some(&narrow), Some(&other)));
        // SamplingFrequency (B5) 48000 as a 4-byte and an 8-byte float.
        let f4 = [&[0xB5, 0x84][..], &48000f32.to_be_bytes()].concat();
        let f8 = [&[0xB5, 0x88][..], &48000f64.to_be_bytes()].concat();
        assert!(same_properties(Some(&f4), Some(&f8)));
        assert!(!same_properties(Some(&f4), None));
    }

    #[test]
    fn flags_left_out_read_as_their_defaults() {
        let set = Flags {
            enabled: Some(true),
            commentary: Some(false),
            ..Flags::default()
        };
        assert_eq!(flags(&set), flags(&Flags::default()));
        let commentary = Flags {
            commentary: Some(true),
            ..Flags::default()
        };
        assert_ne!(flags(&commentary), flags(&Flags::default()));
    }

    #[test]
    fn an_attached_file_compares_without_its_mime_type() {
        // FileName "a.ttf", FileMimeType, FileData "xyz".
        let file = |mime: &[u8], data: &[u8]| {
            let mut v = vec![0x46, 0x6E, 0x85];
            v.extend_from_slice(b"a.ttf");
            v.extend_from_slice(&[0x46, 0x60, 0x80 | u8::try_from(mime.len()).unwrap()]);
            v.extend_from_slice(mime);
            v.extend_from_slice(&[0x46, 0x5C, 0x80 | u8::try_from(data.len()).unwrap()]);
            v.extend_from_slice(data);
            attached(&v, 0).unwrap()
        };
        assert_eq!(
            file(b"application/x-truetype-font", b"xyz"),
            file(b"font/ttf", b"xyz")
        );
        assert_ne!(file(b"font/ttf", b"xyz"), file(b"font/ttf", b"xyZ"));
    }

    fn track(language: &str, bcp47: Option<&str>) -> Track {
        Track {
            id: 2,
            number: 3,
            uid: 3,
            kind: mkv::TRACK_TYPE_SUBTITLE,
            codec: "S_TEXT/ASS".into(),
            name: None,
            language: language.into(),
            language_bcp47: bcp47.map(Into::into),
            default: true,
            forced: false,
            default_duration: None,
            codec_private: Vec::new(),
            encodings: Vec::new(),
            flags: Flags::default(),
            video: None,
            audio: None,
        }
    }

    #[test]
    fn a_bcp_47_tag_added_by_a_newer_mkvmerge_is_no_difference() {
        let diffs = |a: &Track, b: &Track| {
            let mut d = Vec::new();
            header(a, b, false, &mut d).unwrap();
            d
        };
        let old = track("eng", None);
        assert_eq!(diffs(&old, &track("eng", Some("en"))), [] as [String; 0]);
        assert_eq!(
            diffs(&track("eng", Some("en")), &track("eng", Some("en-US"))),
            ["track 2: language differs"]
        );
        assert_eq!(
            diffs(&old, &track("jpn", None)),
            ["track 2: language differs"]
        );
    }

    #[test]
    fn read_orders_compare_by_rank() {
        let t = track("eng", None);
        let block = |order: u32, ms: i64| Block {
            track: 3,
            position: 0,
            timestamp: ms,
            duration: Some(1000),
            payload: format!("{order},0,D,,0,0,0,,line {ms}").into_bytes(),
        };
        let numbered = |orders: [u32; 3]| {
            let blocks: Vec<Block> = orders
                .iter()
                .zip([0, 1000, 2000])
                .map(|(&o, ms)| block(o, ms))
                .collect();
            events(Kind::Ass, &blocks, 1_000_000, &t, true)
        };
        assert_eq!(numbered([7, 3, 9]), numbered([1, 0, 2]));
        assert_ne!(numbered([7, 3, 9]), numbered([0, 1, 2]));
    }

    #[test]
    fn a_rewritten_srt_track_compares_without_crs() {
        let t = Track {
            codec: "S_TEXT/UTF8".into(),
            ..track("eng", None)
        };
        let rows = |text: &str, rewritten| {
            let b = Block {
                track: 3,
                position: 0,
                timestamp: 0,
                duration: Some(1000),
                payload: text.as_bytes().to_vec(),
            };
            events(Kind::Srt, &[b], 1_000_000, &t, rewritten)
        };
        assert_eq!(rows("Two\r\nrows", true), rows("Two\nrows", true));
        assert_ne!(rows("Two\r\nrows", false), rows("Two\nrows", false));
    }

    /// One element whose size fits a 1-byte vint.
    fn el(id: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.push(0x80 | u8::try_from(data.len()).unwrap());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn chapters_compare_without_crc_or_void_and_integers_by_value() {
        // An EditionEntry with one ChapterAtom (UID, start time), as ffmpeg writes it (a CRC-32
        // first, the time in 1 byte) and as mkvmerge does (no CRC, the time in 2 bytes).
        let atom = |time: &[u8]| {
            el(
                &[0xB6],
                &[el(&[0x73, 0xC4], &[7]), el(&[0x91], time)].concat(),
            )
        };
        let ffmpeg = el(
            &[0x45, 0xB9],
            &[el(&[0xBF], &[1, 2, 3, 4]), atom(&[5])].concat(),
        );
        let mkvmerge = el(&[0x45, 0xB9], &atom(&[0, 5]));
        assert_eq!(
            values(&ffmpeg, CHAPTER_MASTERS),
            values(&mkvmerge, CHAPTER_MASTERS)
        );
        let later = el(&[0x45, 0xB9], &atom(&[6]));
        assert_ne!(
            values(&ffmpeg, CHAPTER_MASTERS),
            values(&later, CHAPTER_MASTERS)
        );
    }

    #[test]
    fn chapter_atoms_compare_in_their_order() {
        // An ordered edition plays its atoms in order: the same atoms swapped are a difference.
        let atom = |uid: u8| el(&[0xB6], &el(&[0x73, 0xC4], &[uid]));
        let one = el(&[0x45, 0xB9], &[atom(1), atom(2)].concat());
        let other = el(&[0x45, 0xB9], &[atom(2), atom(1)].concat());
        assert_ne!(
            values(&one, CHAPTER_MASTERS),
            values(&other, CHAPTER_MASTERS)
        );
    }

    #[test]
    fn chapters_that_do_not_parse_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c");
        // A Chapters element whose child claims more bytes than there are.
        std::fs::write(&path, el(&[0x10, 0x43, 0xA7, 0x70], &[0x45, 0xB9, 0x88, 1])).unwrap();
        let err = chapters(&Reader::open(&path).unwrap(), 0).unwrap_err();
        assert!(
            err.to_string().contains("chapters that do not parse"),
            "{err}"
        );
    }

    #[test]
    fn a_header_compares_as_bytes() {
        let latin1 = [b"Style: Caf".as_slice(), &[0xE9]].concat();
        assert_ne!(script(&latin1), script("Style: Caf\u{FFFD}".as_bytes()));
        assert_eq!(script(b"A\r\nB\r\n\r\n"), script(b"A\nB"));
    }

    #[test]
    fn an_attachments_crc_is_no_attached_file() {
        let file = el(
            &[0x61, 0xA7],
            &[el(&[0x46, 0x6E], b"a.ttf"), el(&[0x46, 0x5C], b"xyz")].concat(),
        );
        let dir = tempfile::tempdir().unwrap();
        let read = |children: &[u8]| {
            let path = dir.path().join("a");
            std::fs::write(&path, el(&[0x19, 0x41, 0xA4, 0x69], children)).unwrap();
            attached_files(&Reader::open(&path).unwrap(), 0).unwrap()
        };
        let with_crc = read(&[el(&[0xBF], &[1, 2, 3, 4]), file.clone()].concat());
        assert_eq!(with_crc, read(&file));
        assert_eq!(with_crc.len(), 1);
    }

    #[test]
    fn bytes_that_are_not_utf8_compare_as_bytes() {
        // Latin-1 é, and what a lossy rewrite would make of it.
        let t = track("eng", None);
        let line = |bytes: &[u8]| {
            let b = Block {
                track: 3,
                position: 0,
                timestamp: 0,
                duration: Some(1000),
                payload: [b"0,0,D,,0,0,0,,caf".as_slice(), bytes].concat(),
            };
            events(Kind::Ass, &[b], 1_000_000, &t, true)
        };
        assert_ne!(line(&[0xE9]), line("\u{FFFD}".as_bytes()));
    }

    #[test]
    fn an_ass_header_compares_without_crlf_or_trailing_blank_lines() {
        assert_eq!(
            script(b"[Script Info]\r\nA: 1\r\n\r\n"),
            script(b"[Script Info]\nA: 1\n")
        );
        assert_ne!(script(b"A: 1\n"), script(b"A: 2\n"));
    }
}

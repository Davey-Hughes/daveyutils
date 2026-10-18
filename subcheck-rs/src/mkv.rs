//! Matroska layout: the parts of a file's header and index that subcheck needs, read without
//! touching any Cluster data.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::ebml::{self, Error, Header, Reader, Result};

pub const EBML: u32 = 0x1A45_DFA3;
pub const SEGMENT: u32 = 0x1853_8067;
pub const SEEK_HEAD: u32 = 0x114D_9B74;
pub const SEEK: u32 = 0x4DBB;
pub const SEEK_ID: u32 = 0x53AB;
pub const SEEK_POSITION: u32 = 0x53AC;
pub const INFO: u32 = 0x1549_A966;
pub const TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
pub const TRACKS: u32 = 0x1654_AE6B;
pub const TRACK_ENTRY: u32 = 0xAE;
pub const TRACK_NUMBER: u32 = 0xD7;
pub const TRACK_UID: u32 = 0x73C5;
pub const TRACK_TYPE: u32 = 0x83;
pub const FLAG_DEFAULT: u32 = 0x88;
pub const FLAG_FORCED: u32 = 0x55AA;
pub const DEFAULT_DURATION: u32 = 0x0023_E383;
pub const NAME: u32 = 0x536E;
pub const LANGUAGE: u32 = 0x0022_B59C;
pub const LANGUAGE_BCP47: u32 = 0x0022_B59D;
pub const CODEC_ID: u32 = 0x86;
pub const CODEC_PRIVATE: u32 = 0x63A2;
pub const CONTENT_ENCODINGS: u32 = 0x6D80;
pub const CONTENT_ENCODING: u32 = 0x6240;
pub const CONTENT_ENCODING_ORDER: u32 = 0x5031;
pub const CONTENT_ENCODING_SCOPE: u32 = 0x5032;
pub const CONTENT_ENCODING_TYPE: u32 = 0x5033;
pub const CONTENT_COMPRESSION: u32 = 0x5034;
pub const CONTENT_COMP_ALGO: u32 = 0x4254;
pub const CONTENT_COMP_SETTINGS: u32 = 0x4255;
pub const CUES: u32 = 0x1C53_BB6B;
pub const CUE_POINT: u32 = 0xBB;
pub const CUE_TIME: u32 = 0xB3;
pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
pub const CUE_TRACK: u32 = 0xF7;
pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
pub const CUE_RELATIVE_POSITION: u32 = 0xF0;
pub const TAGS: u32 = 0x1254_C367;
pub const TAG: u32 = 0x7373;
pub const TARGETS: u32 = 0x63C0;
pub const TAG_TRACK_UID: u32 = 0x63C5;
pub const SIMPLE_TAG: u32 = 0x67C8;
pub const TAG_NAME: u32 = 0x45A3;
pub const TAG_STRING: u32 = 0x4487;
pub const CLUSTER: u32 = 0x1F43_B675;
pub const TIMESTAMP: u32 = 0xE7;
pub const SIMPLE_BLOCK: u32 = 0xA3;
pub const BLOCK_GROUP: u32 = 0xA0;
pub const BLOCK: u32 = 0xA1;
pub const BLOCK_DURATION: u32 = 0x9B;

pub const TRACK_TYPE_VIDEO: u64 = 0x01;
pub const TRACK_TYPE_SUBTITLE: u64 = 0x11;

/// The ContentEncodingScope bit for frame contents; 2 is CodecPrivate.
pub const SCOPE_FRAMES: u64 = 1;
pub const SCOPE_CODEC_PRIVATE: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoding {
    Zlib,
    /// Header stripping: these bytes were removed from the front of every frame.
    HeaderStrip(Vec<u8>),
    /// Any other compression, or encryption, by name.
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentEncoding {
    pub order: u64,
    /// Bit flags: [`SCOPE_FRAMES`], [`SCOPE_CODEC_PRIVATE`].
    pub scope: u64,
    pub encoding: Encoding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// mkvmerge's track ID: the 0-based position of the TrackEntry within Tracks.
    pub id: usize,
    pub number: u64,
    pub uid: u64,
    pub kind: u64,
    pub codec: String,
    pub name: Option<String>,
    /// ISO 639-2. Matroska's default when the element is absent is "eng".
    pub language: String,
    pub language_bcp47: Option<String>,
    pub default: bool,
    pub forced: bool,
    /// Nanoseconds.
    pub default_duration: Option<u64>,
    pub codec_private: Vec<u8>,
    /// Highest ContentEncodingOrder first: the order to undo them in.
    pub encodings: Vec<ContentEncoding>,
}

impl Track {
    pub fn is_text_subtitle(&self) -> bool {
        self.kind == TRACK_TYPE_SUBTITLE
            && matches!(
                self.codec.as_str(),
                "S_TEXT/ASS" | "S_TEXT/SSA" | "S_TEXT/UTF8"
            )
    }
}

/// One CueTrackPositions entry, flattened with its CuePoint's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cue {
    /// TimestampScale units.
    pub time: u64,
    pub track: u64,
    /// Relative to the Segment's data, as stored.
    pub cluster: u64,
    /// Relative to the Cluster's data.
    pub relative: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Absolute offset of the Segment's data: Cues and SeekHead positions count from here.
    pub segment_data: u64,
    /// One past the Segment's last byte (the end of the file for an unknown-size Segment).
    pub segment_end: u64,
    /// Nanoseconds per timestamp unit.
    pub timestamp_scale: u64,
    pub tracks: Vec<Track>,
    pub cues: Vec<Cue>,
    /// `NUMBER_OF_FRAMES` statistics tags, by track UID.
    pub frame_counts: HashMap<u64, u64>,
    /// Absolute offset of the first Cluster.
    pub first_cluster: Option<u64>,
    /// Set when the Segment's size says the file should be longer than it is.
    pub truncated: Option<Truncated>,
}

/// A file cut short: its Segment declares more bytes than the file has. An interrupted copy or
/// download leaves one; whatever lies past the cut (often the Cues and Tags) is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Truncated {
    /// One past the Segment's last byte, as its size declares: the length the file should have.
    pub declared_end: u64,
    pub file_size: u64,
}

impl Truncated {
    /// The line that reports it: `warning: <file>: truncated: …`.
    pub fn warning(&self, file: &Path) -> String {
        format!("warning: {}: {self}", file.display())
    }

    /// How much is there: `file has 1000 of the 2000 bytes its Segment declares (50.0%)`.
    pub fn shortfall(&self) -> String {
        // Tenths of a percent, in integers.
        let permille = u128::from(self.file_size) * 1000 / u128::from(self.declared_end.max(1));
        format!(
            "file has {} of the {} bytes its Segment declares ({}.{}%)",
            self.file_size,
            self.declared_end,
            permille / 10,
            permille % 10
        )
    }
}

impl std::fmt::Display for Truncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "truncated: {}", self.shortfall())
    }
}

/// Open a file and read its layout.
pub fn open(path: &Path) -> Result<(Reader, Layout)> {
    let r = Reader::open(path)?;
    let layout = read_layout(&r)?;
    Ok((r, layout))
}

pub fn read_layout(r: &Reader) -> Result<Layout> {
    let ebml_header = r.header_at(0)?;
    if ebml_header.id != EBML {
        return ebml::parse_err(0, "not a Matroska file (no EBML header)");
    }
    let Some(seg_at) = ebml_header.end() else {
        return Err(Error::Unsupported("unknown-size EBML header".into()));
    };
    let seg = r.header_at(seg_at)?;
    if seg.id != SEGMENT {
        return ebml::parse_err(seg_at, "no Segment after the EBML header");
    }
    // A Segment of unknown size ends where the file does. One of known size that ends past it is
    // read up to the end of the file, and reported.
    let truncated = seg
        .end()
        .filter(|&end| end > r.file_size())
        .map(|declared_end| Truncated {
            declared_end,
            file_size: r.file_size(),
        });
    let segment_end = seg.end().map_or(r.file_size(), |e| e.min(r.file_size()));
    let mut layout = Layout {
        segment_data: seg.data_start,
        segment_end,
        timestamp_scale: 1_000_000,
        tracks: Vec::new(),
        cues: Vec::new(),
        frame_counts: HashMap::new(),
        first_cluster: None,
        truncated,
    };
    match read_index(r, &mut layout) {
        Ok(()) => Ok(layout),
        // Whatever failed, in a file cut short the missing bytes are the likely reason.
        Err(e) => Err(match layout.truncated {
            Some(t) => Error::Truncated {
                truncated: t.to_string(),
                cause: Box::new(e),
            },
            None => e,
        }),
    }
}

/// The top-level elements before the first Cluster, then what the SeekHeads point to.
fn read_index(r: &Reader, layout: &mut Layout) -> Result<()> {
    let mut loaded = HashSet::new();
    let mut pending = Vec::new();

    // Everything before the first Cluster, in order.
    let mut pos = layout.segment_data;
    while pos < layout.segment_end {
        let h = r.header_at(pos)?;
        if h.id == CLUSTER {
            layout.first_cluster = Some(pos);
            break;
        }
        load(r, &h, layout, &mut pending, &mut loaded)?;
        let Some(end) = h.end() else {
            return Err(Error::Unsupported(format!(
                "unknown-size element {:#X} at byte {pos}",
                h.id
            )));
        };
        pos = end;
    }
    // Then what the SeekHeads point to that the walk did not reach: Cues and Tags usually sit
    // after the Clusters.
    while let Some(at) = pending.pop() {
        if !loaded.contains(&at) {
            // A cut can fall inside an indexed element (the Cues or Tags at the end, usually): it
            // went with the rest of the tail, like the elements wholly past the cut. Only the cut
            // is excused: a header with all its bytes there must still parse.
            let cut_in_header = layout.truncated.is_some()
                && r.file_size().saturating_sub(at) < ebml::MAX_HEADER as u64;
            let h = match r.header_at(at) {
                Ok(h) => h,
                Err(Error::Parse { .. }) if cut_in_header => continue,
                Err(e) => return Err(e),
            };
            if layout.truncated.is_some() && h.end().is_some_and(|end| end > r.file_size()) {
                continue;
            }
            load(r, &h, layout, &mut pending, &mut loaded)?;
        }
    }
    Ok(())
}

/// Parse one top-level element into `layout`, queueing what a SeekHead points to.
fn load(
    r: &Reader,
    h: &Header,
    layout: &mut Layout,
    pending: &mut Vec<u64>,
    loaded: &mut HashSet<u64>,
) -> Result<()> {
    if !loaded.insert(h.offset) {
        return Ok(());
    }
    match h.id {
        SEEK_HEAD => {
            let data = r.data(h)?;
            for at in parse_seek_head(&data, h.data_start, layout.segment_data)? {
                if at < layout.segment_end {
                    pending.push(at);
                }
            }
        }
        INFO => layout.timestamp_scale = parse_info(&r.data(h)?, h.data_start)?,
        TRACKS => layout.tracks = parse_tracks(&r.data(h)?, h.data_start)?,
        CUES => layout.cues = parse_cues(&r.data(h)?, h.data_start)?,
        TAGS => layout.frame_counts = parse_tags(&r.data(h)?, h.data_start)?,
        _ => {}
    }
    Ok(())
}

/// Absolute offsets of the elements a SeekHead indexes, for the kinds [`load`] parses.
fn parse_seek_head(data: &[u8], base: u64, segment_data: u64) -> Result<Vec<u64>> {
    let mut out = Vec::new();
    for (id, at, seek) in ebml::children(data, base)? {
        if id != SEEK {
            continue;
        }
        let mut target = None;
        let mut position = None;
        for (cid, _, v) in ebml::children(seek, at)? {
            match cid {
                SEEK_ID => target = u32::try_from(ebml::uint(v)).ok(),
                SEEK_POSITION => position = Some(ebml::uint(v)),
                _ => {}
            }
        }
        if let (Some(target), Some(position)) = (target, position) {
            if matches!(target, SEEK_HEAD | INFO | TRACKS | CUES | TAGS) {
                out.push(segment_data + position);
            }
        }
    }
    Ok(out)
}

fn parse_info(data: &[u8], base: u64) -> Result<u64> {
    let mut scale = 1_000_000;
    for (id, _, v) in ebml::children(data, base)? {
        if id == TIMESTAMP_SCALE {
            scale = ebml::uint(v);
        }
    }
    if scale == 0 {
        return ebml::parse_err(base, "TimestampScale is 0");
    }
    Ok(scale)
}

fn parse_tracks(data: &[u8], base: u64) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();
    for (id, at, entry) in ebml::children(data, base)? {
        if id == TRACK_ENTRY {
            tracks.push(parse_track(tracks.len(), entry, at)?);
        }
    }
    Ok(tracks)
}

fn parse_track(id: usize, data: &[u8], base: u64) -> Result<Track> {
    let mut t = Track {
        id,
        number: 0,
        uid: 0,
        kind: 0,
        codec: String::new(),
        name: None,
        language: "eng".into(),
        language_bcp47: None,
        default: true,
        forced: false,
        default_duration: None,
        codec_private: Vec::new(),
        encodings: Vec::new(),
    };
    for (cid, at, v) in ebml::children(data, base)? {
        match cid {
            TRACK_NUMBER => t.number = ebml::uint(v),
            TRACK_UID => t.uid = ebml::uint(v),
            TRACK_TYPE => t.kind = ebml::uint(v),
            FLAG_DEFAULT => t.default = ebml::uint(v) != 0,
            FLAG_FORCED => t.forced = ebml::uint(v) != 0,
            DEFAULT_DURATION => t.default_duration = Some(ebml::uint(v)),
            NAME => t.name = Some(ebml::string(v)),
            LANGUAGE => t.language = ebml::string(v),
            LANGUAGE_BCP47 => t.language_bcp47 = Some(ebml::string(v)),
            CODEC_ID => t.codec = ebml::string(v),
            CODEC_PRIVATE => t.codec_private = v.to_vec(),
            CONTENT_ENCODINGS => t.encodings = parse_encodings(v, at)?,
            _ => {}
        }
    }
    Ok(t)
}

fn parse_encodings(data: &[u8], base: u64) -> Result<Vec<ContentEncoding>> {
    let mut out = Vec::new();
    for (id, at, enc) in ebml::children(data, base)? {
        if id != CONTENT_ENCODING {
            continue;
        }
        let (mut order, mut scope, mut kind) = (0, SCOPE_FRAMES, 0);
        let mut compression: (u64, &[u8]) = (at, &[]);
        for (cid, cat, v) in ebml::children(enc, at)? {
            match cid {
                CONTENT_ENCODING_ORDER => order = ebml::uint(v),
                CONTENT_ENCODING_SCOPE => scope = ebml::uint(v),
                CONTENT_ENCODING_TYPE => kind = ebml::uint(v),
                CONTENT_COMPRESSION => compression = (cat, v),
                _ => {}
            }
        }
        let encoding = if kind == 0 {
            // A ContentCompression with no ContentCompAlgo is zlib: the element's default.
            let (mut algo, mut settings) = (0, Vec::new());
            for (cid, _, v) in ebml::children(compression.1, compression.0)? {
                match cid {
                    CONTENT_COMP_ALGO => algo = ebml::uint(v),
                    CONTENT_COMP_SETTINGS => settings = v.to_vec(),
                    _ => {}
                }
            }
            match algo {
                0 => Encoding::Zlib,
                3 => Encoding::HeaderStrip(settings),
                other => Encoding::Unsupported(format!("compression algorithm {other}")),
            }
        } else {
            Encoding::Unsupported("encryption".into())
        };
        out.push(ContentEncoding {
            order,
            scope,
            encoding,
        });
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.order));
    Ok(out)
}

fn parse_cues(data: &[u8], base: u64) -> Result<Vec<Cue>> {
    let mut out = Vec::new();
    for (id, at, point) in ebml::children(data, base)? {
        if id != CUE_POINT {
            continue;
        }
        let kids = ebml::children(point, at)?;
        let Some(time) = kids
            .iter()
            .find(|(cid, _, _)| *cid == CUE_TIME)
            .map(|(_, _, v)| ebml::uint(v))
        else {
            continue;
        };
        for (cid, pat, positions) in kids {
            if cid != CUE_TRACK_POSITIONS {
                continue;
            }
            let (mut track, mut cluster, mut relative) = (None, None, None);
            for (pid, _, v) in ebml::children(positions, pat)? {
                match pid {
                    CUE_TRACK => track = Some(ebml::uint(v)),
                    CUE_CLUSTER_POSITION => cluster = Some(ebml::uint(v)),
                    CUE_RELATIVE_POSITION => relative = Some(ebml::uint(v)),
                    _ => {}
                }
            }
            if let (Some(track), Some(cluster)) = (track, cluster) {
                out.push(Cue {
                    time,
                    track,
                    cluster,
                    relative,
                });
            }
        }
    }
    Ok(out)
}

fn parse_tags(data: &[u8], base: u64) -> Result<HashMap<u64, u64>> {
    let mut out = HashMap::new();
    for (id, at, tag) in ebml::children(data, base)? {
        if id != TAG {
            continue;
        }
        let mut uids = Vec::new();
        let mut frames = None;
        for (cid, cat, v) in ebml::children(tag, at)? {
            match cid {
                TARGETS => {
                    for (tid, _, tv) in ebml::children(v, cat)? {
                        if tid == TAG_TRACK_UID {
                            uids.push(ebml::uint(tv));
                        }
                    }
                }
                SIMPLE_TAG => {
                    let (mut name, mut value) = (String::new(), String::new());
                    for (sid, _, sv) in ebml::children(v, cat)? {
                        match sid {
                            TAG_NAME => name = ebml::string(sv),
                            TAG_STRING => value = ebml::string(sv),
                            _ => {}
                        }
                    }
                    if name == "NUMBER_OF_FRAMES" {
                        frames = value.trim().parse::<u64>().ok();
                    }
                }
                _ => {}
            }
        }
        if let Some(frames) = frames {
            for uid in uids {
                out.insert(uid, frames);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode one element whose size fits a 1-byte vint.
    fn el(id: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.push(0x80 | u8::try_from(data.len()).unwrap());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn truncation_reads_as_bytes_present_of_bytes_declared() {
        // The copy of Tokyo Ghoul Root A E01 that found this: 4.2% of the file is there.
        let t = Truncated {
            declared_end: 7_976_268_468,
            file_size: 338_132_992,
        };
        assert_eq!(
            t.to_string(),
            "truncated: file has 338132992 of the 7976268468 bytes its Segment declares (4.2%)"
        );
    }

    #[test]
    fn encodings_default_to_zlib_on_frames_and_sort_highest_order_first() {
        let zlib = el(&[0x62, 0x40], &el(&[0x50, 0x34], &[]));
        let strip = el(
            &[0x62, 0x40],
            &[
                el(&[0x50, 0x31], &[1]),
                el(
                    &[0x50, 0x34],
                    &[el(&[0x42, 0x54], &[3]), el(&[0x42, 0x55], b"Di")].concat(),
                ),
            ]
            .concat(),
        );
        let encs = parse_encodings(&[zlib, strip].concat(), 0).unwrap();
        assert_eq!(
            encs,
            vec![
                ContentEncoding {
                    order: 1,
                    scope: SCOPE_FRAMES,
                    encoding: Encoding::HeaderStrip(b"Di".to_vec())
                },
                ContentEncoding {
                    order: 0,
                    scope: SCOPE_FRAMES,
                    encoding: Encoding::Zlib
                },
            ]
        );
    }

    #[test]
    fn encryption_and_unknown_compression_are_unsupported() {
        let encrypted = el(&[0x62, 0x40], &el(&[0x50, 0x33], &[1]));
        let lzo = el(&[0x62, 0x40], &el(&[0x50, 0x34], &el(&[0x42, 0x54], &[2])));
        let encs = parse_encodings(&[encrypted, lzo].concat(), 0).unwrap();
        assert_eq!(encs[0].encoding, Encoding::Unsupported("encryption".into()));
        assert_eq!(
            encs[1].encoding,
            Encoding::Unsupported("compression algorithm 2".into())
        );
    }

    #[test]
    fn a_track_without_language_is_english() {
        let t = parse_track(3, &el(&[0xD7], &[2]), 0).unwrap();
        assert_eq!((t.id, t.number, t.language.as_str()), (3, 2, "eng"));
        assert!(t.default && !t.forced);
    }

    #[test]
    fn a_parse_error_inside_a_track_reports_its_own_byte_offset() {
        // Tracks data at byte 1000: a TrackEntry (AE, size 4) holding a TrackNumber and then a
        // 0x00 byte, which starts no element. The entry's data begins at 1002; the bad byte is 1005.
        let data = [0xAE, 0x84, 0xD7, 0x81, 0x01, 0x00];
        let err = parse_tracks(&data, 1000).unwrap_err();
        assert!(matches!(err, Error::Parse { offset: 1005, .. }), "{err}");
    }

    #[test]
    fn cue_points_flatten_every_track_position() {
        let positions = |track: u8, rel: u8| {
            el(
                &[0xB7],
                &[
                    el(&[0xF7], &[track]),
                    el(&[0xF1], &[0x10]),
                    el(&[0xF0], &[rel]),
                ]
                .concat(),
            )
        };
        let point = el(
            &[0xBB],
            &[el(&[0xB3], &[0x64]), positions(1, 4), positions(1, 60)].concat(),
        );
        let cues = parse_cues(&point, 0).unwrap();
        assert_eq!(
            cues,
            vec![
                Cue {
                    time: 100,
                    track: 1,
                    cluster: 16,
                    relative: Some(4)
                },
                Cue {
                    time: 100,
                    track: 1,
                    cluster: 16,
                    relative: Some(60)
                },
            ]
        );
    }
}

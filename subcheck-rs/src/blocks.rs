//! Every block of the chosen tracks: through the Cues when they cover a track, by walking the
//! Clusters when they do not.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;

use crate::ebml::{self, Error, Header, Reader, Result};
use crate::mkv::{self, ContentEncoding, Cue, Encoding, Layout, Track};

/// The largest block taken at its word. Subtitle blocks are bytes to kilobytes; a bigger size is a
/// corrupt one, and reading it would pull the rest of a multi-GB file into memory first.
const MAX_BLOCK: u64 = 16 << 20;

/// How far past a requested offset a read extends. Subtitle blocks are small and their cues are
/// sorted by position, so neighbours in one Cluster usually come out of a single read.
const WINDOW: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub track: u64,
    /// Absolute offset of the SimpleBlock or BlockGroup element: file order.
    pub position: u64,
    /// The Cluster's timestamp plus the block's own, in TimestampScale units.
    pub timestamp: i64,
    /// BlockDuration in TimestampScale units, when the block has one.
    pub duration: Option<u64>,
    /// The frame, with its content encodings undone.
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPath {
    Cues,
    Walk,
    /// Nothing read: the track's NUMBER_OF_FRAMES statistic is 0.
    Empty,
}

#[derive(Debug)]
pub struct TrackBlocks {
    pub track: u64,
    pub path: ReadPath,
    /// File order.
    pub blocks: Vec<Block>,
}

/// The blocks of the tracks numbered `numbers` (TrackNumber, not mkvmerge ID). `force_walk`
/// ignores the Cues, to check the fast path against the slow one.
pub fn read_tracks(
    r: &Reader,
    layout: &Layout,
    numbers: &[u64],
    force_walk: bool,
) -> Result<Vec<TrackBlocks>> {
    let mut found: BTreeMap<u64, (ReadPath, Vec<Block>)> = BTreeMap::new();
    let mut walk = HashSet::new();
    for &n in numbers {
        let track = find_track(layout, n)?;
        if force_walk {
            walk.insert(n);
        } else if layout.frame_counts.get(&track.uid) == Some(&0) {
            // The statistics say the track has no blocks. It has no Cues either, so without this
            // the walk would read a whole file to confirm it: 372 s for a real 5.9 GB episode.
            found.insert(n, (ReadPath::Empty, Vec::new()));
        } else if let Some(blocks) = via_cues(r, layout, track)? {
            found.insert(n, (ReadPath::Cues, blocks));
        } else {
            walk.insert(n);
        }
    }
    if !walk.is_empty() {
        let mut walked = walk_clusters(r, layout, &walk)?;
        for &n in &walk {
            found.insert(n, (ReadPath::Walk, walked.remove(&n).unwrap_or_default()));
        }
    }
    let mut out = Vec::new();
    for &n in numbers {
        let Some((path, mut blocks)) = found.remove(&n) else {
            continue;
        };
        let track = find_track(layout, n)?;
        for b in &mut blocks {
            b.payload = decode(
                std::mem::take(&mut b.payload),
                &track.encodings,
                mkv::SCOPE_FRAMES,
            )?;
        }
        out.push(TrackBlocks {
            track: n,
            path,
            blocks,
        });
    }
    Ok(out)
}

fn find_track(layout: &Layout, number: u64) -> Result<&Track> {
    layout
        .tracks
        .iter()
        .find(|t| t.number == number)
        .ok_or_else(|| Error::Unsupported(format!("no track number {number}")))
}

/// Undo the content encodings that apply to `scope` ([`mkv::SCOPE_FRAMES`] or
/// [`mkv::SCOPE_CODEC_PRIVATE`]), highest order first.
pub fn decode(data: Vec<u8>, encodings: &[ContentEncoding], scope: u64) -> Result<Vec<u8>> {
    let mut data = data;
    for enc in encodings.iter().filter(|e| e.scope & scope != 0) {
        data = match &enc.encoding {
            Encoding::Zlib => {
                let mut out = Vec::new();
                flate2::read::ZlibDecoder::new(&data[..]).read_to_end(&mut out)?;
                out
            }
            Encoding::HeaderStrip(prefix) => [prefix.as_slice(), &data].concat(),
            Encoding::Unsupported(what) => return Err(Error::Unsupported(what.clone())),
        };
    }
    Ok(data)
}

/// The track's blocks, read through the Cues; `None` when the Cues cannot be trusted for it.
fn via_cues(r: &Reader, layout: &Layout, track: &Track) -> Result<Option<Vec<Block>>> {
    let mut cues: Vec<&Cue> = layout
        .cues
        .iter()
        .filter(|c| c.track == track.number)
        .collect();
    if cues.is_empty() || cues.iter().any(|c| c.relative.is_none()) {
        return Ok(None);
    }
    cues.sort_by_key(|c| (c.cluster, c.relative));
    cues.dedup_by_key(|c| (c.cluster, c.relative));
    if let Some(&frames) = layout.frame_counts.get(&track.uid) {
        if (cues.len() as u64) < frames {
            return Ok(None);
        }
    }
    // Every Cluster head and block this loop will read. A block sits at Cluster start + header
    // (at most 12 bytes) + relative position, so starting at Cluster start + relative covers it.
    r.prefetch(cues.iter().flat_map(|c| {
        let at = layout.segment_data + c.cluster;
        [
            (at, 64),
            (at + c.relative.unwrap_or_default(), WINDOW as u64 + 12),
        ]
    }));
    let mut window = Window::default();
    let mut clusters: HashMap<u64, (u64, i64)> = HashMap::new();
    let mut blocks = Vec::with_capacity(cues.len());
    for cue in cues {
        let at = layout.segment_data + cue.cluster;
        let (data_start, cluster_ts) = if let Some(&c) = clusters.get(&at) {
            c
        } else {
            let Some(c) = cluster_timestamp(r, &mut window, at)? else {
                return Ok(None);
            };
            clusters.insert(at, c);
            c
        };
        let position = data_start + cue.relative.unwrap_or_default();
        // A cue that leads to something other than a well-formed block is a Cues problem, not a
        // file problem: the walk may still read the file. Only an I/O error is final.
        let parsed = match window
            .element(r, position)
            .and_then(|element| parse_block_element(&element, position))
        {
            Ok(parsed) => parsed,
            Err(Error::Io(e)) => return Err(Error::Io(e)),
            Err(_) => return Ok(None),
        };
        let timestamp = cluster_ts + i64::from(parsed.relative);
        if parsed.track != track.number || u64::try_from(timestamp).ok() != Some(cue.time) {
            return Ok(None);
        }
        blocks.push(Block {
            track: parsed.track,
            position,
            timestamp,
            duration: parsed.duration,
            payload: parsed.payload,
        });
    }
    blocks.sort_by_key(|b| b.position);
    Ok(Some(blocks))
}

/// The data start and Timestamp of the Cluster at `at`, or `None` when there is no Cluster there
/// or its Timestamp is not among its first elements (CRC-32 and Void may come first).
fn cluster_timestamp(r: &Reader, window: &mut Window, at: u64) -> Result<Option<(u64, i64)>> {
    let head = window.get(r, at, 64)?.to_vec();
    let Ok(h) = ebml::parse_header(&head, at) else {
        return Ok(None);
    };
    if h.id != mkv::CLUSTER {
        return Ok(None);
    }
    let mut pos = h.data_start;
    loop {
        let Some(rest) = head.get(usize::try_from(pos - at).unwrap_or(usize::MAX)..) else {
            return Ok(None);
        };
        let Ok(c) = ebml::parse_header(rest, pos) else {
            return Ok(None);
        };
        let Some(end) = c.end() else {
            return Ok(None);
        };
        match c.id {
            mkv::TIMESTAMP => {
                let from = usize::try_from(c.data_start - at).unwrap_or(usize::MAX);
                let to = usize::try_from(end - at).unwrap_or(usize::MAX);
                let Some(v) = head.get(from..to) else {
                    return Ok(None);
                };
                let ts = i64::try_from(ebml::uint(v)).unwrap_or(i64::MAX);
                return Ok(Some((h.data_start, ts)));
            }
            0xBF | 0xEC => pos = end, // CRC-32, Void
            _ => return Ok(None),
        }
    }
}

/// The bytes most recently read, so cues pointing into them cost no further read.
#[derive(Default)]
struct Window {
    start: u64,
    buf: Vec<u8>,
}

impl Window {
    /// Up to `want` bytes at `offset` (fewer only where the file ends).
    fn get(&mut self, r: &Reader, offset: u64, want: usize) -> Result<&[u8]> {
        let cached_end = self.start + self.buf.len() as u64;
        let inside = offset >= self.start && offset + want as u64 <= cached_end;
        if !inside {
            self.buf = r.peek(offset, want.max(WINDOW))?;
            self.start = offset;
        }
        let from = usize::try_from(offset - self.start).unwrap_or(usize::MAX);
        let to = from.saturating_add(want).min(self.buf.len());
        Ok(&self.buf[from..to])
    }

    /// The whole element at `offset`, header included.
    fn element(&mut self, r: &Reader, offset: u64) -> Result<Vec<u8>> {
        let h = ebml::parse_header(self.get(r, offset, 12)?, offset)?;
        let Some(size) = h.size else {
            return ebml::parse_err(offset, "unknown-size block");
        };
        if size > MAX_BLOCK {
            return ebml::parse_err(offset, format!("block of {size} bytes is too large"));
        }
        let total = usize::try_from(h.data_start - offset + size).unwrap_or(usize::MAX);
        let buf = self.get(r, offset, total)?;
        if buf.len() < total {
            return ebml::parse_err(offset, "block runs past the end of the file");
        }
        Ok(buf.to_vec())
    }
}

#[derive(Debug)]
struct Parsed {
    track: u64,
    relative: i16,
    duration: Option<u64>,
    payload: Vec<u8>,
}

/// A SimpleBlock or BlockGroup, from `buf` holding the whole element as read at `offset`.
fn parse_block_element(buf: &[u8], offset: u64) -> Result<Parsed> {
    let h = ebml::parse_header(buf, offset)?;
    let Some(size) = h.size else {
        return ebml::parse_err(offset, "unknown-size block");
    };
    let start = usize::try_from(h.data_start - offset).unwrap_or(usize::MAX);
    let end = usize::try_from(size)
        .ok()
        .and_then(|s| start.checked_add(s));
    let Some(data) = end.and_then(|end| buf.get(start..end)) else {
        return ebml::parse_err(offset, "block truncated");
    };
    match h.id {
        mkv::SIMPLE_BLOCK => parse_block(data, h.data_start, None),
        mkv::BLOCK_GROUP => {
            let mut block = None;
            let mut duration = None;
            for (id, at, child) in ebml::children(data, h.data_start)? {
                match id {
                    mkv::BLOCK => block = Some((at, child)),
                    mkv::BLOCK_DURATION => duration = Some(ebml::uint(child)),
                    _ => {}
                }
            }
            let Some((at, block)) = block else {
                return ebml::parse_err(offset, "BlockGroup without a Block");
            };
            parse_block(block, at, duration)
        }
        other => ebml::parse_err(
            offset,
            format!("expected a block, found element {other:#X}"),
        ),
    }
}

/// A Block or SimpleBlock body: track number, 16-bit relative timestamp, flags, frame.
fn parse_block(data: &[u8], at: u64, duration: Option<u64>) -> Result<Parsed> {
    let Some((track, n, _)) = ebml::read_vint(data) else {
        return ebml::parse_err(at, "bad block track number");
    };
    let Some(&[hi, lo, flags]) = data.get(n..n + 3) else {
        return ebml::parse_err(at, "block too short");
    };
    if flags & 0x06 != 0 {
        return Err(Error::Unsupported(format!(
            "laced subtitle block at byte {at}"
        )));
    }
    Ok(Parsed {
        track,
        relative: i16::from_be_bytes([hi, lo]),
        duration,
        payload: data[n + 3..].to_vec(),
    })
}

/// Every block of `tracks`, found by walking the Clusters element by element. Reads only element
/// headers and the first bytes of each block, except for the blocks it keeps.
fn walk_clusters(
    r: &Reader,
    layout: &Layout,
    tracks: &HashSet<u64>,
) -> Result<HashMap<u64, Vec<Block>>> {
    let mut out: HashMap<u64, Vec<Block>> = HashMap::new();
    let Some(mut pos) = layout.first_cluster else {
        return Ok(out);
    };
    while pos < layout.segment_end {
        let h = match r.header_at(pos) {
            Ok(h) => h,
            // The cut in a file cut short fell inside this header: the walk is over.
            Err(Error::Parse { .. }) if cut_in_header(r, layout, pos) => break,
            Err(e) => return Err(e),
        };
        let Some(end) = h.end() else {
            return Err(Error::Unsupported(format!(
                "unknown-size element {:#X} at byte {pos}",
                h.id
            )));
        };
        if h.id == mkv::CLUSTER {
            // The Cluster the cut falls in is read up to the cut.
            let cut = layout.truncated.is_some() && end > layout.segment_end;
            walk_cluster(r, &h, end.min(layout.segment_end), cut, tracks, &mut out)?;
        }
        pos = end;
    }
    Ok(out)
}

/// Whether a file cut short ends within an element header's length of `pos`.
fn cut_in_header(r: &Reader, layout: &Layout, pos: u64) -> bool {
    layout.truncated.is_some() && r.file_size().saturating_sub(pos) < ebml::MAX_HEADER as u64
}

/// One Cluster, whose data runs from `cluster.data_start` to `end`. `cut`: the file ends at `end`,
/// inside this Cluster, so an element that runs past it is where the walk stops, not an error.
fn walk_cluster(
    r: &Reader,
    cluster: &Header,
    end: u64,
    cut: bool,
    tracks: &HashSet<u64>,
    out: &mut HashMap<u64, Vec<Block>>,
) -> Result<()> {
    let mut cluster_ts = None;
    let mut pos = cluster.data_start;
    while pos < end {
        let head = r.peek(pos, 32)?;
        let c = match ebml::parse_header(&head, pos) {
            Ok(c) => c,
            Err(Error::Parse { .. }) if cut && end - pos < ebml::MAX_HEADER as u64 => break,
            Err(e) => return Err(e),
        };
        let Some(c_end) = c.end() else {
            return ebml::parse_err(pos, "unknown-size element inside a Cluster");
        };
        if cut && c_end > end {
            break;
        }
        match c.id {
            mkv::TIMESTAMP => {
                let v = bytes_at(r, &head, pos, c.data_start, c_end - c.data_start)?;
                cluster_ts = Some(i64::try_from(ebml::uint(&v)).unwrap_or(i64::MAX));
            }
            mkv::SIMPLE_BLOCK | mkv::BLOCK_GROUP
                if tracks.contains(&block_track(r, &c, &head)?) =>
            {
                let Some(ts) = cluster_ts else {
                    return ebml::parse_err(pos, "block before its Cluster's Timestamp");
                };
                if c_end > end {
                    return ebml::parse_err(pos, "block runs past the end of its Cluster");
                }
                if c_end - pos > MAX_BLOCK {
                    return ebml::parse_err(
                        pos,
                        format!("block of {} bytes is too large", c_end - pos),
                    );
                }
                let len = usize::try_from(c_end - pos).unwrap_or(usize::MAX);
                let parsed = parse_block_element(&r.read_exact(pos, len)?, pos)?;
                out.entry(parsed.track).or_default().push(Block {
                    track: parsed.track,
                    position: pos,
                    timestamp: ts + i64::from(parsed.relative),
                    duration: parsed.duration,
                    payload: parsed.payload,
                });
            }
            _ => {}
        }
        pos = c_end;
    }
    Ok(())
}

/// The track number of the SimpleBlock or BlockGroup `h`, whose first bytes are `head`.
fn block_track(r: &Reader, h: &Header, head: &[u8]) -> Result<u64> {
    let body_at = if h.id == mkv::SIMPLE_BLOCK {
        h.data_start
    } else {
        // A BlockGroup's Block is normally its first child; look further only when it is not.
        let end = h.end().unwrap_or(h.data_start);
        let mut pos = h.data_start;
        loop {
            if pos >= end {
                return ebml::parse_err(h.offset, "BlockGroup without a Block");
            }
            let child = ebml::parse_header(&bytes_at(r, head, h.offset, pos, 12)?, pos)?;
            if child.id == mkv::BLOCK {
                break child.data_start;
            }
            let Some(child_end) = child.end() else {
                return ebml::parse_err(pos, "unknown-size element inside a BlockGroup");
            };
            pos = child_end;
        }
    };
    let body = bytes_at(r, head, h.offset, body_at, 8)?;
    match ebml::read_vint(&body) {
        Some((track, _, _)) => Ok(track),
        None => ebml::parse_err(body_at, "bad block track number"),
    }
}

/// `n` bytes at `at` (fewer where the file ends): from `head`, read at `head_at`, when it holds
/// them, else from the file.
fn bytes_at<'a>(
    r: &Reader,
    head: &'a [u8],
    head_at: u64,
    at: u64,
    n: u64,
) -> Result<Cow<'a, [u8]>> {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    let skip = usize::try_from(at.saturating_sub(head_at)).unwrap_or(usize::MAX);
    match head.get(skip..skip.saturating_add(n)) {
        Some(s) if at >= head_at => Ok(Cow::Borrowed(s)),
        _ => Ok(Cow::Owned(r.peek(at, n)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn decode_undoes_zlib_then_header_strip_in_order() {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(b"logue: hello").unwrap();
        let encodings = [
            ContentEncoding {
                order: 1,
                scope: mkv::SCOPE_FRAMES,
                encoding: Encoding::Zlib,
            },
            ContentEncoding {
                order: 0,
                scope: mkv::SCOPE_FRAMES,
                encoding: Encoding::HeaderStrip(b"Dia".to_vec()),
            },
        ];
        let out = decode(z.finish().unwrap(), &encodings, mkv::SCOPE_FRAMES).unwrap();
        assert_eq!(out, b"Dialogue: hello");
    }

    #[test]
    fn decode_skips_encodings_outside_the_scope() {
        let encodings = [ContentEncoding {
            order: 0,
            scope: mkv::SCOPE_FRAMES,
            encoding: Encoding::Zlib,
        }];
        let out = decode(b"raw".to_vec(), &encodings, mkv::SCOPE_CODEC_PRIVATE).unwrap();
        assert_eq!(out, b"raw");
    }

    #[test]
    fn decode_refuses_unsupported_encodings() {
        let encodings = [ContentEncoding {
            order: 0,
            scope: mkv::SCOPE_FRAMES,
            encoding: Encoding::Unsupported("encryption".into()),
        }];
        let err = decode(b"x".to_vec(), &encodings, mkv::SCOPE_FRAMES).unwrap_err();
        assert!(
            matches!(err, Error::Unsupported(ref w) if w == "encryption"),
            "{err}"
        );
    }

    #[test]
    fn block_body_parses_track_time_and_frame() {
        let p = parse_block(&[0x82, 0xFF, 0xFE, 0x80, b'h', b'i'], 0, Some(5)).unwrap();
        assert_eq!((p.track, p.relative, p.duration), (2, -2, Some(5)));
        assert_eq!(p.payload, b"hi");
    }

    #[test]
    fn laced_blocks_are_refused() {
        let err = parse_block(&[0x81, 0, 0, 0x02, 0], 7, None).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
    }

    #[test]
    fn a_huge_block_size_is_refused_without_reading_it() {
        // A SimpleBlock claiming 2^40 bytes, at the start of a 100-byte file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let mut bytes = vec![0xA3, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0];
        bytes.resize(100, 0);
        std::fs::write(&path, &bytes).unwrap();
        let r = Reader::open(&path).unwrap();
        let err = Window::default().element(&r, 0).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn block_group_takes_its_duration() {
        // BlockGroup { Block { track 1, +10 ticks, frame "x" }, BlockDuration 150 }
        let group = [
            0xA0, 0x8A, 0xA1, 0x85, 0x81, 0x00, 0x0A, 0x00, b'x', 0x9B, 0x81, 0x96,
        ];
        let p = parse_block_element(&group, 0).unwrap();
        assert_eq!((p.track, p.relative, p.duration), (1, 10, Some(150)));
        assert_eq!(p.payload, b"x");
        assert!(parse_block_element(&group[..11], 0).is_err(), "truncated");
    }
}

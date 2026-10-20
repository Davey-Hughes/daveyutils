//! Every block of the chosen tracks: through the Cues when they cover a track, by walking the
//! Clusters when they do not.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
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

#[derive(Debug, Clone)]
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

/// The blocks of each track numbered `numbers`, in one pass so a file whose tracks have no Cues is
/// walked once, not once per track. If that fails (one track's encoding unsupported, say), they are
/// read one at a time so the failure costs only that track. One result per number, in order.
pub fn read_each(r: &Reader, layout: &Layout, numbers: &[u64]) -> Vec<Result<TrackBlocks>> {
    // read_tracks gives a number asked for twice one entry: read each once, hand it out per ask.
    let mut distinct = numbers.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    match read_tracks(r, layout, &distinct, false) {
        Ok(all) => numbers
            .iter()
            .map(|&n| {
                Ok(all
                    .iter()
                    .find(|t| t.track == n)
                    .expect("read_tracks returns one entry per track asked for")
                    .clone())
            })
            .collect(),
        Err(_) => numbers
            .iter()
            .map(|&n| {
                read_tracks(r, layout, &[n], false).map(|mut one| {
                    one.pop()
                        .expect("read_tracks returns one entry per track asked for")
                })
            })
            .collect(),
    }
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
    if let Some(&frames) = layout.frame_counts.get(&track.uid)
        && (cues.len() as u64) < frames
    {
        return Ok(None);
    }
    // Where each cue leads: its Cluster, and its block at Cluster start + header (at most 12 bytes)
    // + relative position. One that leads outside the Segment is corrupt, and the Cues with it;
    // inside, nothing read from them can run past the end of the number.
    let mut placed = Vec::with_capacity(cues.len());
    for cue in cues {
        let relative = cue.relative.unwrap_or_default();
        let at = layout.cluster_at(cue);
        let block = at.and_then(|at| at.checked_add(relative));
        match (at, block) {
            (Some(at), Some(block)) if block < layout.segment_end => {
                placed.push((cue, at, relative, block));
            }
            _ => return Ok(None),
        }
    }
    // Every Cluster head and block the loop will read; starting the block's at Cluster start +
    // relative covers its header.
    r.prefetch(
        placed
            .iter()
            .flat_map(|&(_, at, _, block)| [(at, 64), (block, WINDOW as u64 + 12)]),
    );
    let mut window = Window::default();
    let mut clusters: HashMap<u64, (u64, i64)> = HashMap::new();
    let mut blocks = Vec::with_capacity(placed.len());
    for (cue, at, relative, _) in placed {
        let (data_start, cluster_ts) = if let Some(&c) = clusters.get(&at) {
            c
        } else {
            let Some(c) = cluster_timestamp(r, &mut window, at)? else {
                return Ok(None);
            };
            clusters.insert(at, c);
            c
        };
        // At or past the Segment's end once the Cluster's header is counted: corrupt too.
        let position = data_start + relative;
        if position >= layout.segment_end {
            return Ok(None);
        }
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
        // A time past the end of i64 is no time the cue could give.
        let Some(timestamp) = cluster_ts.checked_add(i64::from(parsed.relative)) else {
            return Ok(None);
        };
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
                // A time no i64 holds is no Cluster the cues can be trusted to lead to.
                let Ok(ts) = i64::try_from(ebml::uint(v)) else {
                    return Ok(None);
                };
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
                // One no i64 holds is an error for a block that needs it, and only then.
                cluster_ts = Some((pos, i64::try_from(ebml::uint(&v)).ok()));
            }
            mkv::SIMPLE_BLOCK | mkv::BLOCK_GROUP
                if tracks.contains(&block_track(r, &c, &head)?) =>
            {
                let Some((ts_at, ts)) = cluster_ts else {
                    return ebml::parse_err(pos, "block before its Cluster's Timestamp");
                };
                let Some(ts) = ts else {
                    return ebml::parse_err(ts_at, "Cluster timestamp out of range");
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
                let Some(timestamp) = ts.checked_add(i64::from(parsed.relative)) else {
                    return ebml::parse_err(pos, "block timestamp out of range");
                };
                out.entry(parsed.track).or_default().push(Block {
                    track: parsed.track,
                    position: pos,
                    timestamp,
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

/// The largest Cluster read whole by [`each_cluster`]: real ones are a few MB.
const MAX_CLUSTER: u64 = 512 << 20;

/// Each Cluster from the element at `pos` on, read whole, in file order: `f` gets its data's
/// offset and bytes, and says whether to go on.
fn each_cluster(
    r: &Reader,
    layout: &Layout,
    mut pos: u64,
    mut f: impl FnMut(u64, &[u8]) -> Result<bool>,
) -> Result<()> {
    while pos < layout.segment_end {
        let h = r.header_at(pos)?;
        let Some(end) = h.end() else {
            return Err(Error::Unsupported(format!(
                "unknown-size element {:#X} at byte {pos}",
                h.id
            )));
        };
        if h.id == mkv::CLUSTER {
            let size = end - h.data_start;
            if size > MAX_CLUSTER || end > r.file_size() {
                return ebml::parse_err(pos, format!("Cluster of {size} bytes"));
            }
            let data = r.read_exact(h.data_start, usize::try_from(size).unwrap_or(usize::MAX))?;
            if !f(h.data_start, &data)? {
                return Ok(());
            }
        }
        pos = end;
    }
    Ok(())
}

/// A block's body (a Block's or SimpleBlock's data), as a Cluster holds it.
struct Body<'a> {
    /// Its offset, for errors.
    at: u64,
    data: &'a [u8],
    /// The BlockGroup's BlockDuration, in TimestampScale units, if it has one.
    duration: Option<u64>,
}

/// The block a Cluster child holds, given as `(id, offset, data)` the way [`ebml::children`]
/// gives it; `None` for any other child, and for a BlockGroup without a Block.
fn block_body(id: u32, at: u64, child: &[u8]) -> Result<Option<Body<'_>>> {
    Ok(match id {
        mkv::SIMPLE_BLOCK => Some(Body {
            at,
            data: child,
            duration: None,
        }),
        mkv::BLOCK_GROUP => {
            let kids = ebml::children(child, at)?;
            let duration = kids
                .iter()
                .find(|(cid, _, _)| *cid == mkv::BLOCK_DURATION)
                .map(|(_, _, v)| ebml::uint(v));
            kids.into_iter()
                .find(|(cid, _, _)| *cid == mkv::BLOCK)
                .map(|(_, at, data)| Body { at, data, duration })
        }
        _ => None,
    })
}

/// Every frame of every track, in file order, as `(track number, frame)`: lacing and content
/// encodings undone. Reads the whole file, a Cluster at a time: for checking a remux frame by frame.
pub fn each_frame(r: &Reader, layout: &Layout, mut f: impl FnMut(u64, &[u8])) -> Result<()> {
    let encodings: HashMap<u64, &[ContentEncoding]> = layout
        .tracks
        .iter()
        .map(|t| (t.number, t.encodings.as_slice()))
        .collect();
    let Some(first) = layout.first_cluster else {
        return Ok(());
    };
    each_cluster(r, layout, first, |data_start, data| {
        for (id, at, child) in ebml::children(data, data_start)? {
            let Some(body) = block_body(id, at, child)? else {
                continue;
            };
            let (track, frames) = frames(body.data, body.at)?;
            let enc = encodings.get(&track).copied().unwrap_or_default();
            for frame in frames {
                f(track, &decode(frame.to_vec(), enc, mkv::SCOPE_FRAMES)?);
            }
        }
        Ok(true)
    })
}

/// A frame's time and what it holds, for telling whether a remux kept each frame where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timed {
    /// `None` for a frame laced behind its block's first with no duration to time it by.
    pub time_ns: Option<i128>,
    /// Of the frame, its content encodings undone.
    pub hash: u64,
}

/// The frames of the tracks numbered `tracks` whose times fall in `from_ns..to_ns`, by track
/// number, in file order. Reads the Clusters from the last Cue at or before `from_ns` (the first
/// Cluster, without one) to the first that starts past `to_ns`. A frame laced behind a block's
/// first is timed by the track's DefaultDuration, or the block's duration split evenly over its
/// frames; with neither, it has no time, and is given if its block's falls in the span.
pub fn frames_between(
    r: &Reader,
    layout: &Layout,
    from_ns: i128,
    to_ns: i128,
    tracks: &[u64],
) -> Result<HashMap<u64, Vec<Timed>>> {
    let scale = i128::from(layout.timestamp_scale);
    let wanted: HashMap<u64, &Track> = layout
        .tracks
        .iter()
        .filter(|t| tracks.contains(&t.number))
        .map(|t| (t.number, t))
        .collect();
    let mut out: HashMap<u64, Vec<Timed>> = HashMap::new();
    // The last cue at or before `from_ns` that leads inside the Segment: one past it is corrupt.
    let start = layout
        .cues
        .iter()
        .filter(|c| i128::from(c.time) * scale <= from_ns)
        .filter_map(|c| Some((c.time, layout.cluster_at(c)?)))
        .max()
        .map(|(_, at)| at)
        .or(layout.first_cluster);
    let Some(start) = start else {
        return Ok(out);
    };
    each_cluster(r, layout, start, |data_start, data| {
        let mut cluster_ts = None;
        for (id, at, child) in ebml::children(data, data_start)? {
            if id == mkv::TIMESTAMP {
                let ts = i128::from(ebml::uint(child)) * scale;
                if ts > to_ns {
                    return Ok(false);
                }
                cluster_ts = Some(ts);
                continue;
            }
            let Some(Body {
                at,
                data: body,
                duration,
            }) = block_body(id, at, child)?
            else {
                continue;
            };
            let Some(ts) = cluster_ts else {
                return ebml::parse_err(at, "block before its Cluster's Timestamp");
            };
            let (track, frames) = frames(body, at)?;
            let Some(t) = wanted.get(&track) else {
                continue;
            };
            // After the track number, the block's 16-bit time relative to its Cluster's.
            let Some(&[hi, lo]) = ebml::read_vint(body).and_then(|(_, n, _)| body.get(n..n + 2))
            else {
                return ebml::parse_err(at, "block too short");
            };
            let block_ns = ts + i128::from(i16::from_be_bytes([hi, lo])) * scale;
            let duration = duration.map(|d| i128::from(d) * scale);
            let times = lace_times(block_ns, frames.len(), t.default_duration, duration);
            for (frame, time_ns) in frames.into_iter().zip(times) {
                let when = time_ns.unwrap_or(block_ns);
                if when < from_ns || when >= to_ns {
                    continue;
                }
                let mut hasher = DefaultHasher::new();
                decode(frame.to_vec(), &t.encodings, mkv::SCOPE_FRAMES)?.hash(&mut hasher);
                out.entry(track).or_default().push(Timed {
                    time_ns,
                    hash: hasher.finish(),
                });
            }
        }
        Ok(true)
    })?;
    Ok(out)
}

/// The times of the `count` frames laced in a block at `block_ns`: one a frame duration after the
/// other, by the track's DefaultDuration or else the block's duration (ns) split evenly. Without
/// either, only the first frame's time is known. mkvmerge laces Vorbis eight to a block that way.
fn lace_times(
    block_ns: i128,
    count: usize,
    default_duration: Option<u64>,
    block_duration: Option<i128>,
) -> Vec<Option<i128>> {
    let n = i128::try_from(count).unwrap_or(i128::MAX).max(1);
    let step = default_duration
        .map(i128::from)
        .or(block_duration.map(|d| d / n));
    (0..n)
        .map(|i| match (i, step) {
            (0, _) => Some(block_ns),
            (_, Some(step)) => Some(block_ns + step * i),
            (_, None) => None,
        })
        .collect()
}

/// A Block or SimpleBlock body's track number and frames, its lacing (Xiph, EBML or fixed-size)
/// undone. `at` is the body's offset, for errors.
fn frames(body: &[u8], at: u64) -> Result<(u64, Vec<&[u8]>)> {
    let Some((track, n, _)) = ebml::read_vint(body) else {
        return ebml::parse_err(at, "bad block track number");
    };
    let Some((&flags, data)) = body.get(n + 2..).and_then(<[u8]>::split_first) else {
        return ebml::parse_err(at, "block too short");
    };
    let lacing = (flags >> 1) & 3;
    if lacing == 0 {
        return Ok((track, vec![data]));
    }
    let bad = || ebml::parse_err(at, "bad lacing");
    let Some((&last, mut rest)) = data.split_first() else {
        return bad();
    };
    let count = usize::from(last) + 1;
    let mut sizes: Vec<usize> = Vec::with_capacity(count);
    match lacing {
        // Xiph: each size but the last as a run of 255s and a final byte below 255.
        1 => {
            for _ in 1..count {
                let mut size = 0;
                loop {
                    let Some((&b, r)) = rest.split_first() else {
                        return bad();
                    };
                    rest = r;
                    size += usize::from(b);
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
        // Fixed: every frame the same size.
        2 => {
            if rest.len() % count != 0 {
                return bad();
            }
            sizes = vec![rest.len() / count; count - 1];
        }
        // EBML: the first size, then each next one as a signed difference from the one before.
        _ => {
            let Some((first, len, _)) = ebml::read_vint(rest) else {
                return bad();
            };
            rest = &rest[len..];
            sizes.push(usize::try_from(first).unwrap_or(usize::MAX));
            for _ in 2..count {
                let Some((raw, len, _)) = ebml::read_vint(rest) else {
                    return bad();
                };
                rest = &rest[len..];
                let bias = (1i64 << (7 * len - 1)) - 1;
                let prev = i64::try_from(*sizes.last().unwrap_or(&0)).unwrap_or(i64::MAX);
                let Ok(size) = usize::try_from(prev + i64::try_from(raw).unwrap_or(0) - bias)
                else {
                    return bad();
                };
                sizes.push(size);
            }
        }
    }
    let laced: usize = sizes.iter().sum();
    if laced > rest.len() {
        return bad();
    }
    sizes.push(rest.len() - laced);
    let mut out = Vec::with_capacity(count);
    for size in sizes {
        let (frame, r) = rest.split_at(size);
        out.push(frame);
        rest = r;
    }
    Ok((track, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A block body for track 1: timestamp 0, `flags`, then `data`.
    fn body(flags: u8, data: &[u8]) -> Vec<u8> {
        [&[0x81, 0, 0, flags][..], data].concat()
    }

    #[test]
    fn lacing_is_undone_in_all_three_kinds() {
        // Frames "ab", "c", "def".
        let xiph = body(0x02, &[2, 2, 1, b'a', b'b', b'c', b'd', b'e', b'f']);
        let fixed = body(0x04, &[2, b'a', b'b', b'c', b'd', b'e', b'f']);
        // EBML: first size 2 (0x82), then 1 - 2 = -1 as a 1-byte signed vint: 0x80 | (0x3F - 1).
        let ebml = body(
            0x06,
            &[2, 0x82, 0x80 | 0x3E, b'a', b'b', b'c', b'd', b'e', b'f'],
        );
        let none = body(0x80, b"abc");
        let got = |b: &[u8]| frames(b, 0).unwrap().1.concat_with_bars();
        assert_eq!(got(&xiph), "ab|c|def");
        assert_eq!(got(&fixed), "ab|cd|ef");
        assert_eq!(got(&ebml), "ab|c|def");
        assert_eq!(got(&none), "abc");
        assert!(
            frames(&body(0x02, &[2, 9, 9]), 0).is_err(),
            "sizes past the end"
        );
        assert!(
            frames(&body(0x04, &[1, b'a', b'b', b'c']), 0).is_err(),
            "3 bytes in 2"
        );
    }

    trait Bars {
        fn concat_with_bars(&self) -> String;
    }

    impl Bars for Vec<&[u8]> {
        fn concat_with_bars(&self) -> String {
            self.iter()
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .collect::<Vec<_>>()
                .join("|")
        }
    }

    #[test]
    fn every_frame_of_a_file_comes_out_once() {
        let (r, layout) = mkv::open(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/apply.mkv"
        )))
        .unwrap();
        let mut count: HashMap<u64, u64> = HashMap::new();
        each_frame(&r, &layout, |track, _| {
            *count.entry(track).or_default() += 1;
        })
        .unwrap();
        for t in &layout.tracks {
            assert_eq!(
                count.get(&t.number),
                layout.frame_counts.get(&t.uid),
                "track {}",
                t.id
            );
        }
    }

    #[test]
    fn laced_frames_are_timed_one_frame_duration_apart() {
        // 8 MP3 frames of 24 ms, as mkvmerge laces them.
        assert_eq!(
            lace_times(1_000, 3, Some(24_000_000), Some(1)),
            [Some(1_000), Some(24_001_000), Some(48_001_000)]
        );
        // No DefaultDuration: the block's duration split evenly.
        assert_eq!(
            lace_times(0, 4, None, Some(100)),
            [Some(0), Some(25), Some(50), Some(75)]
        );
        // Neither: only the first frame has a time.
        assert_eq!(lace_times(7, 3, None, None), [Some(7), None, None]);
        assert_eq!(lace_times(7, 1, None, None), [Some(7)]);
    }

    #[test]
    fn frames_between_gives_each_frame_in_the_span_with_its_time() {
        let (r, layout) = mkv::open(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/audio.mkv"
        )))
        .unwrap();
        let all = frames_between(&r, &layout, 0, i128::MAX, &[1, 2]).unwrap();
        for t in &layout.tracks[..2] {
            let frames = &all[&t.number];
            assert_eq!(
                Some(&(frames.len() as u64)),
                layout.frame_counts.get(&t.uid),
                "track {}",
                t.id
            );
            assert!(frames.windows(2).all(|w| w[0].time_ns < w[1].time_ns));
            assert!(frames.iter().all(|f| f.time_ns.is_some()));
        }
        // Track 2 is track 1 muxed 27 ms late: the same frames, each 27 ms on.
        assert_eq!(all[&1][5].hash, all[&2][5].hash);
        let (late, main) = (all[&2][5].time_ns.unwrap(), all[&1][5].time_ns.unwrap());
        assert!((late - main - 27_000_000).abs() < 1_000_000);
        // 10-11 s: every frame from 10 s up to 11 s, 128 ms apart, and none outside.
        let span = frames_between(&r, &layout, 10_000_000_000, 11_000_000_000, &[1]).unwrap();
        let times: Vec<i128> = span[&1].iter().filter_map(|f| f.time_ns).collect();
        assert_eq!(times.len(), 7, "{times:?}");
        assert!(
            (10_000_000_000..10_128_000_000).contains(&times[0])
                && (10_872_000_000..11_000_000_000).contains(&times[6]),
            "{times:?}"
        );
    }

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

    /// typos.mkv opened, with its one text track.
    fn typos() -> (Reader, Layout, Track) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/typos.mkv");
        let (r, layout) = mkv::open(std::path::Path::new(path)).unwrap();
        let track = layout.tracks[0].clone();
        (r, layout, track)
    }

    #[test]
    fn a_cue_whose_position_overflows_is_no_cue_to_trust() {
        let (r, layout, track) = typos();
        assert!(
            via_cues(&r, &layout, &track).unwrap().is_some(),
            "its Cues are good"
        );
        // Past the end of the number, just short of it, and past the Segment, for the Cluster and
        // for the block in it; and a block that would start in the Segment's last byte but for its
        // Cluster's header.
        let near = u64::MAX - layout.segment_data - 10;
        let corrupt: [fn(&mut Cue, u64, &Layout); 7] = [
            |c, _, _| c.cluster = u64::MAX,
            |c, near, _| c.cluster = near,
            |c, _, _| c.cluster = 1 << 40,
            |c, _, _| c.relative = Some(u64::MAX),
            |c, near, _| c.relative = Some(near),
            |c, _, _| c.relative = Some(1 << 40),
            |c, _, l| c.relative = Some(l.segment_end - 1 - l.segment_data - c.cluster),
        ];
        for (i, corrupt) in corrupt.iter().enumerate() {
            let mut bad = layout.clone();
            let cue = bad
                .cues
                .iter_mut()
                .find(|c| c.track == track.number)
                .unwrap();
            corrupt(cue, near, &layout);
            assert_eq!(via_cues(&r, &bad, &track).unwrap(), None, "case {i}");
        }
    }

    /// A file of one Cluster: Timestamp `i64::MAX`, then a SimpleBlock for track 1 at +1.
    fn cluster_at_the_end_of_time() -> (tempfile::TempDir, Reader) {
        one_cluster_file(i64::MAX.to_be_bytes(), 1)
    }

    /// A file of one Cluster: Timestamp `timestamp`, then a SimpleBlock for track 1 at `relative`.
    fn one_cluster_file(timestamp: [u8; 8], relative: i8) -> (tempfile::TempDir, Reader) {
        let [hi, lo] = i16::from(relative).to_be_bytes();
        let timestamp = [&[0xE7, 0x88][..], &timestamp].concat();
        let block = [0xA3, 0x85, 0x81, hi, lo, 0x80, b'x'];
        let data = [timestamp.as_slice(), &block].concat();
        let cluster = [
            &[
                0x1F,
                0x43,
                0xB6,
                0x75,
                0x80 | u8::try_from(data.len()).unwrap(),
            ][..],
            &data,
        ]
        .concat();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, cluster).unwrap();
        let r = Reader::open(&path).unwrap();
        (dir, r)
    }

    #[test]
    fn a_block_timestamp_past_the_end_of_time_is_an_error() {
        let (_dir, r) = cluster_at_the_end_of_time();
        let h = r.header_at(0).unwrap();
        let mut out = HashMap::new();
        let end = h.end().unwrap();
        let err = walk_cluster(&r, &h, end, false, &HashSet::from([1]), &mut out).unwrap_err();
        assert!(
            err.to_string().contains("block timestamp out of range"),
            "{err}"
        );
    }

    #[test]
    fn a_cue_to_a_block_timestamp_past_the_end_of_time_is_no_cue_to_trust() {
        let (_dir, r) = cluster_at_the_end_of_time();
        let (_, mut layout, mut track) = typos();
        track.number = 1;
        layout.segment_data = 0;
        layout.frame_counts.clear();
        layout.cues = vec![Cue {
            time: 0,
            track: 1,
            cluster: 0,
            relative: Some(10),
        }];
        assert_eq!(via_cues(&r, &layout, &track).unwrap(), None);
    }

    #[test]
    fn a_cluster_timestamp_past_i64_is_an_error_not_the_largest_time() {
        // 2^64 - 1 ticks: no i64 holds it, and a block at 0 would otherwise be read at i64::MAX.
        let (_dir, r) = one_cluster_file([0xFF; 8], 0);
        let h = r.header_at(0).unwrap();
        let mut out = HashMap::new();
        let end = h.end().unwrap();
        let err = walk_cluster(&r, &h, end, false, &HashSet::from([1]), &mut out).unwrap_err();
        assert!(
            err.to_string().contains("Cluster timestamp out of range"),
            "{err}"
        );
        assert_eq!(
            cluster_timestamp(&r, &mut Window::default(), 0).unwrap(),
            None
        );
        // A Cluster none of whose blocks is wanted needs no time: walking past it is fine.
        let mut out = HashMap::new();
        walk_cluster(&r, &h, end, false, &HashSet::from([2]), &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn frames_from_a_cue_past_the_segment_start_from_a_good_one() {
        let (r, layout, track) = typos();
        // From the last cue's time, so that cue is where the read starts.
        let last = layout.cues.iter().map(|c| c.time).max().unwrap();
        let from = i128::from(last) * i128::from(layout.timestamp_scale);
        let good = frames_between(&r, &layout, from, i128::MAX, &[track.number]).unwrap();
        assert!(!good.is_empty());
        // Only the last cue past the Segment: the read starts from the one before.
        let mut bad = layout.clone();
        let last = bad.cues.iter_mut().max_by_key(|c| c.time).unwrap();
        last.cluster = 1 << 40;
        assert_eq!(
            frames_between(&r, &bad, from, i128::MAX, &[track.number]).unwrap(),
            good
        );
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

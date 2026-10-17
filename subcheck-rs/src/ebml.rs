//! EBML primitives: element IDs and sizes, element headers, and positioned reads that count bytes.

use std::cell::Cell;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("parse error at byte {offset}: {msg}")]
    Parse { offset: u64, msg: String },
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn parse_err<T>(offset: u64, msg: impl Into<String>) -> Result<T> {
    Err(Error::Parse {
        offset,
        msg: msg.into(),
    })
}

/// An element header: where the element starts, its ID, and where its data lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub offset: u64,
    pub id: u32,
    pub data_start: u64,
    /// `None` is EBML's "unknown size".
    pub size: Option<u64>,
}

impl Header {
    /// One past the element's last byte, or `None` for an unknown size.
    pub fn end(&self) -> Option<u64> {
        self.size.map(|s| self.data_start + s)
    }
}

/// An element ID at the start of `buf`, with its marker bit kept (Matroska writes IDs that way),
/// and its length in bytes.
pub fn read_id(buf: &[u8]) -> Option<(u32, usize)> {
    let first = *buf.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || buf.len() < len {
        return None;
    }
    let id = buf[..len]
        .iter()
        .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
    Some((id, len))
}

/// A variable-length integer at the start of `buf`, marker bit stripped: its value, its length, and
/// whether every value bit is set (EBML's "unknown size" when the vint is an element size).
pub fn read_vint(buf: &[u8]) -> Option<(u64, usize, bool)> {
    let first = *buf.first()?;
    if first == 0 {
        return None;
    }
    let len_bits = first.leading_zeros() + 1;
    let len = len_bits as usize;
    if buf.len() < len {
        return None;
    }
    // The first byte keeps 8 - len value bits after the marker; none when len is 8.
    let mask = u8::MAX.checked_shr(len_bits).unwrap_or(0);
    let value = buf[1..len]
        .iter()
        .fold(u64::from(first & mask), |acc, &b| (acc << 8) | u64::from(b));
    let all_ones = value == (1u64 << (7 * len)) - 1;
    Some((value, len, all_ones))
}

/// An element header parsed from bytes that were read at `offset`.
pub fn parse_header(buf: &[u8], offset: u64) -> Result<Header> {
    let Some((id, id_len)) = read_id(buf) else {
        return parse_err(offset, "bad element ID");
    };
    let Some((size, size_len, unknown)) = read_vint(&buf[id_len..]) else {
        return parse_err(offset, "bad element size");
    };
    Ok(Header {
        offset,
        id,
        data_start: offset + (id_len + size_len) as u64,
        size: (!unknown).then_some(size),
    })
}

/// Unsigned integer element data: big-endian, 0 to 8 bytes.
pub fn uint(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
}

/// String element data, up to its first NUL (Matroska pads strings with them).
pub fn string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// The children of a master element held in memory, as `(id, data offset, data)`: the offset is
/// absolute, so a nested `children` call reports errors at the right byte. `base` is the absolute
/// offset of `data[0]`.
pub fn children(data: &[u8], base: u64) -> Result<Vec<(u32, u64, &[u8])>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let at = base + pos as u64;
        let h = parse_header(&data[pos..], at)?;
        let Some(size) = h.size else {
            return parse_err(at, "unknown-size element inside a master element");
        };
        let start = pos + usize::try_from(h.data_start - at).unwrap_or(usize::MAX);
        let end = usize::try_from(size)
            .ok()
            .and_then(|s| start.checked_add(s))
            .filter(|&e| e <= data.len());
        let Some(end) = end else {
            return parse_err(at, "element runs past the end of its parent");
        };
        out.push((h.id, h.data_start, &data[start..end]));
        pos = end;
    }
    Ok(out)
}

/// The largest master element read whole (Tracks, Cues, Tags). A two-hour film's Cues come to a
/// few hundred KB; anything near this is a corrupt size.
pub const MAX_IN_MEMORY: u64 = 64 << 20;

/// A file read with positioned reads only, counting the bytes it reads.
#[derive(Debug)]
pub struct Reader {
    file: File,
    size: u64,
    bytes_read: Cell<u64>,
    reads: Cell<u64>,
}

impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file,
            size,
            bytes_read: Cell::new(0),
            reads: Cell::new(0),
        })
    }

    pub fn file_size(&self) -> u64 {
        self.size
    }

    /// Ask the kernel to start fetching these `(offset, len)` ranges now, all at once.
    ///
    /// The reads that follow are small and scattered over a file that usually sits on a spinning
    /// disk behind NFS. Issued one at a time, each waits out its own seek (about 15 ms measured).
    /// Announced together, the NFS client sends them concurrently and the server's disk can serve
    /// them in one sweep. A hint only: failure, or a platform without it, just means slower reads.
    pub fn prefetch(&self, ranges: impl IntoIterator<Item = (u64, u64)>) {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let fd = self.file.as_raw_fd();
            for (offset, len) in ranges {
                let (Ok(offset), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else {
                    continue;
                };
                // SAFETY: posix_fadvise only reads its integer arguments; `fd` is open for as
                // long as `self` lives.
                unsafe {
                    libc::posix_fadvise(fd, offset, len, libc::POSIX_FADV_WILLNEED);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ranges.into_iter();
    }

    pub fn reads(&self) -> u64 {
        self.reads.get()
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.get()
    }

    /// Exactly `n` bytes at `offset`.
    pub fn read_exact(&self, offset: u64, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; n];
        self.file.read_exact_at(&mut buf, offset)?;
        self.bytes_read.set(self.bytes_read.get() + n as u64);
        self.reads.set(self.reads.get() + 1);
        Ok(buf)
    }

    /// Up to `n` bytes at `offset`: fewer only where the file ends.
    pub fn peek(&self, offset: u64, n: usize) -> Result<Vec<u8>> {
        let left = self.size.saturating_sub(offset);
        self.read_exact(offset, n.min(usize::try_from(left).unwrap_or(usize::MAX)))
    }

    /// The element header at `offset`.
    pub fn header_at(&self, offset: u64) -> Result<Header> {
        parse_header(&self.peek(offset, 12)?, offset)
    }

    /// An element's whole data. Only for elements small enough to hold in memory: never a Cluster.
    pub fn data(&self, h: &Header) -> Result<Vec<u8>> {
        let Some(size) = h.size else {
            return Err(Error::Unsupported(format!(
                "unknown-size element {:#X} at byte {}",
                h.id, h.offset
            )));
        };
        // A corrupt size inside a multi-GB file would otherwise be read in whole before failing.
        if size > MAX_IN_MEMORY {
            return parse_err(
                h.offset,
                format!("element of {size} bytes is too large to hold in memory"),
            );
        }
        if h.data_start + size > self.size {
            return parse_err(h.offset, "element runs past the end of the file");
        }
        let Ok(n) = usize::try_from(size) else {
            return parse_err(h.offset, "element too large to read");
        };
        self.read_exact(h.data_start, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_keep_their_marker_bit() {
        assert_eq!(read_id(&[0x1A, 0x45, 0xDF, 0xA3]), Some((0x1A45_DFA3, 4)));
        assert_eq!(read_id(&[0xA3, 0x00]), Some((0xA3, 1)));
        assert_eq!(read_id(&[0x42, 0x86]), Some((0x4286, 2)));
    }

    #[test]
    fn id_rejects_more_than_four_bytes_and_truncation() {
        assert_eq!(read_id(&[0x08, 0, 0, 0, 0]), None);
        assert_eq!(read_id(&[0x1A, 0x45]), None);
        assert_eq!(read_id(&[]), None);
    }

    #[test]
    fn vints_strip_the_marker() {
        assert_eq!(read_vint(&[0x81]), Some((1, 1, false)));
        assert_eq!(read_vint(&[0x40, 0x02]), Some((2, 2, false)));
        assert_eq!(read_vint(&[0x10, 0x00, 0x01, 0x00]), Some((256, 4, false)));
    }

    #[test]
    fn vint_all_ones_is_unknown() {
        assert_eq!(read_vint(&[0xFF]), Some((0x7F, 1, true)));
        assert_eq!(
            read_vint(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            Some(((1 << 56) - 1, 8, true))
        );
    }

    #[test]
    fn eight_byte_vint_has_no_value_bits_in_its_first_byte() {
        assert_eq!(
            read_vint(&[0x01, 0, 0, 0, 0, 0, 0x01, 0x00]),
            Some((256, 8, false))
        );
    }

    #[test]
    fn vint_rejects_zero_first_byte_and_truncation() {
        assert_eq!(read_vint(&[0x00, 0x81]), None);
        assert_eq!(read_vint(&[0x40]), None);
    }

    #[test]
    fn header_records_where_the_data_starts() {
        let h = parse_header(&[0x1F, 0x43, 0xB6, 0x75, 0x84, 0, 0, 0, 0], 100).unwrap();
        assert_eq!(
            h,
            Header {
                offset: 100,
                id: 0x1F43_B675,
                data_start: 105,
                size: Some(4)
            }
        );
        assert_eq!(h.end(), Some(109));
    }

    #[test]
    fn header_with_unknown_size() {
        let h = parse_header(&[0x18, 0x53, 0x80, 0x67, 0xFF], 0).unwrap();
        assert_eq!(h.size, None);
        assert_eq!(h.end(), None);
    }

    #[test]
    fn children_splits_a_master_element() {
        // TrackNumber 1, CodecID "S_TEXT/ASS"
        let mut data = vec![0xD7, 0x81, 0x01, 0x86, 0x8A];
        data.extend_from_slice(b"S_TEXT/ASS");
        let kids = children(&data, 0).unwrap();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0], (0xD7, 2, &[0x01][..]));
        assert_eq!((kids[1].0, kids[1].1), (0x86, 5));
        assert_eq!(string(kids[1].2), "S_TEXT/ASS");
    }

    #[test]
    fn children_rejects_a_child_running_past_its_parent() {
        let err = children(&[0xD7, 0x84, 0x01], 50).unwrap_err();
        assert!(matches!(err, Error::Parse { offset: 50, .. }), "{err}");
    }

    #[test]
    fn data_refuses_an_element_too_large_to_hold_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, [0u8; 64]).unwrap();
        let r = Reader::open(&path).unwrap();
        let h = Header {
            offset: 0,
            id: 0x1C53_BB6B,
            data_start: 8,
            size: Some(1 << 30),
        };
        let err = r.data(&h).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn uint_and_string() {
        assert_eq!(uint(&[]), 0);
        assert_eq!(uint(&[0x0F, 0x42, 0x40]), 1_000_000);
        assert_eq!(string(b"eng\0\0"), "eng");
    }
}

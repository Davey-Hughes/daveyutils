//! subcheck: read the text subtitles embedded in MKV files without reading the rest of the file.
//!
//! A subtitle track's packets are scattered through the whole file, so a demuxer that reads in
//! order (mkvextract, ffmpeg) reads every byte to collect them. The file's own index, the Cues, says
//! where each one is; `blocks` reads just those, and walks the Clusters only when the index does
//! not cover a track.

pub mod blocks;
pub mod ebml;
pub mod mkv;
pub mod subs;

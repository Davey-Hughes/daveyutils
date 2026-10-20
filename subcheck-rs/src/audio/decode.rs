//! A window of one audio track, decoded by ffmpeg: mono 32-bit float at [`RATE`], each sample
//! placed by its timestamp.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use super::correlate::RATE;
use crate::group;

/// Resampled to RATE (which adds no delay: 44.1 and 96 kHz copies line up within 0.01 ms) and
/// placed by timestamp, so a gap in a track (frames missing, as at the head of FMAB's TrueHD)
/// stays silence instead of closing up into a shift, and a track whose first frame there comes
/// late is padded at the front. min_hard_comp: Matroska stores times to the millisecond, so a
/// timestamp that strays by less is rounding, not a gap.
fn filter() -> String {
    format!(
        "aresample={RATE}:async=1:min_hard_comp=0.001:first_pts=0,aformat=sample_fmts=flt:channel_layouts=mono"
    )
}

/// `len_ns` of the file's `index`th audio track from `start_ns`. ffmpeg numbers a Matroska file's
/// audio streams (`0:a:N`) in track order, and its seek starts every track at `start_ns` by its
/// own timestamps.
pub fn window(
    ffmpeg: &OsStr,
    file: &Path,
    index: usize,
    start_ns: u64,
    len_ns: u64,
) -> Result<Vec<f32>, String> {
    let out = group::output(
        Command::new(ffmpeg)
            .args(["-nostdin", "-v", "error", "-ss"])
            .arg(seconds(start_ns))
            .arg("-i")
            .arg(file)
            .arg("-map")
            .arg(format!("0:a:{index}"))
            .arg("-t")
            .arg(seconds(len_ns))
            .arg("-af")
            .arg(filter())
            .args(["-f", "f32le", "-"]),
    )
    .map_err(|e| match e {
        group::Failed::Start(e) => starting(ffmpeg, &e),
        group::Failed::Wait(e) => format!("waiting for ffmpeg: {e}"),
    })?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let last = said.lines().rev().map(str::trim).find(|l| !l.is_empty());
        return Err(format!(
            "ffmpeg failed ({}): {}",
            out.status,
            last.unwrap_or("it said nothing")
        ));
    }
    Ok(out
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

/// Whether `ffmpeg` runs at all: asked once, before the first file.
pub fn check(ffmpeg: &OsStr) -> Result<(), String> {
    let ran = Command::new(ffmpeg)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match ran {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "{} -version failed ({s}): name another ffmpeg with --ffmpeg",
            ffmpeg.to_string_lossy()
        )),
        Err(e) => Err(starting(ffmpeg, &e)),
    }
}

fn starting(ffmpeg: &OsStr, e: &io::Error) -> String {
    let name = ffmpeg.to_string_lossy();
    if e.kind() == io::ErrorKind::NotFound {
        format!("{name} not found: install ffmpeg, or name it with --ffmpeg")
    } else {
        format!("starting {name}: {e}")
    }
}

/// Seconds, to the nanosecond, as ffmpeg reads a time.
fn seconds(ns: u64) -> String {
    format!("{}.{:09}", ns / 1_000_000_000, ns % 1_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_go_to_ffmpeg_to_the_nanosecond() {
        assert_eq!(seconds(0), "0.000000000");
        assert_eq!(seconds(288_000_000_001), "288.000000001");
    }

    #[test]
    fn a_missing_ffmpeg_is_named_with_the_way_out() {
        let err = check(OsStr::new("/nonexistent/ffmpeg")).unwrap_err();
        assert_eq!(
            err,
            "/nonexistent/ffmpeg not found: install ffmpeg, or name it with --ffmpeg"
        );
    }
}

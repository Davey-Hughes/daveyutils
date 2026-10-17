use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use subcheck::blocks::ReadPath;
use subcheck::mkv::{self, Track};
use subcheck::subs::{self, Kind};

#[derive(Parser)]
#[command(
    name = "subcheck",
    version,
    about = "Proofread the text subtitles embedded in MKV files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write one subtitle track out exactly as mkvextract would, reading only that track's packets.
    Extract {
        file: PathBuf,
        /// mkvmerge track ID, as `mkvmerge -J` prints it. May be left out when the file has a single
        /// text subtitle track.
        #[arg(long)]
        track: Option<usize>,
        /// Output path, or `-` for stdout. Default: <file stem>.track<ID>.<ass|ssa|srt> in the
        /// current directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite the output file if it exists.
        #[arg(short, long)]
        force: bool,
        /// Report the read path and the bytes read on stderr.
        #[arg(short, long)]
        verbose: bool,
        /// Ignore the Cues and walk every Cluster: for checking the fast path against the slow one.
        #[arg(long, hide = true)]
        walk: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("subcheck: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Extract {
            file,
            track,
            output,
            force,
            verbose,
            walk,
        } => extract(&file, track, output, force, verbose, walk),
    }
}

fn extract(
    file: &Path,
    track: Option<usize>,
    output: Option<PathBuf>,
    force: bool,
    verbose: bool,
    walk: bool,
) -> Result<()> {
    let (r, layout) = mkv::open(file).with_context(|| format!("reading {}", file.display()))?;
    let track = pick_track(&layout.tracks, track)?;

    // Built in memory, then written in one go: a failed extraction leaves no partial file behind.
    let mut buf = Vec::new();
    let done = subs::extract(&r, &layout, track, walk, &mut buf)
        .with_context(|| format!("extracting track {} of {}", track.id, file.display()))?;

    let output = output.unwrap_or_else(|| default_output(file, track.id, done.kind));
    if output == Path::new("-") {
        std::io::stdout().write_all(&buf)?;
    } else {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .create_new(!force)
            .open(&output)
            .with_context(|| format!("creating {} (use --force to overwrite)", output.display()))?;
        f.write_all(&buf)?;
    }
    if verbose {
        // Without the statistic, nothing says how many blocks the Cues should have covered.
        let unchecked =
            done.path == ReadPath::Cues && !layout.frame_counts.contains_key(&track.uid);
        eprintln!(
            "track {}: {} events via {:?}, {} bytes in {} reads, of {}{}",
            track.id,
            done.events,
            done.path,
            r.bytes_read(),
            r.reads(),
            r.file_size(),
            if unchecked {
                " (no NUMBER_OF_FRAMES tag to check the Cues against)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn pick_track(tracks: &[Track], id: Option<usize>) -> Result<&Track> {
    if let Some(id) = id {
        return tracks
            .iter()
            .find(|t| t.id == id)
            .with_context(|| format!("no track with ID {id}"));
    }
    let text: Vec<&Track> = tracks.iter().filter(|t| t.is_text_subtitle()).collect();
    match text.as_slice() {
        [only] => Ok(only),
        [] => bail!("no text subtitle tracks"),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|t| {
                    format!(
                        "  {}: {} {} {}",
                        t.id,
                        t.codec,
                        t.language,
                        t.name.as_deref().unwrap_or("")
                    )
                })
                .collect();
            bail!(
                "{} text subtitle tracks; pick one with --track:\n{}",
                many.len(),
                list.join("\n")
            )
        }
    }
}

fn default_output(file: &Path, id: usize, kind: Kind) -> PathBuf {
    let stem = file
        .file_stem()
        .unwrap_or(file.as_os_str())
        .to_string_lossy();
    PathBuf::from(format!("{stem}.track{id}.{}", kind.extension()))
}

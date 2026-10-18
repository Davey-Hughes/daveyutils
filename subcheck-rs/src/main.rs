use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use subcheck::blocks::{self, ReadPath};
use subcheck::ebml;
use subcheck::list::{self, Filter};
use subcheck::llm::Backend;
use subcheck::mkv::{self, Track};
use subcheck::review;
use subcheck::scan;
use subcheck::store::{self, State};
use subcheck::subs::{self, Kind};
use subcheck::text;
use subcheck::timing::{self, TrackReport};

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
    /// Proofread the text subtitles of MKV files with an LLM and record each suggested fix.
    ///
    /// Files are walked recursively; files unchanged since an earlier scan are skipped. Findings
    /// go to a run file under the state directory; read them with `subcheck list`.
    Scan {
        /// MKV files, or directories to search for them.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// claude, gpt (Codex) or ollama:<model>.
        #[arg(long, default_value = "claude")]
        llm: String,
        /// Any other LLM: a shell command that reads the prompt on stdin and prints the reply.
        #[arg(long, conflicts_with = "llm")]
        llm_cmd: Option<String>,
        /// Subtitle languages to check, as ISO 639-2 codes. Repeat for more (--lang eng --lang und).
        #[arg(long = "lang", default_value = "eng")]
        langs: Vec<String>,
        /// LLM calls to run at once.
        #[arg(short, long, default_value_t = 2)]
        jobs: usize,
        /// Lines per LLM call. Default 400, or 150 for ollama (its default context is 4k tokens).
        #[arg(long)]
        chunk_lines: Option<usize>,
        /// Check files even if they are unchanged since an earlier scan.
        #[arg(long)]
        force: bool,
        /// Print the prompts instead of sending them; records nothing.
        #[arg(long)]
        dry_run: bool,
        /// Show how each track was read.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Accept, reject or reword each finding of a run, with a key that opens it in mpv: the newest
    /// run, or the one named.
    Review {
        /// A run name (as `scan` prints it) or a run file. Default: the newest.
        run: Option<String>,
    },
    /// Report subtitle tracks shifted by whole frames against the video. Reads only the index
    /// and the subtitles; no LLM, nothing recorded.
    ///
    /// Counts the frames between each line's start or end and the nearest video keyframe, which
    /// is where most cuts are: a track whose timed lines pile up at +1 is a frame late. Directories
    /// are walked recursively.
    Timing {
        /// MKV files, or directories to search for them.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Only tracks in these languages, as ISO 639-2 codes; repeat for more. Default: every text
        /// subtitle track.
        #[arg(long = "lang")]
        langs: Vec<String>,
        /// Add each style's offset counts, and whether its times were used.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Show a run's findings as text: the newest run, or the one named.
    List {
        /// A run name (as `scan` prints it) or a run file. Default: the newest.
        run: Option<String>,
        /// Only findings you accepted.
        #[arg(long, conflicts_with_all = ["rejected", "pending"])]
        accepted: bool,
        /// Only findings you rejected.
        #[arg(long, conflicts_with = "pending")]
        rejected: bool,
        /// Only findings not yet decided.
        #[arg(long)]
        pending: bool,
        /// Add the mpv command that opens each finding's moment.
        #[arg(long)]
        mpv: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("subcheck: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Extract {
            file,
            track,
            output,
            force,
            verbose,
            walk,
        } => extract(&file, track, output, force, verbose, walk).map(|()| ExitCode::SUCCESS),
        Command::Scan {
            paths,
            llm,
            llm_cmd,
            langs,
            jobs,
            chunk_lines,
            force,
            dry_run,
            verbose,
        } => {
            let backend = match llm_cmd {
                Some(cmd) => Backend::shell(&cmd),
                None => Backend::preset(&llm).map_err(anyhow::Error::msg)?,
            };
            let opts = scan::Options {
                chunk_lines: chunk_lines.unwrap_or(backend.chunk_lines),
                backend,
                jobs,
                langs,
                force,
                verbose,
            };
            let state = store::state_dir()?;
            if dry_run {
                scan::dry_run(
                    &paths,
                    &opts,
                    &state,
                    &mut std::io::stdout().lock(),
                    &mut std::io::stderr(),
                )?;
                return Ok(ExitCode::SUCCESS);
            }
            let s = scan::run(&paths, &opts, &state, &mut std::io::stderr())?;
            if s.run.is_empty() {
                if s.files == 0 {
                    eprintln!("Nothing to check: no MKV files found");
                } else {
                    eprintln!(
                        "Nothing to check: {} unchanged since an earlier scan (--force to check again)",
                        scan::files(s.cached)
                    );
                }
                return Ok(ExitCode::SUCCESS);
            }
            eprintln!(
                "run {}: {} files, {} checked, {} failed, {} unchanged since an earlier scan; {} findings",
                s.run, s.files, s.checked, s.failed, s.cached, s.findings
            );
            eprintln!(
                "Review them with: subcheck review {} (or print them: subcheck list {0})",
                s.run
            );
            Ok(if s.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Review { run } => {
            let state = store::state_dir()?;
            let path = store::find_run(&state, run.as_deref())?;
            review::run(&path, &state)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Timing {
            paths,
            langs,
            verbose,
        } => Ok(timing(&paths, &langs, verbose)),
        Command::List {
            run,
            accepted,
            rejected,
            pending,
            mpv,
        } => {
            let path = store::find_run(&store::state_dir()?, run.as_deref())?;
            let run =
                store::read_run(&path).with_context(|| format!("reading {}", path.display()))?;
            let filter = match (accepted, rejected, pending) {
                (true, _, _) => Filter::Only(State::Accepted),
                (_, true, _) => Filter::Only(State::Rejected),
                (_, _, true) => Filter::Only(State::Pending),
                _ => Filter::All,
            };
            print!("{}", list::render(&run, filter, mpv));
            Ok(ExitCode::SUCCESS)
        }
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
    // Said first, so it is seen even when what is left of the file fails to extract.
    if let Some(t) = layout.truncated {
        eprintln!("{}", t.warning(file));
    }
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

/// A block per file on stdout. A path or file that cannot be read is reported on stderr, a track
/// that cannot be read in its row, and the rest are still checked; any of them makes the exit status
/// 1. A shifted or unsupported track is a result, not a failure.
fn timing(paths: &[PathBuf], langs: &[String], verbose: bool) -> ExitCode {
    let mut failed = false;
    let mut files = Vec::new();
    for p in paths {
        match scan::mkv_files(std::slice::from_ref(p)) {
            Ok((found, warnings)) => {
                for w in warnings {
                    eprintln!("warning: {w}");
                }
                let root = std::path::absolute(p).ok().filter(|r| r.is_dir());
                files.extend(found.into_iter().map(|f| {
                    let (short, long) = timing_labels(p, root.as_deref(), &f);
                    (f, short, long)
                }));
            }
            Err(e) => {
                eprintln!("subcheck: {e}");
                failed = true;
            }
        }
    }
    // By path; a file reached through two arguments keeps the same label whatever their order.
    files.sort();
    files.dedup_by(|a, b| a.0 == b.0);
    if files.is_empty() && !failed {
        eprintln!("Nothing to check: no MKV files found");
    }
    for (file, short, long) in &files {
        let alike = files.iter().filter(|(_, s, _)| s == short).count() > 1;
        let name = if alike { long } else { short };
        match timing_file(file, langs) {
            Ok((tracks, unread)) => {
                print!("{}", timing::render(name, &tracks, verbose));
                failed |= unread;
            }
            Err(e) => {
                eprintln!("subcheck: {}: {e:#}", file.display());
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// How a file's block is headed: its path below the directory argument it was found under (`root`),
/// so two seasons' `01.mkv` can be told apart, or its name when the argument named the file. Second,
/// its path as given, for when two files in the run would share that.
fn timing_labels(arg: &Path, root: Option<&Path>, file: &Path) -> (String, String) {
    let below = root.and_then(|r| file.strip_prefix(r).ok());
    let short = below.map_or_else(
        || PathBuf::from(file.file_name().unwrap_or(file.as_os_str())),
        Path::to_path_buf,
    );
    let long = below.map_or_else(|| arg.to_path_buf(), |b| arg.join(b));
    (
        short.to_string_lossy().into_owned(),
        long.to_string_lossy().into_owned(),
    )
}

/// Every chosen text track of one file against its video, and whether a track could not be read.
fn timing_file(file: &Path, langs: &[String]) -> Result<(Vec<TrackReport>, bool)> {
    let (r, layout) = mkv::open(file)?;
    if let Some(t) = layout.truncated {
        eprintln!("{}", t.warning(file));
    }
    let chosen: Vec<(&Track, Kind)> = layout
        .tracks
        .iter()
        .filter(|t| t.is_text_subtitle())
        .filter_map(|t| Some((t, Kind::of(t)?)))
        .filter(|(t, _)| langs.is_empty() || scan::wanted_language(t, langs))
        .collect();
    let name = |t: &Track| {
        t.name
            .clone()
            .unwrap_or_else(|| format!("({})", t.language))
    };
    let video = match timing::video(&layout) {
        Ok(v) => v,
        Err(reason) => {
            let tracks = chosen
                .iter()
                .map(|(t, _)| TrackReport {
                    id: t.id,
                    name: name(t),
                    result: Err(format!("unsupported: {reason}")),
                })
                .collect();
            return Ok((tracks, false));
        }
    };
    let numbers: Vec<u64> = chosen.iter().map(|(t, _)| t.number).collect();
    let reads = blocks::read_each(&r, &layout, &numbers);
    let mut unread = false;
    let tracks = chosen
        .into_iter()
        .zip(reads)
        .map(|((t, kind), read)| TrackReport {
            id: t.id,
            name: name(t),
            result: match read {
                Ok(b) => {
                    let events = text::events(
                        t.id,
                        kind,
                        &b.blocks,
                        layout.timestamp_scale,
                        t.default_duration,
                    );
                    Ok(timing::analyse(&video, &events))
                }
                // Already reads `unsupported: …`.
                Err(e @ ebml::Error::Unsupported(_)) => Err(e.to_string()),
                Err(e) => {
                    unread = true;
                    Err(format!("cannot read: {e}"))
                }
            },
        })
        .collect();
    Ok((tracks, unread))
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

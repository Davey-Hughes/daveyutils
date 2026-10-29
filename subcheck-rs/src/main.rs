use std::fs::OpenOptions;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use subcheck::apply::{self, Mode};
use subcheck::audio::{self, correlate, decode};
use subcheck::blocks::{self, ReadPath};
use subcheck::ebml;
use subcheck::group;
use subcheck::list::{self, Filter};
use subcheck::llm::Backend;
use subcheck::mkv::{self, Track};
use subcheck::review;
use subcheck::scan::{self, Planned};
use subcheck::store::{self, State};
use subcheck::subs::{self, Kind};
use subcheck::text;
use subcheck::timing::{self, Analysis, TrackReport};

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
        /// Add to this run (a name as `scan` prints it, or a run file) instead of starting one:
        /// only the checks it does not have yet are made, so running it again adds nothing.
        #[arg(long, conflicts_with_all = ["force", "dry_run"])]
        run: Option<String>,
        /// claude, gpt (Codex) or ollama:<model>. Default: claude, or with --run the run's.
        #[arg(long)]
        llm: Option<String>,
        /// Any other LLM: a shell command that reads the prompt on stdin and prints the reply.
        #[arg(long, conflicts_with = "llm")]
        llm_cmd: Option<String>,
        /// Subtitle languages to check, as ISO 639-2 codes. Repeat for more (--lang eng --lang und).
        /// Default: eng, or with --run the run's.
        #[arg(long = "lang")]
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
        /// Print the scan's log instead of opening review's screen on it, as when stdout or stderr
        /// is not a terminal.
        #[arg(long)]
        no_tui: bool,
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
    /// Report whether the audio tracks that hold the same sound line up. Decodes a few windows of
    /// each with ffmpeg; nothing recorded.
    ///
    /// Every pair of audio tracks is cross-correlated in ten-second windows spread over the file.
    /// A track whose sound matches a lower-numbered one's is reported against it: in sync, late or
    /// early by a steady amount (with the mkvmerge --sync that lines it up, where one does), or
    /// drifting.
    /// Directories are walked recursively.
    Audio {
        /// MKV files, or directories to search for them.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// The ffmpeg program.
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: PathBuf,
        /// Add every pair's lag and score in every window.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Make a run's accepted fixes in its MKV files: the newest run, or the one named.
    ///
    /// Each file is remuxed with mkvmerge beside the original, checked against it, and swapped
    /// in; the original is kept as <name>.mkv.orig.
    Apply(ApplyArgs),
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

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // command-line flags
struct ApplyArgs {
    /// A run name (as `scan` prints it) or a run file. Default: the newest.
    run: Option<String>,
    /// Show what would change, event by event; write nothing.
    #[arg(long, conflicts_with_all = ["no_backup", "full", "tracks_only", "check"])]
    dry_run: bool,
    /// Keep no copy of the original. The check before the swap is then the full one.
    #[arg(long, conflicts_with_all = ["tracks_only", "check"])]
    no_backup: bool,
    /// Check every frame of every track too, reading both files whole.
    #[arg(long, conflicts_with = "tracks_only")]
    full: bool,
    /// Write each file's fixed tracks to DIR and print the mkvmerge command that would remux it,
    /// for a remux done elsewhere. No MKV is touched.
    #[arg(long, value_name = "DIR", conflicts_with = "check")]
    tracks_only: Option<PathBuf>,
    /// Check remuxes built elsewhere (.<name>.mkv.subcheck beside each original) against their
    /// originals. Nothing is swapped.
    #[arg(long)]
    check: bool,
    /// Fix per-letter karaoke too: change the row's letters and re-space it by each letter's
    /// width, measured from the style's other rows. Look at each one afterwards (review, o).
    #[arg(long)]
    karaoke: bool,
    /// The mkvmerge program.
    #[arg(long, default_value = "mkvmerge", hide = true)]
    mkvmerge: PathBuf,
}

fn main() -> ExitCode {
    let ran = run(Cli::parse());
    group::finish();
    match ran {
        Ok(code) => code,
        Err(e) => {
            eprintln!("subcheck: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)] // one arm per subcommand
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
            run,
            llm,
            llm_cmd,
            langs,
            jobs,
            chunk_lines,
            force,
            dry_run,
            verbose,
            no_tui,
        } => {
            let state = store::state_dir()?;
            let into = run
                .as_deref()
                .map(|r| store::find_run(&state, Some(r)))
                .transpose()?;
            let (backend, langs) = if let Some(path) = &into {
                let run =
                    store::read_run(path).with_context(|| format!("reading {}", path.display()))?;
                // The cache says which languages a run from before runs kept them checked.
                let old = run.header.as_ref().is_some_and(|h| h.langs.is_empty());
                let cache = if old {
                    store::Cache::load(&state)?
                } else {
                    store::Cache::default()
                };
                scan::resume(&run, &cache, llm.as_deref(), llm_cmd.as_deref(), &langs)
                    .map_err(anyhow::Error::msg)?
            } else {
                let backend = match llm_cmd {
                    Some(cmd) => Backend::shell(&cmd),
                    None => Backend::preset(llm.as_deref().unwrap_or("claude"))
                        .map_err(anyhow::Error::msg)?,
                };
                let langs = if langs.is_empty() {
                    vec![scan::DEFAULT_LANG.to_string()]
                } else {
                    langs
                };
                (backend, langs)
            };
            let opts = scan::Options {
                chunk_lines: chunk_lines.unwrap_or(backend.chunk_lines),
                backend,
                jobs,
                langs,
                force,
                verbose,
                into: into.clone(),
            };
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
            let adding = into.is_some();
            let screen =
                !no_tui && std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
            // What planning warns of goes to stderr as it comes, and is kept for the screen, which
            // covers it once it opens.
            let mut log = review::Tee::new(std::io::stderr());
            let s = match scan::plan(&paths, &opts, &state, &mut log)? {
                Planned::Nothing(s) => s,
                Planned::Scan(plan) if screen => {
                    let warnings = log.into_lines();
                    let open = review::Open::Scan {
                        plan,
                        opts,
                        paths,
                        warnings,
                    };
                    return review::run(open, &state);
                }
                Planned::Scan(plan) => {
                    scan::execute(plan, &opts, &state, &mut std::io::stderr(), &|_| {})?
                }
            };
            for line in scan::summary_lines(&s, adding) {
                eprintln!("{line}");
            }
            Ok(if s.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Review { run } => {
            let state = store::state_dir()?;
            let path = store::find_run(&state, run.as_deref())?;
            review::run(review::Open::Run(path), &state)
        }
        Command::Timing {
            paths,
            langs,
            verbose,
        } => Ok(timing(&paths, &langs, verbose)),
        Command::Audio {
            paths,
            ffmpeg,
            verbose,
        } => Ok(audio(&paths, ffmpeg.as_os_str(), verbose)),
        Command::Apply(args) => apply_run(args),
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

/// `subcheck apply`: exit status 1 when a file failed (or, with --check, differs or is missing).
fn apply_run(a: ApplyArgs) -> Result<ExitCode> {
    let mode = match (a.dry_run, a.tracks_only, a.check) {
        (true, _, _) => Mode::DryRun,
        (_, Some(dir), _) => Mode::TracksOnly(dir),
        (_, _, true) => Mode::Check { full: a.full },
        _ => Mode::Apply {
            backup: !a.no_backup,
            full: a.full,
        },
    };
    let state = store::state_dir()?;
    let path = store::find_run(&state, a.run.as_deref())?;
    let s = apply::run(
        &path,
        &state,
        &mode,
        a.karaoke,
        a.mkvmerge.as_os_str(),
        &mut std::io::stdout().lock(),
    )?;
    Ok(if s.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
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
    let (files, mut failed) = labelled_files(paths);
    for (file, name) in &files {
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

/// As `timing`: a block per file on stdout, and exit status 1 when a path, a file or a window
/// cannot be read. A track out of line is a result, not a failure. A missing ffmpeg stops it
/// before the first file.
fn audio(paths: &[PathBuf], ffmpeg: &std::ffi::OsStr, verbose: bool) -> ExitCode {
    if let Err(e) = decode::check(ffmpeg) {
        eprintln!("subcheck: {e}");
        return ExitCode::FAILURE;
    }
    let (files, mut failed) = labelled_files(paths);
    for (file, name) in &files {
        match audio_file(file, ffmpeg) {
            Ok((tracks, windows, pairs)) => {
                print!(
                    "{}",
                    audio::render(name, &tracks, &windows, &pairs, verbose)
                );
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

/// The MKV files under `paths`, by path, each with the label its block is headed by, and whether
/// a path could not be read (said on stderr). Says so when there are none.
fn labelled_files(paths: &[PathBuf]) -> (Vec<(PathBuf, String)>, bool) {
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
    let labelled = files
        .iter()
        .map(|(file, short, long)| {
            let alike = files.iter().filter(|(_, s, _)| s == short).count() > 1;
            (file.clone(), if alike { long } else { short }.clone())
        })
        .collect();
    (labelled, failed)
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

/// A track's events and their analysis, or what to print in place of a verdict.
type Analysed<'a> = (&'a Track, Result<(Vec<text::Event>, Analysis), String>);

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
    let video = match timing::video(&layout) {
        Ok(v) => v,
        Err(reason) => {
            let tracks = chosen
                .iter()
                .map(|(t, _)| TrackReport {
                    id: t.id,
                    name: t.shown_name(),
                    result: Err(format!("unsupported: {reason}")),
                    lines: None,
                })
                .collect();
            return Ok((tracks, false));
        }
    };
    let numbers: Vec<u64> = chosen.iter().map(|(t, _)| t.number).collect();
    let reads = blocks::read_each(&r, &layout, &numbers);
    let mut unread = false;
    let read: Vec<Analysed> = chosen
        .into_iter()
        .zip(reads)
        .map(|((t, kind), read)| {
            let result = match read {
                Ok(b) => {
                    let events = text::events(
                        t.id,
                        kind,
                        &b.blocks,
                        layout.timestamp_scale,
                        t.default_duration,
                    );
                    let a = timing::analyse(&video, &events);
                    Ok((events, a))
                }
                // Already reads `unsupported: …`.
                Err(e @ ebml::Error::Unsupported(_)) => Err(e.to_string()),
                Err(e) => {
                    unread = true;
                    Err(format!("cannot read: {e}"))
                }
            };
            (t, result)
        })
        .collect();
    let analysed: Vec<(usize, &[text::Event], &Analysis)> = read
        .iter()
        .filter_map(|(t, r)| r.as_ref().ok().map(|(e, a)| (t.id, e.as_slice(), a)))
        .collect();
    let mut said = timing::shared::lines_of(&video, &analysed).into_iter();
    let tracks = read
        .iter()
        .map(|(t, result)| TrackReport {
            id: t.id,
            name: t.shown_name(),
            result: result
                .as_ref()
                .map(|(_, a)| a.clone())
                .map_err(Clone::clone),
            lines: result.is_ok().then(|| said.next().flatten()).flatten(),
        })
        .collect();
    Ok((tracks, unread))
}

/// One file's audio tracks, its windows' starts, and every pair's lag in each window.
fn audio_file(
    file: &Path,
    ffmpeg: &std::ffi::OsStr,
) -> Result<(Vec<audio::Track>, Vec<u64>, Vec<audio::Pair>)> {
    let (_, layout) = mkv::open(file)?;
    if let Some(t) = layout.truncated {
        eprintln!("{}", t.warning(file));
    }
    let tracks: Vec<audio::Track> = layout
        .tracks
        .iter()
        .filter(|t| t.kind == mkv::TRACK_TYPE_AUDIO)
        .map(audio::Track::of)
        .collect();
    if tracks.len() < 2 {
        return Ok((tracks, Vec::new(), Vec::new()));
    }
    let duration = layout
        .duration_ns
        .context("the file's Info has no Duration")?;
    let windows = audio::windows(duration);
    let mut pairs: Vec<audio::Pair> = Vec::new();
    for (i, a) in tracks.iter().enumerate() {
        for b in &tracks[i + 1..] {
            pairs.push(audio::Pair {
                a: a.id,
                b: b.id,
                lags: Vec::new(),
            });
        }
    }
    // One set of FFT plans: every window is as long as the first (ten seconds, or a short file's
    // only one).
    let Some(&(_, first_len)) = windows.first() else {
        return Ok((tracks, Vec::new(), pairs));
    };
    debug_assert!(windows.iter().all(|w| w.1 == first_len), "{windows:?}");
    let samples =
        usize::try_from(u128::from(first_len) * u128::from(correlate::RATE) / 1_000_000_000)
            .unwrap_or(usize::MAX);
    let c = correlate::Correlator::new(samples);
    for &(start, len) in &windows {
        // Every track at once: each its own ffmpeg, reading the same clusters.
        let decoded: Vec<Result<Vec<f32>, String>> = std::thread::scope(|s| {
            let running: Vec<_> = (0..tracks.len())
                .map(|index| s.spawn(move || decode::window(ffmpeg, file, index, start, len)))
                .collect();
            running
                .into_iter()
                .map(|r| r.join().expect("decoding does not panic"))
                .collect()
        });
        let decoded = decoded
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("decoding the window at {} s", start / 1_000_000_000))?;
        let prepared: Vec<correlate::Prepared> = decoded.iter().map(|d| c.prepare(d)).collect();
        let at = |id: usize| {
            tracks
                .iter()
                .position(|t| t.id == id)
                .expect("a pair of its tracks")
        };
        for p in &mut pairs {
            p.lags.push(c.lag(&prepared[at(p.a)], &prepared[at(p.b)]));
        }
    }
    Ok((tracks, windows.iter().map(|w| w.0).collect(), pairs))
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
                    let mut line = format!("  {}: {} {}", t.id, t.codec, t.shown_language());
                    if let Some(name) = &t.name {
                        line.push(' ');
                        line.push_str(name);
                    }
                    line
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

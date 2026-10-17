use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use subcheck::blocks::ReadPath;
use subcheck::list::{self, Filter};
use subcheck::llm::Backend;
use subcheck::mkv::{self, Track};
use subcheck::scan;
use subcheck::store::{self, State};
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
            eprintln!("Read them with: subcheck list {}", s.run);
            Ok(if s.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
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

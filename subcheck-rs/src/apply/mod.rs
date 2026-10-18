//! `subcheck apply`: make a run's accepted fixes in the MKV files. Per file: find each fix's
//! events, write the fixed tracks, remux with mkvmerge beside the original, check the remux, and
//! swap it in, keeping the original as `.orig` unless told not to.

pub mod karaoke;
pub mod patch;
pub mod remux;
pub mod verify;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::blocks::{self, Block};
use crate::group;
use crate::list::shell_quote;
use crate::mkv::{self, Layout, Track};
use crate::review::model::changed;
use crate::store::{self, Applied, Record, RunWriter};
use crate::subs::{self, Kind};
use crate::text::timestamp;
use patch::{Fix, Outcome};
use remux::Replacement;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Remux, check and swap in place.
    Apply { backup: bool, full: bool },
    /// Say what would change, and write nothing.
    DryRun,
    /// Write the fixed tracks to this directory and print the mkvmerge commands.
    TracksOnly(PathBuf),
    /// Check remuxes made elsewhere, found beside their originals.
    Check { full: bool },
}

/// What a run of apply came to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Fixes made (in the default mode) or to make (in the others).
    pub fixed: usize,
    pub already: usize,
    pub by_hand: usize,
    pub not_found: usize,
    pub remuxed: usize,
    pub failed: usize,
}

/// The remux of `file`, beside it: hidden, and with no video extension, so Plex leaves it alone.
pub fn remux_path(file: &Path) -> PathBuf {
    let mut name = OsStr::new(".").to_owned();
    name.push(file.file_name().unwrap_or_default());
    name.push(".subcheck");
    file.with_file_name(name)
}

/// Where the original is kept: `<name>.mkv.orig`.
pub fn backup_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".orig");
    PathBuf::from(name)
}

/// Apply the run in `run_path`'s accepted fixes, as `mode` says; `state` is the state directory.
/// `karaoke`: re-space per-letter karaoke rows rather than leave them for a hand fix.
pub fn run(
    run_path: &Path,
    state: &Path,
    mode: &Mode,
    karaoke: bool,
    mkvmerge: &OsStr,
    out: &mut impl Write,
) -> anyhow::Result<Summary> {
    let run = store::read_run(run_path)?;
    let accepted = patch::fixes(&run);
    let mut summary = Summary::default();
    if accepted.is_empty() {
        writeln!(out, "No accepted fixes in run {}.", run.name)?;
        return Ok(summary);
    }
    let mut writer = None;
    if matches!(mode, Mode::Apply { .. }) {
        remux::run(mkvmerge, &["--version".into()]).map_err(anyhow::Error::msg)?;
        writer = Some(RunWriter::open(run_path)?);
    }
    let mut paths: Vec<&str> = Vec::new();
    for f in &accepted {
        if !paths.contains(&f.finding.file.as_str()) {
            paths.push(&f.finding.file);
        }
    }
    let names = track_names(&paths);
    for (path, name) in paths.iter().zip(names) {
        let mine: Vec<Fix> = accepted
            .iter()
            .filter(|f| f.finding.file == *path)
            .cloned()
            .collect();
        writeln!(out, "{path}")?;
        let job = Job {
            path: Path::new(path),
            name,
            fixes: &mine,
            mode,
            karaoke,
            state,
            mkvmerge,
        };
        match job.run(out) {
            Ok(done) => {
                for (_, o) in &done.outcomes {
                    match o {
                        Outcome::Patched | Outcome::Respaced { .. } => summary.fixed += 1,
                        Outcome::AlreadyApplied => summary.already += 1,
                        Outcome::ByHand(_) => summary.by_hand += 1,
                        Outcome::NotFound => summary.not_found += 1,
                    }
                }
                summary.remuxed += usize::from(done.remuxed);
                if let Some(w) = &mut writer {
                    let at = jiff::Timestamp::now();
                    let records: Vec<Record> = done
                        .outcomes
                        .iter()
                        .filter(|(_, o)| {
                            matches!(
                                o,
                                Outcome::AlreadyApplied
                                    | Outcome::Patched
                                    | Outcome::Respaced { .. }
                            )
                        })
                        .map(|&(finding, _)| Record::Applied(Applied { finding, at }))
                        .collect();
                    w.append(&records)?;
                }
            }
            Err(e) => {
                writeln!(out, "  failed: {e}")?;
                summary.failed += 1;
            }
        }
        writeln!(out)?;
    }
    let s = summary;
    let verb = match mode {
        Mode::Apply { .. } => "applied",
        _ => "to apply",
    };
    writeln!(
        out,
        "{verb} {}, already applied {}, by hand {}, not found {}; {} remuxed, {} failed",
        s.fixed, s.already, s.by_hand, s.not_found, s.remuxed, s.failed
    )?;
    // The check plans the fixes again, and must plan them as these tracks were. A run outside the
    // state directory is found by its path, not its name.
    if matches!(mode, Mode::TracksOnly(_)) && s.fixed > 0 {
        let flag = if karaoke { " --karaoke" } else { "" };
        let in_state = run_path.parent() == Some(state.join("runs").as_path());
        let shown = if in_state {
            run.name.clone()
        } else {
            shell_quote(&run_path.to_string_lossy())
        };
        writeln!(
            out,
            "Once they are remuxed, check them with: subcheck apply {shown} --check{flag}"
        )?;
    }
    Ok(summary)
}

/// What each file's fixed tracks are named for: its stem, unless another file in the run has the
/// same one (their tracks would overwrite each other's); then its stem and a number that no other
/// file's name is.
fn track_names(paths: &[&str]) -> Vec<String> {
    let stems: Vec<String> = paths
        .iter()
        .map(|p| {
            Path::new(p)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let mut used = std::collections::HashSet::new();
    let mut out = Vec::new();
    for stem in &stems {
        let shared = stems.iter().filter(|s| *s == stem).count() > 1;
        let mut name = stem.clone();
        if shared || used.contains(&name) {
            // At most 2n names are taken (the names given and the stems), so one of the first
            // 2n + 1 numbers is free.
            name = (1..=2 * stems.len() + 1)
                .map(|n| format!("{stem}.{n}"))
                .find(|n| !used.contains(n) && !stems.contains(n))
                .expect("one of 2n + 1 names is free");
        }
        used.insert(name.clone());
        out.push(name);
    }
    out
}

/// One file's fixes.
struct Job<'a> {
    path: &'a Path,
    /// What its fixed tracks are named for: `<name>.track<ID>.<ext>`.
    name: String,
    fixes: &'a [Fix<'a>],
    mode: &'a Mode,
    karaoke: bool,
    state: &'a Path,
    mkvmerge: &'a OsStr,
}

/// A file done: each fix's outcome, and whether it was remuxed.
struct Done {
    outcomes: Vec<(u64, Outcome)>,
    remuxed: bool,
}

/// The original file, and the blocks read of its tracks with fixes, by track ID.
struct Original<'a> {
    r: &'a crate::ebml::Reader,
    layout: &'a Layout,
    read: &'a BTreeMap<usize, &'a [Block]>,
}

/// A track with fixes: the original's header, and its blocks as patched.
struct Fixed<'a> {
    track: &'a Track,
    kind: Kind,
    patched: Vec<Block>,
}

impl Job<'_> {
    /// Plan, report, then act as the mode says. An error leaves the file as it was.
    fn run(&self, out: &mut impl Write) -> Result<Done, String> {
        let (r, layout) = mkv::open(self.path).map_err(|e| e.to_string())?;
        if let Some(t) = layout.truncated {
            return Err(format!("{t}: replace this copy first"));
        }
        let ids: BTreeSet<usize> = self
            .fixes
            .iter()
            .flat_map(|f| f.finding.occurrences.iter().map(|o| o.track))
            .collect();
        let chosen: Vec<&Track> = layout
            .tracks
            .iter()
            .filter(|t| ids.contains(&t.id) && t.is_text_subtitle())
            .collect();
        // Read once, for the plan and the check alike: the tracks the fixes are on. Where the check
        // will run and these must be walked anyway (no statistics), every other text track comes
        // in the same walk; otherwise the check reads them itself, and only if it runs.
        let checks = matches!(self.mode, Mode::Apply { .. } | Mode::Check { .. });
        let wanted: Vec<&Track> = if checks && verify::must_walk(&layout, &chosen) {
            layout
                .tracks
                .iter()
                .filter(|t| t.is_text_subtitle())
                .collect()
        } else {
            chosen.clone()
        };
        let numbers: Vec<u64> = wanted.iter().map(|t| t.number).collect();
        let walk = verify::must_walk(&layout, &wanted);
        let read = blocks::read_tracks(&r, &layout, &numbers, walk).map_err(|e| e.to_string())?;
        let read: BTreeMap<usize, &[Block]> = wanted
            .iter()
            .zip(&read)
            .map(|(t, b)| (t.id, b.blocks.as_slice()))
            .collect();
        let headers: Vec<Vec<u8>> = chosen
            .iter()
            .map(|t| {
                blocks::decode(
                    t.codec_private.clone(),
                    &t.encodings,
                    mkv::SCOPE_CODEC_PRIVATE,
                )
            })
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let views: Vec<patch::Track> = chosen
            .iter()
            .zip(&headers)
            .map(|(t, header)| patch::Track {
                id: t.id,
                kind: Kind::of(t).expect("a text subtitle track"),
                blocks: read[&t.id],
                header,
                scale: layout.timestamp_scale,
                default_duration: t.default_duration,
            })
            .collect();
        let plan = patch::plan(&views, self.fixes, self.karaoke);
        self.report(&plan, &views, out).map_err(|e| e.to_string())?;
        note_tags(&r, &layout, &chosen, &plan, out)?;
        let fixed: Vec<Fixed> = chosen
            .iter()
            .filter_map(|t| {
                let edits = plan.edits.get(&t.id)?;
                Some(Fixed {
                    track: t,
                    kind: Kind::of(t).expect("a text subtitle track"),
                    patched: patch::edited(read[&t.id], edits),
                })
            })
            .collect();
        let original = Original {
            r: &r,
            layout: &layout,
            read: &read,
        };
        let remuxed = if fixed.is_empty() {
            false
        } else {
            self.act(&original, &fixed, out)?
        };
        Ok(Done {
            outcomes: plan.outcomes,
            remuxed,
        })
    }

    /// What the mode does with a file that has fixes to make; whether it was remuxed.
    fn act(&self, o: &Original, fixed: &[Fixed], out: &mut impl Write) -> Result<bool, String> {
        match self.mode {
            Mode::DryRun => Ok(false),
            Mode::TracksOnly(dir) => {
                let files = self.write_tracks(dir, o, fixed)?;
                for f in &files {
                    for written in std::iter::once(&f.file).chain(&f.tags) {
                        say(out, &format!("wrote {}", written.display()))?;
                    }
                }
                let args = remux::args(self.path, o.layout, &files, &remux_path(self.path));
                let line: Vec<String> = args
                    .iter()
                    .map(|a| shell_quote(&a.to_string_lossy()))
                    .collect();
                say(out, &format!("mkvmerge {}", line.join(" ")))?;
                Ok(false)
            }
            Mode::Check { full } => {
                let remuxed = remux_path(self.path);
                if !remuxed.exists() {
                    return Err(format!("not built yet: {}", remuxed.display()));
                }
                let diffs = verify(o, &remuxed, fixed, *full)?;
                if !diffs.is_empty() {
                    return Err(differs(&remuxed, &diffs));
                }
                say(
                    out,
                    &format!(
                        "{} checked ({}): OK",
                        remuxed.display(),
                        quick_or_full(*full)
                    ),
                )?;
                Ok(false)
            }
            Mode::Apply { backup, full } => {
                let full = *full || !backup;
                let kept = self.apply(o, fixed, *backup, full)?;
                say(
                    out,
                    &format!(
                        "remuxed, verified ({}), swapped; {}",
                        quick_or_full(full),
                        kept.map_or("no backup kept".into(), |b| format!(
                            "backup: {}",
                            b.display()
                        ))
                    ),
                )?;
                Ok(true)
            }
        }
    }

    /// One line per fix: when, what changes, and what came of it. A dry run adds each patched
    /// event's text before and after.
    fn report(
        &self,
        plan: &patch::Plan,
        views: &[patch::Track],
        out: &mut impl Write,
    ) -> io::Result<()> {
        for (fix, (_, outcome)) in self.fixes.iter().zip(&plan.outcomes) {
            let f = fix.finding;
            let when = f.occurrences.first().map_or(0, |o| o.start_ms);
            let (a, b) = changed(&f.original, &fix.text);
            let what = format!("{} → {}", &f.original[a], &fix.text[b]);
            let status = match outcome {
                Outcome::Patched => "ok".to_string(),
                Outcome::Respaced { px, guessed } => respaced(*px, guessed),
                Outcome::AlreadyApplied => "already applied".into(),
                Outcome::ByHand(why) => format!("by hand: {why}"),
                Outcome::NotFound => "not found: the file changed since the scan".into(),
            };
            writeln!(out, "  {}  {what}  {status}", timestamp(when))?;
        }
        if *self.mode == Mode::DryRun {
            for (id, edits) in &plan.edits {
                let view = views
                    .iter()
                    .find(|v| v.id == *id)
                    .expect("an edited track was read");
                let rows: Vec<&patch::KaraokeRow> =
                    plan.rows.iter().filter(|r| r.track == *id).collect();
                // A karaoke row's events are shown as the row, below.
                let in_row = |i: &usize| rows.iter().any(|r| r.blocks.contains(i));
                for (i, new) in edits.changed.iter().filter(|(i, _)| !in_row(i)) {
                    let old = subs::c_string(&view.blocks[*i].payload);
                    writeln!(out, "    track {id}: {}", text_of(view.kind, &old))?;
                    writeln!(
                        out,
                        "         now: {}",
                        text_of(view.kind, &subs::c_string(new))
                    )?;
                }
                for r in rows {
                    writeln!(out, "    track {id}: {}", r.was)?;
                    writeln!(
                        out,
                        "         now: {} (karaoke letters: {})",
                        r.now,
                        letter_counts(r)
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Each fixed track written to `dir`, named as `extract` names it, with its own tags beside it
    /// as `<name>.track<ID>.tags.xml` when it has any. A failure removes what it wrote, a file cut
    /// short by it too.
    fn write_tracks(
        &self,
        dir: &Path,
        o: &Original,
        fixed: &[Fixed],
    ) -> Result<Vec<Replacement>, String> {
        let mut written = Vec::new();
        let files = self.write_each(dir, o, fixed, &mut written);
        if files.is_err() {
            for path in &written {
                let _ = fs::remove_file(path);
            }
        }
        files
    }

    /// The work of [`Job::write_tracks`], each file added to `written` before it is written.
    fn write_each(
        &self,
        dir: &Path,
        o: &Original,
        fixed: &[Fixed],
        written: &mut Vec<PathBuf>,
    ) -> Result<Vec<Replacement>, String> {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let tags = tags_data(o.r, o.layout)?;
        // A file is ours to remove once it is opened for writing (a write cut short leaves part of
        // it); one that could not even be opened is someone else's.
        let mut write = |path: PathBuf, bytes: &[u8]| {
            let fail = |e: io::Error| format!("writing {}: {e}", path.display());
            let mut file = fs::File::create(&path).map_err(fail)?;
            written.push(path.clone());
            file.write_all(bytes).map_err(fail)?;
            Ok::<PathBuf, String>(path)
        };
        let mut files = Vec::new();
        for f in fixed {
            let mut buf = Vec::new();
            subs::write_track(
                f.track,
                f.kind,
                &f.patched,
                o.layout.timestamp_scale,
                &mut buf,
            )
            .map_err(|e| e.to_string())?;
            let ext = f.kind.extension();
            let file = write(
                dir.join(format!("{}.track{}.{ext}", self.name, f.track.id)),
                &buf,
            )?;
            let xml = match &tags {
                Some((data, base)) => {
                    remux::track_tags_xml(data, *base, f.track.uid)
                        .map_err(|e| format!("reading the tags: {e}"))?
                        .0
                }
                None => None,
            };
            let tags = match xml {
                Some(xml) => Some(write(
                    dir.join(format!("{}.track{}.tags.xml", self.name, f.track.id)),
                    xml.as_bytes(),
                )?),
                None => None,
            };
            files.push(Replacement {
                id: f.track.id,
                file,
                tags,
            });
        }
        Ok(files)
    }

    /// Remux beside the original, check it, and swap it in. The backup's path when one is kept.
    fn apply(
        &self,
        o: &Original,
        fixed: &[Fixed],
        backup: bool,
        full: bool,
    ) -> Result<Option<PathBuf>, String> {
        let kept = backup_path(self.path);
        if backup && kept.exists() {
            return Err(format!(
                "{} exists: an earlier backup is never overwritten",
                kept.display()
            ));
        }
        let size = fs::metadata(self.path).map_err(|e| e.to_string())?.len();
        let dir = self.path.parent().unwrap_or(Path::new("."));
        if let Some(free) = free_space(dir)
            && free < size
        {
            return Err(format!(
                "not enough space in {}: the remux needs {size} bytes, {free} are free",
                dir.display()
            ));
        }
        let remuxed = remux_path(self.path);
        // Created here first: an unwritable directory says so before any work.
        if let Err(e) = fs::File::create(&remuxed) {
            return Err(format!(
                "cannot write in {}: {e} (use --tracks-only)",
                dir.display()
            ));
        }
        // And the original can be linked as the backup: a refusal (a filesystem without links,
        // or an original another user owns) says so now, not after the remux. The probe's name
        // is ours: one left by an interrupted run is removed first.
        if backup {
            let mut probe = remuxed.as_os_str().to_owned();
            probe.push(".link");
            let probe = PathBuf::from(probe);
            let _ = fs::remove_file(&probe);
            let removing = group::remove_on_signal(&probe);
            let linked = fs::hard_link(self.path, &probe);
            let _ = fs::remove_file(&probe);
            drop(removing);
            if let Err(e) = linked {
                let _ = fs::remove_file(&remuxed);
                return Err(format!(
                    "cannot keep the original as a backup: {e} (--no-backup swaps without one)"
                ));
            }
        }
        // Held through the swap too: once the remux is renamed into place, removing it by its old
        // name finds nothing.
        let _removing = group::remove_on_signal(&remuxed);
        let result = self
            .remux_and_check(o, fixed, &remuxed, full)
            .and_then(|()| swap(self.path, &remuxed, backup.then_some(kept.as_path())));
        if result.is_err() {
            let _ = fs::remove_file(&remuxed);
        }
        result.map(|()| backup.then_some(kept))
    }

    fn remux_and_check(
        &self,
        o: &Original,
        fixed: &[Fixed],
        remuxed: &Path,
        full: bool,
    ) -> Result<(), String> {
        let tracks = self.state.join("apply");
        let files = self.write_tracks(&tracks, o, fixed)?;
        let args = remux::args(self.path, o.layout, &files, remuxed);
        let ran = remux::run(self.mkvmerge, &args);
        for f in &files {
            for written in std::iter::once(&f.file).chain(&f.tags) {
                let _ = fs::remove_file(written);
            }
        }
        if let Some(warnings) = ran? {
            eprintln!(
                "subcheck: mkvmerge warned about {}: {warnings}",
                self.path.display()
            );
        }
        let diffs = verify(o, remuxed, fixed, full)?;
        if !diffs.is_empty() {
            return Err(differs(remuxed, &diffs));
        }
        Ok(())
    }
}

/// Put `remuxed` in `file`'s place with `file`'s permissions, keeping the original at `kept` when
/// there is one. The original is linked there first and the remux then renamed over it, so `file`
/// is never missing: a backup needs a filesystem with hard links. One that is there already is never
/// replaced. A failure leaves `file` as it was.
fn swap(file: &Path, remuxed: &Path, kept: Option<&Path>) -> Result<(), String> {
    let perms = fs::metadata(file).map_err(|e| e.to_string())?.permissions();
    fs::set_permissions(remuxed, perms).map_err(|e| e.to_string())?;
    // Between the link and the rename, a signal removes the link too: left behind, it would
    // stop every later run as an earlier backup.
    let mut unlinking = None;
    if let Some(kept) = kept {
        fs::hard_link(file, kept).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                format!(
                    "{} exists: an earlier backup is never overwritten",
                    kept.display()
                )
            } else {
                format!(
                    "keeping the original as {}: {e} (--no-backup swaps without one)",
                    kept.display()
                )
            }
        })?;
        unlinking = Some(group::remove_on_signal(kept));
    }
    let renamed = fs::rename(remuxed, file);
    drop(unlinking);
    renamed.map_err(|e| {
        if let Some(kept) = kept {
            let _ = fs::remove_file(kept);
        }
        format!("swapping in the remux: {e}")
    })
}

/// `remuxed` against the original, with `fixed` in place of its tracks.
fn verify(
    o: &Original,
    remuxed: &Path,
    fixed: &[Fixed],
    full: bool,
) -> Result<Vec<String>, String> {
    let (r2, layout2) =
        mkv::open(remuxed).map_err(|e| format!("reading {}: {e}", remuxed.display()))?;
    let patched: BTreeMap<usize, &[Block]> = fixed
        .iter()
        .map(|f| (f.track.id, f.patched.as_slice()))
        .collect();
    verify::compare(
        verify::File {
            r: o.r,
            layout: o.layout,
        },
        verify::File {
            r: &r2,
            layout: &layout2,
        },
        &patched,
        o.read,
        full,
    )
    .map_err(|e| e.to_string())
}

/// In every mode, the dry run included: what of each fixed track's own tags may not carry over
/// exactly. Tags that cannot be read are said here too, not made the file's failure: a dry run or
/// a check has no use for them (a remux fails on them when it writes the tracks).
fn note_tags(
    r: &crate::ebml::Reader,
    layout: &Layout,
    chosen: &[&Track],
    plan: &patch::Plan,
    out: &mut impl Write,
) -> Result<(), String> {
    let data = match tags_data(r, layout) {
        Ok(Some(data)) => data,
        Ok(None) => return Ok(()),
        Err(e) => return say(out, &format!("note: the file's tags cannot be read: {e}")),
    };
    let (data, base) = data;
    for t in chosen.iter().filter(|t| plan.edits.contains_key(&t.id)) {
        let odd = match remux::track_tags_xml(&data, base, t.uid) {
            Ok((_, odd)) => odd,
            Err(e) => return say(out, &format!("note: the file's tags cannot be read: {e}")),
        };
        if !odd.is_empty() {
            let odd: Vec<&str> = odd.into_iter().collect();
            say(
                out,
                &format!(
                    "note: track {}'s own tags may not carry over exactly: {}",
                    t.id,
                    odd.join("; ")
                ),
            )?;
        }
    }
    Ok(())
}

/// The data and offset of the file's Tags element, the one its index leads to (mkvmerge writes one).
fn tags_data(r: &crate::ebml::Reader, layout: &Layout) -> Result<Option<(Vec<u8>, u64)>, String> {
    let Some(at) = layout.tags_at else {
        return Ok(None);
    };
    let h = r.header_at(at).map_err(|e| e.to_string())?;
    Ok(Some((r.data(&h).map_err(|e| e.to_string())?, h.data_start)))
}

/// One indented line of a file's report.
fn say(out: &mut impl Write, line: &str) -> Result<(), String> {
    writeln!(out, "  {line}").map_err(|e| e.to_string())
}

/// What a re-spaced karaoke row's letters did, the counts that are not zero.
fn letter_counts(r: &patch::KaraokeRow) -> String {
    let counts = [
        (r.changed, "changed"),
        (r.added, "added"),
        (r.removed, "removed"),
        (r.moved, "moved"),
    ];
    let said: Vec<String> = counts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, done)| format!("{n} {done}"))
        .collect();
    said.join(", ")
}

/// A karaoke fix's status: how much its rows grew, and which widths were guesses.
fn respaced(px: i64, guessed: &str) -> String {
    let guess = if guessed.is_empty() {
        String::new()
    } else {
        let quoted: Vec<String> = guessed.chars().map(|c| format!("\"{c}\"")).collect();
        format!(" (no {} in this style to measure)", quoted.join(", "))
    };
    format!("karaoke re-spaced by {px:+} px{guess}: check it (review, o)")
}

fn quick_or_full(full: bool) -> &'static str {
    if full { "full" } else { "quick" }
}

fn differs(remuxed: &Path, diffs: &[String]) -> String {
    format!(
        "{} differs from the original: {}",
        remuxed.display(),
        diffs.join("; ")
    )
}

/// An event's text: its Text field for ASS and SSA.
fn text_of(kind: Kind, payload: &str) -> &str {
    crate::text::text_field(kind, payload).map_or(payload, |at| &payload[at..])
}

/// Bytes free to an ordinary user in the filesystem holding `dir`; `None` when it cannot be told.
#[allow(clippy::useless_conversion)] // the counts are u32 on macOS
fn free_space(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes only to `s`, from a NUL-terminated path.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &raw mut s) != 0 {
            return None;
        }
        Some(u64::from(s.f_bavail) * u64::from(s.f_frsize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_karaoke_rows_counts_leave_out_the_zeros() {
        let row = |changed, added, removed, moved| patch::KaraokeRow {
            track: 3,
            blocks: vec![],
            was: String::new(),
            now: String::new(),
            changed,
            added,
            removed,
            moved,
        };
        assert_eq!(letter_counts(&row(1, 0, 0, 0)), "1 changed");
        assert_eq!(letter_counts(&row(0, 1, 0, 12)), "1 added, 12 moved");
        assert_eq!(
            letter_counts(&row(2, 0, 1, 3)),
            "2 changed, 1 removed, 3 moved"
        );
    }

    #[test]
    fn a_karaoke_fix_says_how_much_its_row_grew_and_what_was_guessed() {
        assert_eq!(
            respaced(29, ""),
            "karaoke re-spaced by +29 px: check it (review, o)"
        );
        assert_eq!(
            respaced(-24, "qz"),
            "karaoke re-spaced by -24 px (no \"q\", \"z\" in this style to measure): check it (review, o)"
        );
    }

    #[test]
    fn files_of_one_name_get_names_no_other_file_has() {
        assert_eq!(
            track_names(&["/a/E01.mkv", "/b/E01.mkv", "/c/E01.2.mkv", "/d/E02.mkv"]),
            ["E01.1", "E01.3", "E01.2", "E02"]
        );
    }

    #[test]
    fn a_backup_that_turned_up_meanwhile_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (file, remuxed, kept) = (
            dir.path().join("a.mkv"),
            dir.path().join(".a.mkv.subcheck"),
            dir.path().join("a.mkv.orig"),
        );
        std::fs::write(&file, "original").unwrap();
        std::fs::write(&remuxed, "remux").unwrap();
        std::fs::write(&kept, "an earlier backup").unwrap();
        let err = swap(&file, &remuxed, Some(&kept)).unwrap_err();
        assert!(
            err.ends_with("exists: an earlier backup is never overwritten"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "an earlier backup");
        // Without one there, the original is kept and the remux takes its place.
        std::fs::remove_file(&kept).unwrap();
        swap(&file, &remuxed, Some(&kept)).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "remux");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "original");
        assert!(!remuxed.exists());
    }

    #[test]
    fn the_remux_is_hidden_beside_the_original_and_the_backup_ends_in_orig() {
        let file = Path::new("/m/K-On!! - E01.mkv");
        assert_eq!(remux_path(file), Path::new("/m/.K-On!! - E01.mkv.subcheck"));
        assert_eq!(backup_path(file), Path::new("/m/K-On!! - E01.mkv.orig"));
    }
}

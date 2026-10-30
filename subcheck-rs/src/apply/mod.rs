//! `subcheck apply`: make a run's accepted fixes in the MKV files. Per file: find each fix's
//! events, write the fixed tracks, remux with mkvmerge beside the original, check the remux, and
//! swap it in, keeping the original as `.orig` unless told not to.

pub mod exact;
pub mod karaoke;
pub mod patch;
pub mod remux;
pub mod shift;
pub mod verify;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::blocks::{self, Block};
use crate::group;
use crate::list::shell_quote;
use crate::mkv::{self, Layout, Track};
use crate::review::model::changed;
use crate::store::{self, Applied, Finding, Run, Store};
use crate::subs::{self, Kind};
use crate::text::{timestamp, timestamp_ms};
use patch::{Fix, Outcome};
use remux::Replacement;
use shift::{Grid, Lines, Shifts};
use verify::Languages;

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

/// Make `run`'s accepted fixes, as `mode` says, marking each one made (or found made) in `store`;
/// `state` is the state directory. `karaoke`: re-space per-letter karaoke rows rather than leave
/// them for a hand fix.
pub fn run(
    run: &Run,
    store: &mut Store,
    state: &Path,
    mode: &Mode,
    karaoke: bool,
    mkvmerge: &OsStr,
    out: &mut impl Write,
) -> anyhow::Result<Summary> {
    let accepted = patch::fixes(run);
    let timing = shift::fixes(run);
    let mut summary = Summary::default();
    if accepted.is_empty() && timing.is_empty() {
        writeln!(out, "No accepted fixes in {}.", run.called())?;
        return Ok(summary);
    }
    // The check reads the original's languages as mkvmerge does.
    if matches!(mode, Mode::Apply { .. } | Mode::Check { .. }) {
        remux::run(mkvmerge, &["--version".into()]).map_err(anyhow::Error::msg)?;
    }
    let marking = matches!(mode, Mode::Apply { .. });
    let paths = with_fixes(run, &accepted, &timing);
    let names = track_names(&paths);
    for (path, name) in paths.iter().zip(names) {
        let mine: Vec<Fix> = accepted
            .iter()
            .filter(|f| f.finding.file == *path)
            .cloned()
            .collect();
        let shifts: Vec<&Finding> = timing.iter().filter(|f| f.file == *path).copied().collect();
        writeln!(out, "{path}")?;
        let job = Job {
            path: Path::new(path),
            name,
            fixes: &mine,
            timing: &shifts,
            mode,
            karaoke,
            state,
            mkvmerge,
        };
        match job.run(out) {
            Ok(done) => {
                for (_, o) in &done.outcomes {
                    match o {
                        Outcome::Patched | Outcome::Respaced { .. } | Outcome::Shifted { .. } => {
                            summary.fixed += 1;
                        }
                        Outcome::AlreadyApplied => summary.already += 1,
                        Outcome::ByHand(_) => summary.by_hand += 1,
                        Outcome::NotFound => summary.not_found += 1,
                    }
                }
                summary.remuxed += usize::from(done.remuxed);
                if marking {
                    let at = jiff::Timestamp::now();
                    let applied: Vec<Applied> = done
                        .outcomes
                        .iter()
                        .filter(|(_, o)| {
                            matches!(
                                o,
                                Outcome::AlreadyApplied
                                    | Outcome::Patched
                                    | Outcome::Respaced { .. }
                                    | Outcome::Shifted { .. }
                            )
                        })
                        .map(|&(finding, _)| Applied { finding, at })
                        .collect();
                    // The file is swapped in already. If the database stays busy past the timeout,
                    // apply stops here and this file's marks are not saved; the next `apply`
                    // finds the fixes already in the file and marks them then.
                    store.mark_applied(&applied)?;
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
    // The check plans the fixes again, and must plan them as these tracks were.
    if matches!(mode, Mode::TracksOnly(_)) && s.fixed > 0 {
        let flag = if karaoke { " --karaoke" } else { "" };
        writeln!(
            out,
            "Once they are remuxed, check them with: subcheck apply {} --check{flag}",
            run.name
        )?;
    }
    Ok(summary)
}

/// The files with fixes, in the order the scan checked them.
fn with_fixes<'r>(run: &store::Run, accepted: &[Fix<'r>], timing: &[&'r Finding]) -> Vec<&'r str> {
    let mut files: Vec<&str> = accepted
        .iter()
        .map(|f| f.finding.file.as_str())
        .chain(timing.iter().map(|f| f.file.as_str()))
        .collect();
    files.sort_by_cached_key(|p| {
        run.files
            .iter()
            .position(|r| r.path == *p)
            .unwrap_or(usize::MAX)
    });
    let mut paths: Vec<&str> = Vec::new();
    for p in files {
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    paths
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
    /// Its accepted timing findings.
    timing: &'a [&'a Finding],
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

/// How far after its time a video frame may be stored: one shown before a keyframe can come after
/// it, in the next Cluster.
const REORDER_NS: i128 = 1_000_000_000;

/// Frames less than this apart are read in one walk of the video's Clusters.
const SPAN_GAP_NS: i128 = 10_000_000_000;

/// The stored times of `frames`, each exactly between two stored times on `grid`, read from the
/// first video track's Clusters around them: by frame, in frame order. A frame found at neither
/// time is left out, and its moves stay unsure. Clusters that do not read are an error, as in the
/// check's audio spans.
fn stored(
    r: &crate::ebml::Reader,
    layout: &Layout,
    grid: &Grid,
    frames: &BTreeSet<i128>,
) -> Result<Vec<(i128, i128)>, String> {
    let mut found = Vec::new();
    let Some(video) = layout
        .tracks
        .iter()
        .find(|t| t.kind == mkv::TRACK_TYPE_VIDEO)
    else {
        return Ok(found);
    };
    // Each span, its frames with their two times, is walked once.
    let mut spans: Vec<Vec<(i128, [i128; 2])>> = Vec::new();
    for &n in frames {
        let times = grid.either(n);
        match spans.last_mut() {
            Some(span)
                if span
                    .last()
                    .is_some_and(|(_, t)| times[0] - t[1] < SPAN_GAP_NS) =>
            {
                span.push((n, times));
            }
            _ => spans.push(vec![(n, times)]),
        }
    }
    for span in spans {
        let (from, to) = (span[0].1[0], span[span.len() - 1].1[1]);
        let read = blocks::frames_between(r, layout, from, to + REORDER_NS, &[video.number])
            .map_err(|e| {
                let at = timestamp_ms(i64::try_from(from / 1_000_000).unwrap_or(i64::MAX));
                format!("the video's frames from {at}: {e}")
            })?;
        let times: HashSet<i128> = read
            .get(&video.number)
            .into_iter()
            .flatten()
            .filter_map(|f| f.time_ns)
            .collect();
        for (n, [earlier, later]) in span {
            match (times.contains(&earlier), times.contains(&later)) {
                (true, false) => found.push((n, earlier)),
                (false, true) => found.push((n, later)),
                _ => {}
            }
        }
    }
    Ok(found)
}

/// The original file, and the blocks read of its `S_TEXT` tracks ([`exact::rewritten`]), by track
/// ID.
struct Original<'a> {
    r: &'a crate::ebml::Reader,
    layout: &'a Layout,
    read: &'a BTreeMap<usize, &'a [Block]>,
}

/// A track with fixes: the original's header, its blocks as patched, and how early its written times
/// come back ([`exact::writable`]).
struct Fixed<'a> {
    track: &'a Track,
    kind: Kind,
    patched: Vec<Block>,
    early: exact::Early,
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
            .chain(self.timing.iter().map(|f| timing_fix(f).track))
            .collect();
        let chosen: Vec<&Track> = layout
            .tracks
            .iter()
            .filter(|t| ids.contains(&t.id) && t.is_text_subtitle())
            .collect();
        // Read once, for the plan and the check alike: every track whose lines a remux rewrites
        // where they are not UTF-8 (any S_TEXT track), so the plan looks at them all.
        let wanted: Vec<&Track> = layout
            .tracks
            .iter()
            .filter(|t| exact::rewritten(t))
            .collect();
        let numbers: Vec<u64> = wanted.iter().map(|t| t.number).collect();
        let walk = verify::must_walk(&layout, &wanted);
        let read = blocks::read_tracks(&r, &layout, &numbers, walk).map_err(|e| e.to_string())?;
        let read: BTreeMap<usize, &[Block]> = wanted
            .iter()
            .zip(&read)
            .map(|(t, b)| (t.id, b.blocks.as_slice()))
            .collect();
        let file = exact::remuxable(wanted.iter().map(|t| (*t, read[&t.id])));
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
        let mut views: Vec<patch::Track> = chosen
            .iter()
            .zip(&headers)
            .map(|(t, header)| {
                let mut view = patch::Track {
                    id: t.id,
                    kind: Kind::of(t).expect("a text subtitle track"),
                    blocks: read[&t.id],
                    header,
                    scale: layout.timestamp_scale,
                    default_duration: t.default_duration,
                    timing: patch::Timing::AsScanned,
                    writable: Ok(exact::Early::NONE),
                };
                view.writable = exact::writable(&view, &BTreeMap::new(), file);
                view
            })
            .collect();
        let mut known = Vec::new();
        let (plan, grid) = self.plan(&mut views, &r, &layout, file, &mut known)?;
        self.report(&plan, &views, grid.as_ref(), out)
            .map_err(|e| e.to_string())?;
        note_tags(&r, &layout, &chosen, &plan, out)?;
        let fixed: Vec<Fixed> = chosen
            .iter()
            .zip(&views)
            .filter_map(|(t, view)| {
                let edits = plan.edits.get(&t.id)?;
                let kind = Kind::of(t).expect("a text subtitle track");
                Some(Fixed {
                    track: t,
                    kind,
                    patched: patch::edited(kind, read[&t.id], edits),
                    early: view
                        .writable
                        .expect("a track is edited only where it can be written"),
                })
            })
            .collect();
        note_early(&fixed, out)?;
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

    /// Both kinds of fix: the timing fixes first, as a track remuxed with its timing fix has its
    /// typos at their times shifted. And the file's frame grid, where it has timing fixes and one.
    /// `file`: whether the file can be remuxed as it is ([`exact::remuxable`]). `known` holds the
    /// stored times the file gives the frames the keyframes leave at either of two: the
    /// keyframes' own, in the Cues, and those the fixes turn on, read from `r`.
    fn plan<'v>(
        &'v self,
        views: &mut [patch::Track<'v>],
        r: &crate::ebml::Reader,
        layout: &Layout,
        file: Result<(), exact::Unwritable>,
        known: &'v mut Vec<(i128, i128)>,
    ) -> Result<(patch::Plan, Option<Grid<'v>>), String> {
        if self.timing.is_empty() {
            return Ok((patch::plan(views, self.fixes, self.karaoke), None));
        }
        let video = crate::timing::video(layout);
        let fit = video
            .as_ref()
            .map_err(String::clone)
            .and_then(|v| Grid::fit(v, layout.timestamp_scale));
        if let (Ok(video), Ok(grid)) = (&video, &fit) {
            *known = grid.cued(video);
        }
        let cued = fit.clone().map(|g| g.knowing(known));
        let mut shifted = self.shift(views, &cued);
        let unsure = self.unsure(views, &cued, &shifted);
        let read = match &cued {
            Ok(grid) if !unsure.is_empty() => stored(r, layout, grid, &unsure)?,
            _ => Vec::new(),
        };
        let again = !read.is_empty();
        known.extend(read);
        known.sort_unstable();
        let known: &'v Vec<(i128, i128)> = known;
        let grid = fit.map(|g| g.knowing(known));
        if again {
            shifted = self.shift(views, &grid);
        }
        // A track its timing fix moves is written at the new times. One that cannot be written out
        // exactly so gets neither the shift nor (as its view says) its typo fixes.
        for (f, p) in self.timing.iter().zip(&mut shifted) {
            let Some(view) = views.iter_mut().find(|v| v.id == timing_fix(f).track) else {
                continue;
            };
            let moved = p.times.iter().map(|&(i, t, d)| (i, (t, d))).collect();
            view.writable = exact::writable(view, &moved, file);
            if let (Err(why), Outcome::Shifted { .. }) = (view.writable, &p.outcome) {
                p.outcome = Outcome::ByHand(why.to_string());
                p.times.clear();
            }
        }
        // Where each track's typos are, by where its timing fix's lines are.
        for view in views.iter_mut() {
            let mut mine = self.timing.iter().zip(&shifted);
            let Some((f, p)) = mine.find(|(f, _)| timing_fix(f).track == view.id) else {
                continue;
            };
            let fix = timing_fix(f);
            view.timing = match (p.lines, &grid) {
                (Lines::AsScanned, _) => patch::Timing::AsScanned,
                (Lines::Moved, Ok(grid)) => {
                    patch::Timing::Moved(Shifts::new(fix, *grid, view.kind))
                }
                _ => patch::Timing::Unsure(fix),
            };
        }
        let mut plan = patch::plan(views, self.fixes, self.karaoke);
        for (f, p) in self.timing.iter().zip(shifted) {
            // A track with nothing to change gets no edits: they would remux it.
            if !p.times.is_empty() {
                let times = &mut plan.edits.entry(timing_fix(f).track).or_default().times;
                times.extend(p.times.iter().map(|&(i, t, d)| (i, (t, d))));
            }
            plan.outcomes.push((f.id, p.outcome));
        }
        Ok((plan, grid.ok()))
    }

    /// The frames whose stored times, read from the file, may settle the timing fixes: those
    /// their plans name, and those of the lines a typo is on, on a track its timing fix moved or
    /// may read as moved once they are read, where it is looked for at the line's times shifted.
    fn unsure(
        &self,
        views: &[patch::Track],
        grid: &Result<Grid, String>,
        shifted: &[shift::Plan],
    ) -> BTreeSet<i128> {
        let Ok(grid) = grid else {
            return BTreeSet::new();
        };
        let mut frames: BTreeSet<i128> = shifted.iter().flat_map(|p| p.unsure.clone()).collect();
        for (f, p) in self.timing.iter().zip(shifted) {
            let fix = timing_fix(f);
            let Some(view) = views.iter().find(|v| v.id == fix.track) else {
                continue;
            };
            let moved = match p.lines {
                Lines::Moved => true,
                Lines::Unsure => !p.unsure.is_empty(),
                Lines::AsScanned => false,
            };
            if !moved {
                continue;
            }
            let shifts = Shifts::new(fix, *grid, view.kind);
            let typos = self.fixes.iter().flat_map(|x| &x.finding.occurrences);
            for o in typos.filter(|o| o.track == fix.track) {
                frames.extend(shifts.times(o.start_ms, o.end_ms).err().unwrap_or_default());
            }
        }
        frames
    }

    /// Each timing fix's plan on its track, on the file's frame grid (or why it has none).
    fn shift(&self, views: &[patch::Track], grid: &Result<Grid, String>) -> Vec<shift::Plan> {
        // Not looked at: where its lines are is not known.
        let none = |outcome| shift::Plan {
            outcome,
            times: Vec::new(),
            lines: Lines::Unsure,
            unsure: Vec::new(),
        };
        self.timing
            .iter()
            .map(|f| {
                let fix = timing_fix(f);
                let Some(view) = views.iter().find(|v| v.id == fix.track) else {
                    return none(Outcome::NotFound);
                };
                let on_track = self
                    .timing
                    .iter()
                    .filter(|g| timing_fix(g).track == fix.track);
                if on_track.count() > 1 {
                    return none(Outcome::ByHand("two timing fixes on one track".into()));
                }
                match grid {
                    Ok(grid) => shift::plan(fix, view, grid),
                    // Where its lines are needs no grid where they are as the scan found them.
                    Err(why) => shift::Plan {
                        lines: shift::lines_without_grid(fix, view),
                        ..none(Outcome::ByHand(why.clone()))
                    },
                }
            })
            .collect()
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
                // Read as mkvmerge reads them, as `--check` will, where mkvmerge is here to ask;
                // else each goes as the file has it.
                let languages = remux::languages(self.mkvmerge, self.path).unwrap_or_default();
                let args = remux::args(
                    self.path,
                    o.layout,
                    &files,
                    &languages,
                    &remux_path(self.path),
                );
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
                let languages = remux::languages(self.mkvmerge, self.path)?;
                let diffs = verify(o, &remuxed, fixed, *full, &languages)?;
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
        grid: Option<&Grid>,
        out: &mut impl Write,
    ) -> io::Result<()> {
        let (typos, shifts) = plan.outcomes.split_at(self.fixes.len());
        for (fix, (_, outcome)) in self.fixes.iter().zip(typos) {
            let f = fix.finding;
            let when = f.occurrences.first().map_or(0, |o| o.start_ms);
            let (a, b) = changed(&f.original, &fix.text);
            let what = format!("{} → {}", &f.original[a], &fix.text[b]);
            writeln!(out, "  {}  {what}  {}", timestamp(when), status(outcome))?;
        }
        for (f, (_, outcome)) in self.timing.iter().zip(shifts) {
            let fix = timing_fix(f);
            let what = format!("{} → {}", f.original, f.suggestion);
            writeln!(out, "  track {}  {what}  {}", fix.track, status(outcome))?;
            let Some(view) = views.iter().find(|v| v.id == fix.track) else {
                continue;
            };
            for l in &fix.unshifted {
                let (start, end) = (time(view.kind, l.start_ms), time(view.kind, l.end_ms));
                let text = line_text(view.kind, &l.raw);
                writeln!(out, "    left where it is: {start} – {end}  {text}")?;
            }
            // Each moving stretch's first and last line, before and after.
            let shifted = grid
                .filter(|_| matches!(outcome, Outcome::Shifted { .. }))
                .map(|grid| Shifts::new(fix, *grid, view.kind));
            if let (Mode::DryRun, Some(shifts)) = (self.mode, shifted) {
                let ends = fix
                    .stretches
                    .iter()
                    .flat_map(|s| s.first.iter().chain(&s.last));
                for l in ends {
                    let Ok(Some((start, end))) = shifts.times(l.start_ms, l.end_ms) else {
                        continue;
                    };
                    let t = |ms| time(view.kind, ms);
                    writeln!(
                        out,
                        "    track {}: {} – {} → {} – {}  {}",
                        fix.track,
                        t(l.start_ms),
                        t(l.end_ms),
                        t(start),
                        t(end),
                        line_text(view.kind, &l.raw)
                    )?;
                }
            }
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
                sync_ms: f.early.ms,
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
        let dir = dir_of(self.path);
        if let Some(free) = free_space(dir)
            && free < size
        {
            return Err(format!(
                "not enough space in {}: the remux needs {size} bytes, {free} are free",
                dir.display()
            ));
        }
        let remuxed = remux_path(self.path);
        // Created here first: an unwritable directory says so before any work. Held through the
        // swap too: once the remux is renamed into place, removing it by its old name finds
        // nothing.
        let _removing =
            match group::create_removing(&remuxed, |path| fs::File::create(path).map(drop)) {
                Ok(removing) => removing,
                Err(e) => {
                    return Err(format!(
                        "cannot write in {}: {e} (use --tracks-only)",
                        dir.display()
                    ));
                }
            };
        // And the original can be linked as the backup: a refusal (a filesystem without links,
        // or an original another user owns) says so now, not after the remux. The probe's name
        // is ours: one left by an interrupted run is removed first.
        if backup {
            let mut probe = remuxed.as_os_str().to_owned();
            probe.push(".link");
            let probe = PathBuf::from(probe);
            let _ = fs::remove_file(&probe);
            let linked = group::create_removing(&probe, |probe| fs::hard_link(self.path, probe))
                .map(|removing| {
                    let _ = removing.forget_after(|| fs::remove_file(&probe));
                });
            if let Err(e) = linked {
                let _ = fs::remove_file(&remuxed);
                return Err(format!(
                    "cannot keep the original as a backup: {e} (--no-backup swaps without one)"
                ));
            }
        }
        // mkvmerge's reading of the languages goes into the remux and its check: read before the
        // remux, so a file it cannot read is told before many GB are written, not after.
        let result = remux::languages(self.mkvmerge, self.path)
            .and_then(|languages| self.remux_and_check(o, fixed, &remuxed, &languages, full))
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
        languages: &Languages,
        full: bool,
    ) -> Result<(), String> {
        let tracks = self.state.join("apply");
        let files = self.write_tracks(&tracks, o, fixed)?;
        let args = remux::args(self.path, o.layout, &files, languages, remuxed);
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
        let diffs = verify(o, remuxed, fixed, full, languages)?;
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
    let swapped = |e| format!("swapping in the remux: {e}");
    let Some(kept) = kept else {
        return fs::rename(remuxed, file).map_err(swapped);
    };
    // Until the rename, a signal removes the link too: left behind, it would stop every later run
    // as an earlier backup. Linked and registered as one, and renamed (or, failing that, removed)
    // before it is forgotten, with no signal between: after the rename, the link is the only
    // original.
    let unlinking =
        group::create_removing(kept, |kept| fs::hard_link(file, kept)).map_err(|e| {
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
    unlinking
        .forget_after(|| {
            fs::rename(remuxed, file).inspect_err(|_| {
                let _ = fs::remove_file(kept);
            })
        })
        .map_err(swapped)
}

/// `remuxed` against the original, with `fixed` in place of its tracks, and the original's
/// `languages` as mkvmerge reads them.
fn verify(
    o: &Original,
    remuxed: &Path,
    fixed: &[Fixed],
    full: bool,
    languages: &Languages,
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
        languages,
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

/// In every mode, the dry run included: each fixed track written early, which the remux's `--sync`
/// puts back.
fn note_early(fixed: &[Fixed], out: &mut impl Write) -> Result<(), String> {
    for f in fixed.iter().filter(|f| f.early != exact::Early::NONE) {
        let ms = f.early.ms;
        say(
            out,
            &format!(
                "track {}: every time is {ms} ms past the centisecond: its .{} is written {ms} ms early and remuxed with --sync 0:{ms}",
                f.track.id,
                f.kind.extension()
            ),
        )?;
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

/// What a fix came to, as its line ends.
fn status(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Patched => "ok".to_string(),
        Outcome::Respaced { px, guessed } => respaced(*px, guessed),
        Outcome::Shifted { events: 1 } => "ok (1 event moved)".into(),
        Outcome::Shifted { events } => format!("ok ({events} events moved)"),
        Outcome::AlreadyApplied => "already applied".into(),
        Outcome::ByHand(why) => format!("by hand: {why}"),
        Outcome::NotFound => "not found: the file changed since the scan".into(),
    }
}

/// A timing finding's fix.
fn timing_fix(f: &Finding) -> &store::TimingFix {
    f.timing.as_ref().expect("a timing finding has its fix")
}

/// A time as the track writes it: centiseconds for ASS and SSA, milliseconds for SRT.
fn time(kind: Kind, ms: i64) -> String {
    match kind {
        Kind::Ass | Kind::Ssa => timestamp(ms),
        Kind::Srt => crate::text::timestamp_ms(ms),
    }
}

/// A recorded line's text.
fn line_text(kind: Kind, raw: &str) -> &str {
    shift::fields_and_text(kind, raw).1
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

/// The directory `file` is in: `.` for a bare file name, whose parent is "" (which statvfs, for
/// one, refuses).
fn dir_of(file: &Path) -> &Path {
    file.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Bytes free to an ordinary user in the filesystem holding `dir`; `None` when it cannot be told.
fn free_space(dir: &Path) -> Option<u64> {
    free_blocks(dir).map(|(blocks, size)| blocks.saturating_mul(size))
}

/// The blocks free to an ordinary user in the filesystem holding `dir`, and their size.
///
/// On macOS from statfs: its statvfs casts statfs's 64-bit block counts to 32 bits, so with 2^32
/// blocks or more free (16 TiB at 4 KiB) it tells only the remainder.
fn free_blocks(dir: &Path) -> Option<(u64, u64)> {
    #[cfg(target_os = "macos")]
    let free = {
        let s = rustix::fs::statfs(dir).ok()?;
        (s.f_bavail, u64::from(s.f_bsize))
    };
    #[cfg(not(target_os = "macos"))]
    let free = {
        let s = rustix::fs::statvfs(dir).ok()?;
        (s.f_bavail, s.f_frsize)
    };
    Some(free)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_file_name_is_in_the_current_directory() {
        assert_eq!(dir_of(Path::new("ep01.mkv")), Path::new("."));
        assert_eq!(dir_of(Path::new("s1/ep01.mkv")), Path::new("s1"));
        assert_eq!(dir_of(Path::new("/m/ep01.mkv")), Path::new("/m"));
        assert!(free_space(dir_of(Path::new("ep01.mkv"))).is_some());
    }

    #[test]
    fn free_space_is_told_for_a_directory_and_not_for_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        // Some: a full filesystem has 0 bytes free, and that is told too.
        assert!(free_space(dir.path()).is_some());
        assert_eq!(free_space(&dir.path().join("missing")), None);
    }

    // Where rustix has statfs, to read the truth from.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "needs SUBCHECK_BIG_VOLUME, a volume with 2^32 blocks or more free"]
    fn free_blocks_past_2_to_the_32_are_told_whole() {
        // Past 2^32 blocks free, macOS's statvfs tells only the remainder. No CI runner has such a
        // volume (a sparse disk image tells its host's free space as its own), so run it by hand:
        // SUBCHECK_BIG_VOLUME=<dir> cargo test --lib free_blocks_past -- --ignored
        let dir = std::env::var_os("SUBCHECK_BIG_VOLUME")
            .filter(|v| !v.is_empty())
            .expect("SUBCHECK_BIG_VOLUME names no directory");
        let dir = Path::new(&dir);
        let s = rustix::fs::statfs(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let told = s.f_bavail;
        assert!(
            told >= 1 << 32,
            "{}: statfs tells {told} blocks of {} bytes free, fewer than 2^32",
            dir.display(),
            s.f_bsize
        );
        let (blocks, _) = free_blocks(dir).expect("free blocks");
        assert!(
            blocks >= 1 << 32,
            "{}: {blocks} blocks free against statfs's {told}: truncated to 32 bits",
            dir.display()
        );
    }

    #[test]
    fn files_go_in_the_order_the_scan_checked_them() {
        let finding = |id, file: &str| Finding {
            id,
            file: file.into(),
            style: String::new(),
            original: String::new(),
            suggestion: String::new(),
            reason: String::new(),
            source: "llm".into(),
            occurrences: vec![],
            before: vec![],
            after: vec![],
            timing: None,
        };
        let record = |path: &str| store::FileRecord {
            path: path.into(),
            size: 0,
            mtime: jiff::Timestamp::UNIX_EPOCH,
            error: None,
            tracks: vec![],
            lines: 0,
            truncated: None,
            checks: vec![],
        };
        // A typo in b found first, then a's timing, added by a later `scan --run`.
        let run = store::Run {
            files: vec![record("a.mkv"), record("b.mkv")],
            findings: vec![
                finding(1, "b.mkv"),
                finding(2, "a.mkv"),
                finding(3, "c.mkv"),
            ],
            ..store::Run::default()
        };
        let typos = [Fix {
            finding: &run.findings[0],
            text: String::new(),
        }];
        let timing = [&run.findings[1], &run.findings[2]];
        assert_eq!(
            with_fixes(&run, &typos, &timing),
            ["a.mkv", "b.mkv", "c.mkv"]
        );
    }

    #[test]
    fn a_shift_says_how_many_events_it_moves() {
        assert_eq!(
            status(&Outcome::Shifted { events: 1 }),
            "ok (1 event moved)"
        );
        assert_eq!(
            status(&Outcome::Shifted { events: 2 }),
            "ok (2 events moved)"
        );
    }

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

//! What review, list and apply show: a run, or the files under some paths, put together from each
//! file's current checks as the in-memory [`Run`] review's screen works on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::bail;
use rusqlite::types::Type;
use rusqlite::{Params, Row, Transaction, params};
use serde::de::DeserializeOwned;

use super::write::{CALL_COLUMNS, call, from_json, state_of, time};
use super::{Check, Decision, FileRecord, Finding, Run, ScanRow, Store};
use crate::list::shell_quote;

/// What `review`, `list` and `apply` are given: `[run | paths…]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// The run that started last.
    Newest,
    /// No argument: the folders that hold the files the run that started last named.
    NewestFolders,
    /// A run, by name.
    Run(String),
    /// Files and directories, absolute: every file with a check that is one, or under one.
    Paths(Vec<PathBuf>),
}

impl Selector {
    /// `[run | paths…]`: nothing is the newest run's folders; an argument that exists on disk is a
    /// path, and anything else a run name, which goes alone.
    pub fn parse(args: &[String]) -> anyhow::Result<Selector> {
        match args {
            [] => Ok(Selector::NewestFolders),
            [one] if !Path::new(one).exists() => Ok(Selector::Run(one.clone())),
            many => {
                let mut paths = Vec::new();
                for a in many {
                    if !Path::new(a).exists() {
                        bail!("{a}: no such file or directory (a run name goes alone)");
                    }
                    paths.push(super::absolute(Path::new(a))?);
                }
                Ok(Selector::Paths(paths))
            }
        }
    }
}

impl Store {
    /// The view `s` selects. A run that is not there is `no run named R`; paths with nothing
    /// recorded under any of them, `nothing recorded under P`.
    pub fn view(&self, s: &Selector) -> anyhow::Result<Run> {
        match s {
            Selector::Newest => match self.newest_scan()? {
                Some(scan) => Ok(self.run_view(&scan)?),
                None => bail!("no runs yet"),
            },
            Selector::NewestFolders => match self.newest_scan()? {
                Some(scan) => self.folders_view(&scan),
                None => bail!("no runs yet"),
            },
            Selector::Run(name) => match self.scan_named(name)? {
                Some(scan) => Ok(self.run_view(&scan)?),
                None => bail!("no run named {name}"),
            },
            Selector::Paths(paths) => self.paths_view(paths),
        }
    }

    /// A run's view: each file it named, in the order it named them, as it is now: its current
    /// checks and their findings, each finding's current decision and applied mark, and its latest
    /// attempt when that failed. Two runs that share a file show it the same. Its cost is the
    /// run's own LLM calls.
    pub fn run_view(&self, scan: &ScanRow) -> rusqlite::Result<Run> {
        let tx = self.conn.unchecked_transaction()?;
        start_showing(&tx)?;
        tx.execute(
            "INSERT INTO temp.shown(path, pos) SELECT path, rowid FROM scan_files WHERE scan = ?1",
            [scan.id],
        )?;
        let mut run = shown(&tx)?;
        run.name.clone_from(&scan.name);
        run.header = Some(scan.header.clone());
        add_calls(
            &tx,
            &mut run,
            &format!("SELECT {CALL_COLUMNS} FROM usage WHERE scan = ?1 ORDER BY id"),
            params![scan.id],
        )?;
        tx.commit()?;
        Ok(run)
    }

    /// One file as a view shows it (see [`Store::run_view`]), with no name, header or cost: what
    /// review's screen reads back when a scan saves the file, or a decision followed one of its
    /// findings. A file with nothing recorded has no record and no findings.
    pub fn file_view(&self, path: &str) -> rusqlite::Result<Run> {
        let tx = self.conn.unchecked_transaction()?;
        start_showing(&tx)?;
        tx.execute("INSERT INTO temp.shown(path, pos) VALUES (?1, 0)", [path])?;
        let run = shown(&tx)?;
        tx.commit()?;
        Ok(run)
    }

    /// The view of `paths`: as a run's, of every file with a check that is one of `paths`, or
    /// under one that is a directory, by path. Its cost is those files' LLM calls in the scans that
    /// made their current LLM checks. Named by the paths, quoted, as `subcheck` takes them back.
    pub fn paths_view(&self, paths: &[PathBuf]) -> anyhow::Result<Run> {
        let shown: Vec<(PathBuf, bool)> = paths.iter().map(|p| (p.clone(), p.is_dir())).collect();
        self.shown_paths(&shown)
    }

    /// The folders that hold the files `scan` named, as a paths view: every file recorded under
    /// them, whichever runs checked them. They are folders whether or not they are on disk now.
    pub fn folders_view(&self, scan: &ScanRow) -> anyhow::Result<Run> {
        let files = {
            let mut stmt = self
                .conn
                .prepare("SELECT path FROM scan_files WHERE scan = ?1 ORDER BY rowid")?;
            stmt.query_map([scan.id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let shown: Vec<(PathBuf, bool)> = folders(&files).into_iter().map(|f| (f, true)).collect();
        self.shown_paths(&shown)
    }

    /// The paths view of `paths`, each with whether it is a folder (every file recorded under it)
    /// or a file (that file alone).
    fn shown_paths(&self, paths: &[(PathBuf, bool)]) -> anyhow::Result<Run> {
        let tx = self.conn.unchecked_transaction()?;
        start_showing(&tx)?;
        for (p, folder) in paths {
            let text = p.to_string_lossy();
            if *folder {
                // Every path that starts `dir/`: from `dir/` up to `dir0`, `0` being the
                // character after `/`. (`LIKE` would take a `_` in the path for any character.)
                let dir = text.trim_end_matches('/');
                tx.execute(
                    "INSERT OR IGNORE INTO temp.shown(path, pos)
                     SELECT DISTINCT path, 0 FROM checks WHERE path >= ?1 AND path < ?2",
                    params![format!("{dir}/"), format!("{dir}0")],
                )?;
            } else {
                tx.execute(
                    "INSERT OR IGNORE INTO temp.shown(path, pos)
                     SELECT DISTINCT path, 0 FROM checks WHERE path = ?1",
                    [text],
                )?;
            }
        }
        let named: Vec<String> = paths
            .iter()
            .map(|(p, _)| shell_quote(&p.to_string_lossy()))
            .collect();
        let named = named.join(" ");
        let any: i64 = tx.query_row("SELECT COUNT(*) FROM temp.shown", [], |r| r.get(0))?;
        if any == 0 {
            bail!("nothing recorded under {named}");
        }
        let mut run = shown(&tx)?;
        run.name = named;
        run.of_paths = true;
        run.paths = paths.iter().map(|(p, _)| p.clone()).collect();
        add_calls(
            &tx,
            &mut run,
            &format!(
                "SELECT {CALL_COLUMNS} FROM usage WHERE id IN (
                     SELECT u.id FROM temp.shown s
                     JOIN checks c ON c.path = s.path AND c.kind = 'llm'
                         AND c.replaced_by IS NULL AND c.error IS NULL
                     JOIN usage u ON u.scan = c.scan AND u.path = c.path)
                 ORDER BY id"
            ),
            params![],
        )?;
        tx.commit()?;
        Ok(run)
    }
}

/// The folders that hold `files`, in the order of the files, each once; a folder inside another of
/// them is left out, as that one covers it.
fn folders(files: &[String]) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = Vec::new();
    for f in files {
        if let Some(dir) = Path::new(f).parent()
            && !all.iter().any(|d| d == dir)
        {
            all.push(dir.to_path_buf());
        }
    }
    all.iter()
        .filter(|d| !all.iter().any(|o| o != *d && d.starts_with(o)))
        .cloned()
        .collect()
}

/// An empty `temp.shown`: the paths a view shows, with the order to show them in.
fn start_showing(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS shown(path TEXT PRIMARY KEY, pos INTEGER NOT NULL);
         DELETE FROM temp.shown;",
    )
}

/// A finding's columns, of `findings f`, in the order [`finding`] reads them.
pub(super) const FINDING_COLUMNS: &str = "f.id, f.path, f.style, f.original, f.suggestion, \
     f.reason, f.source, f.occurrences, f.\"before\", f.\"after\", f.timing";

/// The files in `temp.shown` as a view shows them, in its order (see [`Store::run_view`]): no
/// name, header or cost yet.
fn shown(tx: &Transaction) -> rusqlite::Result<Run> {
    let mut run = Run::default();
    let mut at: HashMap<String, usize> = HashMap::new();
    // Each file's current checks, then its latest attempt if that failed, oldest first, merged as
    // review's model takes a scan's records.
    let mut files = tx.prepare(
        "SELECT c.path, c.kind, c.size, c.mtime, c.lines, c.tracks, c.truncated, c.error
         FROM temp.shown s JOIN checks c ON c.path = s.path
         WHERE (c.replaced_by IS NULL AND c.error IS NULL)
             OR (c.error IS NOT NULL AND c.id = (SELECT MAX(id) FROM checks WHERE path = c.path))
         ORDER BY s.pos, s.path, c.id",
    )?;
    for f in files.query_map([], file_record)? {
        let f = f?;
        if let Some(&i) = at.get(&f.path) {
            run.files[i].merge(f);
        } else {
            at.insert(f.path.clone(), run.files.len());
            run.files.push(f);
        }
    }
    let current = "FROM temp.shown s
         JOIN checks c ON c.path = s.path AND c.replaced_by IS NULL AND c.error IS NULL
         JOIN findings f ON f.\"check\" = c.id";
    let mut findings = tx.prepare(&format!(
        "SELECT {FINDING_COLUMNS} {current} ORDER BY s.pos, s.path, f.id"
    ))?;
    for f in findings.query_map([], finding)? {
        run.findings.push(f?);
    }
    let mut decisions = tx.prepare(&format!(
        "SELECT d.finding, d.state, d.edited, d.at {current}
         JOIN decisions d ON d.finding = f.id ORDER BY d.id"
    ))?;
    for d in decisions.query_map([], |r| {
        Ok(Decision {
            finding: r.get(0)?,
            state: state_of(r, 1)?,
            edited: r.get(2)?,
            at: time(r, 3)?,
        })
    })? {
        let d = d?;
        run.decisions.insert(d.finding, d);
    }
    let mut applied = tx.prepare(&format!(
        "SELECT a.finding {current} JOIN applied a ON a.finding = f.id"
    ))?;
    for a in applied.query_map([], |r| r.get(0))? {
        run.applied.insert(a?);
    }
    Ok(run)
}

/// Add the cost of the LLM calls `sql` selects, by [`CALL_COLUMNS`].
fn add_calls(tx: &Transaction, run: &mut Run, sql: &str, p: impl Params) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(sql)?;
    for c in stmt.query_map(p, call)? {
        run.spent.add(c?.usage.as_ref());
    }
    Ok(())
}

/// A `checks` row as a file record of that one check: of no check when it is a failed attempt.
fn file_record(r: &Row) -> rusqlite::Result<FileRecord> {
    let error: Option<String> = r.get(7)?;
    let checks = if error.is_some() {
        Vec::new()
    } else {
        vec![check_of(r, 1)?]
    };
    Ok(FileRecord {
        path: r.get(0)?,
        size: r.get(2)?,
        mtime: time(r, 3)?,
        error,
        tracks: from_json(r, 5)?,
        lines: r.get(4)?,
        truncated: nullable_json(r, 6)?,
        checks,
    })
}

pub(super) fn finding(r: &Row) -> rusqlite::Result<Finding> {
    Ok(Finding {
        id: r.get(0)?,
        file: r.get(1)?,
        style: r.get(2)?,
        original: r.get(3)?,
        suggestion: r.get(4)?,
        reason: r.get(5)?,
        source: r.get(6)?,
        occurrences: from_json(r, 7)?,
        before: from_json(r, 8)?,
        after: from_json(r, 9)?,
        timing: nullable_json(r, 10)?,
    })
}

/// Column `i` of `r`, a nested value that may be null.
fn nullable_json<T: DeserializeOwned>(r: &Row, i: usize) -> rusqlite::Result<Option<T>> {
    match r.get::<_, Option<String>>(i)? {
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, Type::Text, Box::new(e))),
        None => Ok(None),
    }
}

fn check_of(r: &Row, i: usize) -> rusqlite::Result<Check> {
    let text: String = r.get(i)?;
    match text.as_str() {
        "llm" => Ok(Check::Llm),
        "timing" => Ok(Check::Timing),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            i,
            Type::Text,
            format!("no check is called {other}").into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Usage;
    use crate::store::write::tests::{at, attempt, decision, header, llm, store, track, typo};
    use crate::store::{Applied, CheckedWith, LlmCall, Made, State};

    /// Save an LLM check of `path` by `scan` with a finding per (original, suggestion).
    fn checked(store: &mut Store, scan: &ScanRow, path: &str, typos: &[(&str, &str)]) -> Vec<u64> {
        let tracks = [track("cues")];
        let findings = typos.iter().map(|(o, s)| typo(path, o, s)).collect();
        let saved = store
            .save(
                &attempt(scan.id, path, &tracks, "2026-10-17T10:01:00Z"),
                vec![Made {
                    with: llm(),
                    lines: 4,
                    findings,
                }],
            )
            .unwrap();
        saved.findings.iter().map(|f| f.id).collect()
    }

    fn call(path: &str, cost: f64) -> LlmCall {
        LlmCall {
            file: path.into(),
            lines: 4,
            usage: Some(Usage {
                input_tokens: 1000,
                output_tokens: 10,
                cost_usd: Some(cost),
                ..Usage::default()
            }),
        }
    }

    fn paths(run: &Run) -> Vec<&str> {
        run.files.iter().map(|f| f.path.as_str()).collect()
    }

    fn found(run: &Run) -> Vec<(u64, &str)> {
        run.findings
            .iter()
            .map(|f| (f.id, f.original.as_str()))
            .collect()
    }

    #[test]
    fn a_runs_view_shows_every_file_it_named_as_it_is_now() {
        let (_dir, mut store) = store();
        let (a, b, c) = ("/m/E01.mkv", "/m/E02.mkv", "/m/E03.mkv");
        let first = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[a.into(), b.into()])
            .unwrap();
        checked(&mut store, &first, a, &[("befor", "before")]);
        checked(&mut store, &first, b, &[("trough", "through")]);
        // The second scan names A, unchanged and skipped, and checks C.
        let second = store
            .begin_scan(&header("2026-10-17T11:00:00Z"), &[c.into(), a.into()])
            .unwrap();
        checked(&mut store, &second, c, &[("teh", "the")]);
        let view = store.run_view(&second).unwrap();
        assert_eq!(view.name, second.name);
        assert_eq!(view.header.as_ref(), Some(&second.header));
        assert_eq!(paths(&view), [c, a]);
        assert_eq!(found(&view), [(3, "teh"), (1, "befor")]);
        // A checked again by the second scan: both runs show its new findings, and a decision on
        // them, alike.
        let again = checked(
            &mut store,
            &second,
            a,
            &[("befor", "before"), ("Ihave", "I have")],
        );
        store
            .decide(&[decision(again[1], State::Rejected, None)])
            .unwrap();
        let (one, two) = (
            store.run_view(&first).unwrap(),
            store.run_view(&second).unwrap(),
        );
        assert_eq!(found(&one), [(4, "befor"), (5, "Ihave"), (2, "trough")]);
        assert_eq!(found(&two), [(3, "teh"), (4, "befor"), (5, "Ihave")]);
        assert_eq!(one.state_of(5), State::Rejected);
        assert_eq!(two.state_of(5), State::Rejected);
        assert_eq!(one.files[0], two.files[1]);
    }

    #[test]
    fn a_paths_view_has_a_named_file_or_every_file_under_a_named_directory() {
        let dir = tempfile::tempdir().unwrap();
        let show = dir.path().join("Show");
        let near = dir.path().join("Show_2");
        std::fs::create_dir_all(show.join("Specials")).unwrap();
        std::fs::create_dir_all(&near).unwrap();
        let under = |d: &Path, name: &str| d.join(name).to_string_lossy().into_owned();
        let (e01, sp, e02) = (
            under(&show, "E01.mkv"),
            under(&show, "Specials/SP1.mkv"),
            under(&near, "E02.mkv"),
        );
        let (_db, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[])
            .unwrap();
        for p in [&e02, &sp, &e01] {
            checked(&mut store, &scan, p, &[("befor", "before")]);
        }
        let view = store.paths_view(std::slice::from_ref(&show)).unwrap();
        // By path; a directory of a name that starts the same is no part of it.
        assert_eq!(paths(&view), [e01.as_str(), sp.as_str()]);
        assert_eq!(view.header, None);
        assert_eq!(view.name, shell_quote(&show.to_string_lossy()));
        assert_eq!(view.called(), view.name);
        assert_eq!(view.paths, std::slice::from_ref(&show));
        assert_eq!(view.shown_name(), "Show");
        assert_eq!(
            store.run_view(&scan).unwrap().called(),
            format!("run {}", scan.name)
        );
        let file = store.paths_view(&[PathBuf::from(&e02)]).unwrap();
        assert_eq!(paths(&file), [e02.as_str()]);
        let both = store
            .paths_view(&[PathBuf::from(&e02), show.clone()])
            .unwrap();
        assert_eq!(both.files.len(), 3);
        let nothing = dir.path().join("Nothing");
        std::fs::create_dir(&nothing).unwrap();
        let e = store
            .paths_view(std::slice::from_ref(&nothing))
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "nothing recorded under {}",
                shell_quote(&nothing.to_string_lossy())
            )
        );
    }

    #[test]
    fn a_directory_matches_only_the_paths_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        // Neighbours of `Show` in sort order: `!` and `-` sort below `/`, `0` and `_` above it,
        // and `%`, `_` and `[` are special in a `LIKE`.
        let names = ["Show", "Show!", "Show-S2", "Show%_[x]"];
        let mut recorded = Vec::new();
        for n in names {
            std::fs::create_dir_all(base.join(n)).unwrap();
            recorded.push(base.join(n).join("E01.mkv").to_string_lossy().into_owned());
        }
        let (_db, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[])
            .unwrap();
        for p in &recorded {
            checked(&mut store, &scan, p, &[("befor", "before")]);
        }
        for (n, file) in names.iter().zip(&recorded) {
            let view = store.paths_view(&[base.join(n)]).unwrap();
            assert_eq!(paths(&view), [file.as_str()], "{n}");
        }
        // A trailing slash names the same directory.
        let slash = format!("{}/", base.join("Show").display());
        let view = store.paths_view(&[PathBuf::from(slash)]).unwrap();
        assert_eq!(paths(&view), [recorded[0].as_str()]);
    }

    #[test]
    fn a_path_with_dot_or_dotdot_names_the_same_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join("x")).unwrap();
        std::fs::create_dir_all(base.join("Show")).unwrap();
        std::fs::write(base.join("Show/E01.mkv"), "").unwrap();
        let e01 = base.join("Show/E01.mkv").to_string_lossy().into_owned();
        let (_db, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[])
            .unwrap();
        checked(&mut store, &scan, &e01, &[("befor", "before")]);
        for odd in ["x/../Show", "Show/.", "x/../Show/E01.mkv"] {
            let arg = base.join(odd).to_string_lossy().into_owned();
            let Selector::Paths(parsed) = Selector::parse(&[arg]).unwrap() else {
                panic!("{odd} is no path");
            };
            let view = store.paths_view(&parsed).unwrap();
            assert_eq!(paths(&view), [e01.as_str()], "{odd}");
        }
    }

    #[test]
    fn a_failed_attempt_followed_by_a_successful_check_shows_the_check() {
        let (_dir, mut store) = store();
        let a = "/m/E01.mkv";
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[a.into()])
            .unwrap();
        let tracks = [track("cues")];
        store
            .fail(
                &attempt(scan.id, a, &tracks, "2026-10-17T10:30:00Z"),
                &[(llm(), 4)],
                "LLM: boom",
            )
            .unwrap();
        // A later attempt made only the timing check, and it succeeded.
        let ok = store
            .save(
                &attempt(scan.id, a, &tracks, "2026-10-17T11:00:00Z"),
                vec![Made {
                    with: CheckedWith::timing(&["eng".into()], 1),
                    lines: 4,
                    findings: vec![typo(a, "teh", "the")],
                }],
            )
            .unwrap();
        for view in [
            store.run_view(&scan).unwrap(),
            store.paths_view(&[PathBuf::from(a)]).unwrap(),
        ] {
            assert_eq!(view.files.len(), 1);
            assert_eq!(view.files[0].error, None);
            assert_eq!(view.files[0].checks, [Check::Timing]);
            assert_eq!(found(&view), [(ok.findings[0].id, "teh")]);
        }
    }

    #[test]
    fn the_latest_decision_wins_and_an_undo_is_a_row_of_its_own() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &["/m/E01.mkv".into()])
            .unwrap();
        checked(&mut store, &scan, "/m/E01.mkv", &[("befor", "before")]);
        store
            .decide(&[decision(1, State::Accepted, Some("before!"))])
            .unwrap();
        store.decide(&[decision(1, State::Rejected, None)]).unwrap();
        let view = store.run_view(&scan).unwrap();
        assert_eq!(view.state_of(1), State::Rejected);
        // An undo back to pending.
        store.decide(&[decision(1, State::Pending, None)]).unwrap();
        let view = store.run_view(&scan).unwrap();
        assert_eq!(view.state_of(1), State::Pending);
        assert_eq!(view.decisions[&1], decision(1, State::Pending, None));
    }

    #[test]
    fn applied_marks_show_and_each_view_costs_its_own_calls() {
        let (_dir, mut store) = store();
        let (a, b) = ("/m/E01.mkv", "/m/E02.mkv");
        let first = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[a.into(), b.into()])
            .unwrap();
        checked(&mut store, &first, a, &[("befor", "before")]);
        checked(&mut store, &first, b, &[("trough", "through")]);
        store
            .add_usage(first.id, &[call(a, 1.0), call(b, 2.0)])
            .unwrap();
        store
            .mark_applied(&[Applied {
                finding: 2,
                at: at("2026-10-17T12:00:00Z"),
            }])
            .unwrap();
        let view = store.run_view(&first).unwrap();
        assert_eq!(view.applied, [2].into());
        assert_eq!((view.spent.calls, view.spent.cost_usd), (2, 3.0));
        // A checked again by a second scan, with a call of its own.
        let second = store
            .begin_scan(&header("2026-10-17T11:00:00Z"), &[a.into()])
            .unwrap();
        checked(&mut store, &second, a, &[("befor", "before")]);
        store.add_usage(second.id, &[call(a, 4.0)]).unwrap();
        let view = store.run_view(&first).unwrap();
        assert_eq!((view.spent.calls, view.spent.cost_usd), (2, 3.0));
        // The paths view of both files: A's calls from the scan that made its current check,
        // and B's.
        let both = store
            .paths_view(&[PathBuf::from(a), PathBuf::from(b)])
            .unwrap();
        assert_eq!((both.spent.calls, both.spent.cost_usd), (2, 6.0));
        assert_eq!(both.applied, [2].into());
    }

    #[test]
    fn a_file_whose_latest_attempt_failed_shows_failed_over_its_findings() {
        let (_dir, mut store) = store();
        let (a, b) = ("/m/E01.mkv", "/m/E02.mkv");
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[a.into(), b.into()])
            .unwrap();
        let tracks = [track("cues")];
        checked(&mut store, &scan, a, &[("befor", "before")]);
        store
            .fail(
                &attempt(scan.id, a, &tracks, "2026-10-17T11:00:00Z"),
                &[(llm(), 4)],
                "LLM: boom",
            )
            .unwrap();
        store
            .fail(
                &attempt(scan.id, b, &[], "2026-10-17T11:00:00Z"),
                &[(llm(), 0)],
                "cannot read",
            )
            .unwrap();
        let view = store.run_view(&scan).unwrap();
        let shown: Vec<(&str, Option<&str>, &[Check])> = view
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.error.as_deref(), f.checks.as_slice()))
            .collect();
        assert_eq!(
            shown,
            [
                (a, Some("LLM: boom"), [Check::Llm].as_slice()),
                (b, Some("cannot read"), [].as_slice()),
            ]
        );
        assert_eq!(found(&view), [(1, "befor")]);
        assert_eq!(view.files[0].tracks, tracks);
        // Checked again, it is no longer failed.
        checked(&mut store, &scan, a, &[("befor", "before")]);
        let view = store.run_view(&scan).unwrap();
        assert_eq!(view.files[0].error, None);
        let timing = CheckedWith::timing(&["eng".into()], 1);
        assert!(
            !store
                .is_fresh(b, 10, at("2026-10-17T10:00:00Z"), &timing)
                .unwrap()
        );
    }

    #[test]
    fn the_newest_runs_folders_are_every_file_recorded_where_its_files_sit() {
        let (_db, mut store) = store();
        assert_eq!(
            store
                .view(&Selector::NewestFolders)
                .unwrap_err()
                .to_string(),
            "no runs yet"
        );
        let (e01, e02, sp, s2) = (
            "/m/S1/E01.mkv",
            "/m/S1/E02.mkv",
            "/m/S1/Specials/SP1.mkv",
            "/m/S2/E01.mkv",
        );
        let first = store
            .begin_scan(
                &header("2026-10-17T10:00:00Z"),
                &[e01.into(), e02.into(), sp.into(), s2.into()],
            )
            .unwrap();
        for p in [e01, e02, sp, s2] {
            checked(&mut store, &first, p, &[("befor", "before")]);
        }
        // The newest run names a file in S1's Specials and one in S1: S1 covers both. Its folders
        // are not on disk, and are folders all the same.
        let second = store
            .begin_scan(&header("2026-10-17T11:00:00Z"), &[sp.into(), e02.into()])
            .unwrap();
        checked(&mut store, &second, e02, &[("trough", "through")]);
        let view = store.view(&Selector::NewestFolders).unwrap();
        assert!(view.of_paths);
        assert_eq!(view.paths, [PathBuf::from("/m/S1")]);
        assert_eq!(view.name, "'/m/S1'");
        assert_eq!(view.shown_name(), "S1");
        // Every file recorded under S1, the first run's E01 too.
        assert_eq!(paths(&view), [e01, e02, sp]);
        // The newest run itself is still there.
        let newest = store.view(&Selector::Newest).unwrap();
        assert_eq!(paths(&newest), [sp, e02]);
        assert_eq!(newest.shown_name(), format!("run {}", second.name));
        // Two folders, in the order the run named their files.
        store
            .begin_scan(&header("2026-10-17T12:00:00Z"), &[s2.into(), e01.into()])
            .unwrap();
        let view = store.view(&Selector::NewestFolders).unwrap();
        assert_eq!(view.paths, [PathBuf::from("/m/S2"), PathBuf::from("/m/S1")]);
        assert_eq!(view.shown_name(), "S2 +1");
        assert_eq!(view.files.len(), 4);
    }

    #[test]
    fn an_argument_on_disk_is_a_path_and_anything_else_a_run_name() {
        let dir = tempfile::tempdir().unwrap();
        let show = dir.path().join("Show");
        std::fs::create_dir(&show).unwrap();
        let arg = |p: &Path| p.to_string_lossy().into_owned();
        assert_eq!(Selector::parse(&[]).unwrap(), Selector::NewestFolders);
        assert_eq!(
            Selector::parse(&["2026-10-17T14-03-22".into()]).unwrap(),
            Selector::Run("2026-10-17T14-03-22".into())
        );
        assert_eq!(
            Selector::parse(&[arg(&show)]).unwrap(),
            Selector::Paths(vec![show.clone()])
        );
        let e = Selector::parse(&[arg(&show), "2026-10-17T14-03-22".into()]).unwrap_err();
        assert_eq!(
            e.to_string(),
            "2026-10-17T14-03-22: no such file or directory (a run name goes alone)"
        );
        let (_db, mut store) = store();
        assert_eq!(
            store.view(&Selector::Newest).unwrap_err().to_string(),
            "no runs yet"
        );
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[])
            .unwrap();
        assert_eq!(store.view(&Selector::Newest).unwrap().name, scan.name);
        assert_eq!(
            store.view(&Selector::Run(scan.name.clone())).unwrap().name,
            scan.name
        );
        assert_eq!(
            store
                .view(&Selector::Run("nope".into()))
                .unwrap_err()
                .to_string(),
            "no run named nope"
        );
    }

    #[test]
    fn a_files_view_is_that_file_alone_as_it_is_now() {
        let (_dir, mut store) = store();
        let (a, b) = ("/m/E01.mkv", "/m/E02.mkv");
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[a.into(), b.into()])
            .unwrap();
        let ids = checked(&mut store, &scan, a, &[("teh", "the"), ("adn", "and")]);
        checked(&mut store, &scan, b, &[("wiht", "with")]);
        store
            .decide(&[decision(ids[0], State::Accepted, None)])
            .unwrap();
        store
            .mark_applied(&[Applied {
                finding: ids[0],
                at: at("2026-10-17T12:00:00Z"),
            }])
            .unwrap();
        let view = store.file_view(a).unwrap();
        assert_eq!(paths(&view), [a]);
        assert_eq!(found(&view), [(ids[0], "teh"), (ids[1], "adn")]);
        assert_eq!(view.state_of(ids[0]), State::Accepted);
        assert_eq!(view.applied, [ids[0]].into());
        let nothing = store.file_view("/m/E03.mkv").unwrap();
        assert!(nothing.files.is_empty() && nothing.findings.is_empty());
    }
}

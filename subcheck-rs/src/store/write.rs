//! What scan, review and apply write: each scan and the files it names, each file's checks with
//! their findings, the decisions on those, applied marks, and LLM calls.

use std::path::PathBuf;

use jiff::Timestamp;
use rusqlite::types::Type;
use rusqlite::{ErrorCode, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{
    Applied, Check, CheckedWith, Decision, Finding, LlmCall, RunHeader, State, Store, TrackRecord,
};
use crate::llm::Usage;
use crate::mkv::Truncated;

/// A scan's row: its ID, its name (`2026-10-17T14-03-22`), and what it was made with.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanRow {
    pub id: i64,
    pub name: String,
    pub header: RunHeader,
}

impl Store {
    /// A new scan, named for the local time (`2026-10-17T14-03-22`, then `-2`, `-3`, ... for
    /// another started in the same second), with every file it names: skipped ones
    /// too, so its view covers them all. One transaction: no scan is left without its files.
    pub fn begin_scan(
        &mut self,
        header: &RunHeader,
        files: &[PathBuf],
    ) -> rusqlite::Result<ScanRow> {
        let tx = self.write()?;
        let base = jiff::Zoned::now().strftime("%Y-%m-%dT%H-%M-%S").to_string();
        let mut n = 1;
        let (id, name) = loop {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            if let Some(id) = insert_scan(&tx, &name, header)? {
                break (id, name);
            }
            n += 1;
        };
        add_files(&tx, id, files)?;
        tx.commit()?;
        Ok(ScanRow {
            id,
            name,
            header: header.clone(),
        })
    }

    /// Add files to scan `scan`'s (`scan --run`). One it names already keeps its place.
    pub fn add_files(&mut self, scan: i64, files: &[PathBuf]) -> rusqlite::Result<()> {
        let tx = self.write()?;
        add_files(&tx, scan, files)?;
        tx.commit()
    }

    /// The scan named `name`.
    pub fn scan_named(&self, name: &str) -> rusqlite::Result<Option<ScanRow>> {
        self.conn
            .query_row(
                &format!("SELECT {SCAN_COLUMNS} FROM scans WHERE name = ?1"),
                [name],
                scan_row,
            )
            .optional()
    }

    /// The scan that started last. Found by its start as a time: as text it would not sort, a
    /// second's fraction having as many digits as it needs.
    pub fn newest_scan(&self) -> rusqlite::Result<Option<ScanRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {SCAN_COLUMNS} FROM scans"))?;
        let scans = stmt
            .query_map([], scan_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(scans.into_iter().max_by_key(|s| (s.header.started, s.id)))
    }

    /// A transaction that takes the write lock as it begins, waiting up to the busy timeout for
    /// another writer's to end. A deferred one would take it at its first write, and could not
    /// wait then if it had read first.
    pub(super) fn write(&mut self) -> rusqlite::Result<Transaction<'_>> {
        self.conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
    }
}

/// Insert scan `name`, and return its ID; `None` when there is a scan of that name already.
pub(super) fn insert_scan(
    tx: &Transaction,
    name: &str,
    header: &RunHeader,
) -> rusqlite::Result<Option<i64>> {
    let made = tx.execute(
        "INSERT INTO scans(name, started, backend, langs, prompt_version, version, command)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(name) DO NOTHING",
        params![
            name,
            header.started.to_string(),
            header.backend,
            json(&header.langs)?,
            header.prompt_version,
            header.version,
            json(&header.command)?,
        ],
    )?;
    Ok((made == 1).then(|| tx.last_insert_rowid()))
}

/// Add `files` to scan `scan`'s, each once, in the order first named.
pub(super) fn add_files(tx: &Transaction, scan: i64, files: &[PathBuf]) -> rusqlite::Result<()> {
    let mut insert = tx.prepare_cached(
        "INSERT INTO scan_files(scan, path) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
    )?;
    for f in files {
        insert.execute(params![scan, f.to_string_lossy()])?;
    }
    Ok(())
}

const SCAN_COLUMNS: &str = "id, name, started, backend, langs, prompt_version, version, command";

fn scan_row(r: &Row) -> rusqlite::Result<ScanRow> {
    Ok(ScanRow {
        id: r.get(0)?,
        name: r.get(1)?,
        header: RunHeader {
            started: time(r, 2)?,
            backend: r.get(3)?,
            langs: from_json(r, 4)?,
            prompt_version: r.get(5)?,
            version: r.get(6)?,
            command: from_json(r, 7)?,
        },
    })
}

/// A nested value as its column holds it: JSON, by its serde type.
pub(super) fn json<T: Serialize + ?Sized>(value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// Column `i` of `r`, a nested value, read back by its serde type.
pub(super) fn from_json<T: DeserializeOwned>(r: &Row, i: usize) -> rusqlite::Result<T> {
    let text: String = r.get(i)?;
    serde_json::from_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, Type::Text, Box::new(e)))
}

/// Column `i` of `r`, a time.
pub(super) fn time(r: &Row, i: usize) -> rusqlite::Result<Timestamp> {
    let text: String = r.get(i)?;
    text.parse().map_err(|e: jiff::Error| {
        rusqlite::Error::FromSqlConversionFailure(i, Type::Text, Box::new(e))
    })
}

/// What a scan read of one file: what each check it saves of it, or its failed attempt, says of the
/// file.
#[derive(Debug, Clone, Copy)]
pub struct Attempt<'a> {
    pub scan: i64,
    pub path: &'a str,
    pub size: u64,
    pub mtime: Timestamp,
    pub tracks: &'a [TrackRecord],
    pub truncated: Option<Truncated>,
    pub at: Timestamp,
}

/// One check made of a file: what it was made with, the distinct lines it read for the LLM, and
/// its findings, whose IDs [`Store::save`] gives.
#[derive(Debug, Clone, PartialEq)]
pub struct Made {
    pub with: CheckedWith,
    pub lines: usize,
    pub findings: Vec<Finding>,
}

/// What [`Store::save`] wrote: the findings with their IDs, and the decisions carried to them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Saved {
    pub findings: Vec<Finding>,
    pub carried: Vec<Decision>,
}

impl Store {
    /// Whether the file's current check of `with`'s kind was made of the file as it is (`size`,
    /// `mtime`) with the same backend, languages and version, so a scan skips it. A check of a file
    /// shorter than its Segment says, or of one with a track that could not be read, is never
    /// fresh: every scan says so, or tries the track again, until the copy is replaced. Nor is a
    /// check that a later failed attempt of the same kind followed: the file's views show it as
    /// failed, so the next scan makes the check again, as scans did before the database.
    pub fn is_fresh(
        &self,
        path: &str,
        size: u64,
        mtime: Timestamp,
        with: &CheckedWith,
    ) -> rusqlite::Result<bool> {
        let fresh = self
            .conn
            .query_row(
                "SELECT size, mtime, backend, langs, version, truncated, tracks FROM checks c
                 WHERE path = ?1 AND kind = ?2 AND replaced_by IS NULL AND error IS NULL
                   AND NOT EXISTS (SELECT 1 FROM checks f WHERE f.path = c.path
                                   AND f.kind = c.kind AND f.error IS NOT NULL AND f.id > c.id)",
                params![path, check_text(with.check)],
                |r| {
                    let tracks: Vec<TrackRecord> = from_json(r, 6)?;
                    Ok(r.get::<_, u64>(0)? == size
                        && time(r, 1)? == mtime
                        && r.get::<_, String>(2)? == with.backend
                        && from_json::<Vec<String>>(r, 3)? == with.langs
                        && r.get::<_, u32>(4)? == with.prompt_version
                        && r.get::<_, Option<String>>(5)?.is_none()
                        && !tracks.iter().any(|t| t.read.starts_with("skipped: ")))
                },
            )
            .optional()?;
        Ok(fresh == Some(true))
    }

    /// Save the checks a scan made of one file, in one transaction: each replaces the file's
    /// current check of its kind, whatever made that, and the decisions on the replaced check's
    /// findings carry over to the new ones of the same line and fix ([`carry`]). Returns the
    /// findings with their IDs, and the decisions carried.
    pub fn save(&mut self, a: &Attempt, made: Vec<Made>) -> rusqlite::Result<Saved> {
        let tx = self.write()?;
        let mut saved = Saved::default();
        for m in made {
            let (id, old) = insert_check(&tx, a, &m.with, m.lines, None)?;
            for f in m.findings {
                let finding = insert_finding(&tx, id, &f)?;
                saved.findings.push(Finding { id: finding, ..f });
            }
            if let Some(old) = old {
                saved.carried.extend(carry(&tx, old, id)?);
            }
        }
        tx.commit()?;
        Ok(saved)
    }

    /// A failed attempt at the checks `tried`, each with the lines read for it: recorded, and
    /// replacing nothing, so the file's earlier findings stay current. A failed attempt is never a
    /// current check, but it makes the earlier current check of its kind stale ([`Store::is_fresh`]),
    /// so the next scan tries again; the other kinds' checks stay fresh.
    pub fn fail(
        &mut self,
        a: &Attempt,
        tried: &[(CheckedWith, usize)],
        error: &str,
    ) -> rusqlite::Result<()> {
        let tx = self.write()?;
        for (with, lines) in tried {
            insert_check(&tx, a, with, *lines, Some(error))?;
        }
        tx.commit()
    }

    /// Decisions made in review, in one transaction. Each is a row, the latest a finding's current
    /// one: an undo, or `x`, is a row too.
    pub fn decide(&mut self, decisions: &[Decision]) -> rusqlite::Result<()> {
        let tx = self.write()?;
        for d in decisions {
            insert_decision(&tx, d)?;
        }
        tx.commit()
    }

    /// Fixes apply made in their files, or found there.
    pub fn mark_applied(&mut self, applied: &[Applied]) -> rusqlite::Result<()> {
        let tx = self.write()?;
        insert_applied(&tx, applied)?;
        tx.commit()
    }

    /// LLM calls scan `scan` started, as their answers come: retries and failed ones too.
    pub fn add_usage(&mut self, scan: i64, calls: &[LlmCall]) -> rusqlite::Result<()> {
        let tx = self.write()?;
        insert_calls(&tx, scan, calls)?;
        tx.commit()
    }

    /// Scan `scan`'s LLM calls, in the order they were recorded.
    pub fn usage(&self, scan: i64) -> rusqlite::Result<Vec<LlmCall>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CALL_COLUMNS} FROM usage WHERE scan = ?1 ORDER BY id"
        ))?;
        stmt.query_map([scan], call)?.collect()
    }
}

/// Insert a check of the file `a` read, and return its ID and that of the current check it
/// replaced. A failed one (`error`) replaces nothing.
///
/// The partial unique index allows one current check of a kind per file at any moment, and SQLite
/// checks it at each statement, so the new row cannot go in while the old one is current. The old
/// one is pointed at the new one's ID first: one past the highest, which is the new one's, as the
/// transaction holds the write lock. The deferred foreign key lets it point there until the commit.
pub(super) fn insert_check(
    tx: &Transaction,
    a: &Attempt,
    with: &CheckedWith,
    lines: usize,
    error: Option<&str>,
) -> rusqlite::Result<(i64, Option<i64>)> {
    let id: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM checks", [], |r| {
        r.get(0)
    })?;
    let kind = check_text(with.check);
    let old = if error.is_none() {
        tx.query_row(
            "UPDATE checks SET replaced_by = ?1
             WHERE path = ?2 AND kind = ?3 AND replaced_by IS NULL AND error IS NULL
             RETURNING id",
            params![id, a.path, kind],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    tx.execute(
        "INSERT INTO checks(id, scan, path, kind, size, mtime, backend, langs, version, lines,
             tracks, truncated, error, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            a.scan,
            a.path,
            kind,
            a.size,
            a.mtime.to_string(),
            with.backend,
            json(&with.langs)?,
            with.prompt_version,
            lines,
            json(a.tracks)?,
            a.truncated.as_ref().map(json).transpose()?,
            error,
            a.at.to_string(),
        ],
    )?;
    Ok((id, old))
}

/// Insert finding `f` under check `check`, and return its ID: global, never used again.
pub(super) fn insert_finding(tx: &Transaction, check: i64, f: &Finding) -> rusqlite::Result<u64> {
    tx.prepare_cached(
        r#"INSERT INTO findings("check", path, style, original, suggestion, reason, source,
               occurrences, "before", "after", timing)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) RETURNING id"#,
    )?
    .query_row(
        params![
            check,
            f.file,
            f.style,
            f.original,
            f.suggestion,
            f.reason,
            f.source,
            json(&f.occurrences)?,
            json(&f.before)?,
            json(&f.after)?,
            f.timing.as_ref().map(json).transpose()?,
        ],
        |r| r.get(0),
    )
}

/// Carry decisions over from check `old` to check `new`, which replaced it: each finding of `new`
/// takes the current decision of `old`'s finding of the same line and fix (path, original and
/// suggestion; and a timing finding's fix, which names its track), with its wording and time.
/// Never over a decision the finding has; a cleared one (pending) is none to carry; of two that
/// match, the one decided last. Returns the decisions carried.
pub(super) fn carry(tx: &Transaction, old: i64, new: i64) -> rusqlite::Result<Vec<Decision>> {
    let mut stmt = tx.prepare_cached(
        r#"SELECT n.id, d.state, d.edited, d.at
           FROM findings n
           JOIN findings o ON o."check" = ?1 AND o.path = n.path AND o.original = n.original
               AND o.suggestion = n.suggestion AND o.timing IS n.timing
           JOIN decisions d ON d.id = (SELECT MAX(id) FROM decisions WHERE finding = o.id)
           WHERE n."check" = ?2 AND d.state != 'pending'
               AND NOT EXISTS (SELECT 1 FROM decisions WHERE finding = n.id)
           ORDER BY n.id, d.id"#,
    )?;
    let found = stmt
        .query_map(params![old, new], |r| {
            Ok(Decision {
                finding: r.get(0)?,
                state: state_of(r, 1)?,
                edited: r.get(2)?,
                at: time(r, 3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut carried: Vec<Decision> = Vec::new();
    for d in found {
        match carried.last_mut() {
            Some(last) if last.finding == d.finding => *last = d,
            _ => carried.push(d),
        }
    }
    for d in &carried {
        insert_decision(tx, d)?;
    }
    Ok(carried)
}

pub(super) fn insert_decision(tx: &Transaction, d: &Decision) -> rusqlite::Result<()> {
    tx.prepare_cached("INSERT INTO decisions(finding, state, edited, at) VALUES (?1, ?2, ?3, ?4)")?
        .execute(params![
            d.finding,
            state_text(d.state),
            d.edited,
            d.at.to_string()
        ])?;
    Ok(())
}

pub(super) fn insert_applied(tx: &Transaction, applied: &[Applied]) -> rusqlite::Result<()> {
    let mut insert = tx.prepare_cached("INSERT INTO applied(finding, at) VALUES (?1, ?2)")?;
    for a in applied {
        insert.execute(params![a.finding, a.at.to_string()])?;
    }
    Ok(())
}

/// Insert `calls` under scan `scan`: no numbers for a call that reported none.
pub(super) fn insert_calls(tx: &Transaction, scan: i64, calls: &[LlmCall]) -> rusqlite::Result<()> {
    let mut insert = tx.prepare_cached(
        "INSERT INTO usage(scan, path, lines, input_tokens, output_tokens, cache_read_tokens,
             cache_write_tokens, cost_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for c in calls {
        let u = c.usage.as_ref();
        insert.execute(params![
            scan,
            c.file,
            c.lines,
            u.map(|u| u.input_tokens),
            u.map(|u| u.output_tokens),
            u.and_then(|u| u.cache_read_tokens),
            u.and_then(|u| u.cache_write_tokens),
            u.and_then(|u| u.cost_usd),
        ])?;
    }
    Ok(())
}

/// Whether a write failed for another writer holding the lock past the busy timeout.
pub fn is_busy(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
}

pub(super) const CALL_COLUMNS: &str = "path, lines, input_tokens, output_tokens, cache_read_tokens, \
                                       cache_write_tokens, cost_usd";

/// A `usage` row, read by [`CALL_COLUMNS`]: no numbers when the call reported none.
pub(super) fn call(r: &Row) -> rusqlite::Result<LlmCall> {
    let usage = match (r.get::<_, Option<u64>>(2)?, r.get::<_, Option<u64>>(3)?) {
        (Some(input_tokens), Some(output_tokens)) => Some(Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens: r.get(4)?,
            cache_write_tokens: r.get(5)?,
            cost_usd: r.get(6)?,
        }),
        _ => None,
    };
    Ok(LlmCall {
        file: r.get(0)?,
        lines: r.get(1)?,
        usage,
    })
}

pub(super) fn check_text(c: Check) -> &'static str {
    match c {
        Check::Llm => "llm",
        Check::Timing => "timing",
    }
}

fn state_text(s: State) -> &'static str {
    match s {
        State::Pending => "pending",
        State::Accepted => "accepted",
        State::Rejected => "rejected",
    }
}

pub(super) fn state_of(r: &Row, i: usize) -> rusqlite::Result<State> {
    let text: String = r.get(i)?;
    match text.as_str() {
        "pending" => Ok(State::Pending),
        "accepted" => Ok(State::Accepted),
        "rejected" => Ok(State::Rejected),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            i,
            Type::Text,
            format!("no decision is called {other}").into(),
        )),
    }
}

/// Its helpers make the views' and the import's test data too.
#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::store) fn header(started: &str) -> RunHeader {
        RunHeader {
            started: started.parse().unwrap(),
            backend: "claude".into(),
            command: vec!["claude".into(), "-p".into()],
            prompt_version: 1,
            version: "0.1.0".into(),
            langs: vec!["eng".into()],
        }
    }

    pub(in crate::store) fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(&dir.path().join("subcheck.db")).unwrap();
        (dir, store)
    }

    fn files_of(store: &Store, scan: i64) -> Vec<String> {
        let mut stmt = store
            .conn
            .prepare("SELECT path FROM scan_files WHERE scan = ?1 ORDER BY rowid")
            .unwrap();
        stmt.query_map([scan], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn a_scan_is_named_for_its_start_and_keeps_the_files_it_names_in_order_once_each() {
        let (_dir, mut store) = store();
        let h = header("2026-10-17T10:00:00Z");
        let a = store
            .begin_scan(&h, &["/m/b.mkv".into(), "/m/a.mkv".into()])
            .unwrap();
        let b = store.begin_scan(&h, &[]).unwrap();
        // Started in the same second, the second gets a number; a second apart, its own name.
        assert_ne!(a.name, b.name);
        assert_eq!(a.name.len(), "2026-10-17T14-03-22".len());
        assert_eq!(store.scan_named(&a.name).unwrap(), Some(a.clone()));
        assert_eq!(store.scan_named("nope").unwrap(), None);
        store
            .add_files(a.id, &["/m/a.mkv".into(), "/m/c.mkv".into()])
            .unwrap();
        assert_eq!(files_of(&store, a.id), ["/m/b.mkv", "/m/a.mkv", "/m/c.mkv"]);
        assert_eq!(files_of(&store, b.id).len(), 0);
    }

    #[test]
    fn the_newest_scan_is_the_one_that_started_last() {
        let (_dir, mut store) = store();
        assert_eq!(store.newest_scan().unwrap(), None);
        // Half a second later, though as text it sorts first ('.' before 'Z').
        let later = store
            .begin_scan(&header("2026-10-17T10:00:00.5Z"), &[])
            .unwrap();
        store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[])
            .unwrap();
        assert_eq!(store.newest_scan().unwrap(), Some(later));
    }

    pub(in crate::store) fn llm() -> CheckedWith {
        CheckedWith::new("claude", &["eng".into()], 1)
    }

    pub(in crate::store) fn at(t: &str) -> Timestamp {
        t.parse().unwrap()
    }

    /// A typo finding in `path`, its ID for the store to give.
    pub(in crate::store) fn typo(path: &str, original: &str, suggestion: &str) -> Finding {
        Finding {
            id: 0,
            file: path.into(),
            style: "Dialogue".into(),
            original: original.into(),
            suggestion: suggestion.into(),
            reason: "typo".into(),
            source: "llm".into(),
            occurrences: vec![crate::text::Occurrence {
                track: 2,
                start_ms: 1000,
                end_ms: 2000,
            }],
            before: vec![],
            after: vec![],
            timing: None,
        }
    }

    pub(in crate::store) fn track(read: &str) -> TrackRecord {
        TrackRecord {
            id: 2,
            name: Some("English".into()),
            language: "eng".into(),
            language_bcp47: None,
            sid: 1,
            default: true,
            read: read.into(),
        }
    }

    /// A check of `path` (size 10, mtime 10:00) by scan `scan`, as the scan read it at `when`.
    pub(in crate::store) fn attempt<'a>(
        scan: i64,
        path: &'a str,
        tracks: &'a [TrackRecord],
        when: &str,
    ) -> Attempt<'a> {
        Attempt {
            scan,
            path,
            size: 10,
            mtime: at("2026-10-17T10:00:00Z"),
            tracks,
            truncated: None,
            at: at(when),
        }
    }

    pub(in crate::store) fn decision(finding: u64, state: State, edited: Option<&str>) -> Decision {
        Decision {
            finding,
            state,
            edited: edited.map(Into::into),
            at: at("2026-10-17T11:00:00Z"),
        }
    }

    /// Each check of `path`, of any kind, oldest first: (ID, replaced by, error).
    fn checks_of(store: &Store, path: &str) -> Vec<(i64, Option<i64>, Option<String>)> {
        let mut stmt = store
            .conn
            .prepare("SELECT id, replaced_by, error FROM checks WHERE path = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map([path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Finding `id`'s current decision: its latest row.
    fn current(store: &Store, id: u64) -> Option<(State, Option<String>)> {
        store
            .conn
            .query_row(
                "SELECT state, edited FROM decisions WHERE finding = ?1 ORDER BY id DESC LIMIT 1",
                [id],
                |r| Ok((state_of(r, 0)?, r.get(1)?)),
            )
            .optional()
            .unwrap()
    }

    const A: &str = "/m/Show/E01.mkv";

    #[test]
    fn a_check_made_again_replaces_the_current_one_and_its_decisions_carry_over() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        let first = store
            .save(
                &attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z"),
                vec![Made {
                    with: llm(),
                    lines: 4,
                    findings: vec![
                        typo(A, "leave befor it", "leave before it"),
                        typo(A, "go trough it", "go through it"),
                        typo(A, "Ihave to", "I have to"),
                    ],
                }],
            )
            .unwrap();
        let ids: Vec<u64> = first.findings.iter().map(|f| f.id).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert_eq!(first.carried.len(), 0);
        // Reworded and accepted; rejected; accepted, then cleared.
        store
            .decide(&[
                decision(1, State::Accepted, Some("leave before it!")),
                decision(2, State::Rejected, None),
                decision(3, State::Accepted, None),
                decision(3, State::Pending, None),
            ])
            .unwrap();
        let again = store
            .save(
                &attempt(scan.id, A, &tracks, "2026-10-17T12:00:00Z"),
                vec![Made {
                    with: llm(),
                    lines: 4,
                    findings: vec![
                        typo(A, "Ihave to", "I have to"),
                        typo(A, "go trough it", "go through it"),
                        // The same line, another fix: a finding of its own.
                        typo(A, "leave befor it", "leave beforehand"),
                        typo(A, "leave befor it", "leave before it"),
                    ],
                }],
            )
            .unwrap();
        let ids: Vec<u64> = again.findings.iter().map(|f| f.id).collect();
        assert_eq!(ids, [4, 5, 6, 7]);
        assert_eq!(
            again.carried,
            [
                Decision {
                    finding: 5,
                    ..decision(2, State::Rejected, None)
                },
                Decision {
                    finding: 7,
                    ..decision(1, State::Accepted, Some("leave before it!"))
                },
            ]
        );
        assert_eq!(current(&store, 4), None);
        assert_eq!(current(&store, 6), None);
        assert_eq!(
            current(&store, 7),
            Some((State::Accepted, Some("leave before it!".into())))
        );
        // The first check stays, as history, replaced by the second.
        let checks = checks_of(&store, A);
        assert_eq!(checks, [(1, Some(2), None), (2, None, None)]);
    }

    #[test]
    fn a_carried_decision_never_overrides_one_the_finding_has() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        let a = attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z");
        store
            .save(
                &a,
                vec![Made {
                    with: llm(),
                    lines: 1,
                    findings: vec![typo(A, "go trough it", "go through it")],
                }],
            )
            .unwrap();
        store.decide(&[decision(1, State::Accepted, None)]).unwrap();
        // As the import makes it: the new check and its finding, the finding's own decision, then
        // the carry.
        let tx = store.write().unwrap();
        let (new, old) = insert_check(&tx, &a, &llm(), 1, None).unwrap();
        let finding = insert_finding(&tx, new, &typo(A, "go trough it", "go through it")).unwrap();
        insert_decision(&tx, &decision(finding, State::Rejected, None)).unwrap();
        assert_eq!(carry(&tx, old.unwrap(), new).unwrap(), []);
        tx.commit().unwrap();
        assert_eq!(current(&store, finding), Some((State::Rejected, None)));
    }

    #[test]
    fn a_timing_finding_takes_the_decision_of_the_same_fix_on_its_own_track_only() {
        use crate::store::{Basis, FixStretch, TimingFix};
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        let late = |track, frames| Finding {
            style: String::new(),
            original: "1 frame late".into(),
            suggestion: "shift 1 frame earlier".into(),
            source: "timing".into(),
            timing: Some(TimingFix {
                track,
                frame_ns: 41_708_333,
                stretches: vec![FixStretch {
                    from_ms: None,
                    to_ms: None,
                    frames,
                    first: None,
                    last: None,
                    frame_ms: Some(1000),
                }],
                unshifted: vec![],
                basis: Basis::Cuts {
                    times: 85,
                    agreeing: 2,
                    windows: 2,
                },
            }),
            ..typo(A, "", "")
        };
        let timing = CheckedWith::timing(&["eng".into()], 1);
        let save = |store: &mut Store, findings| {
            store
                .save(
                    &attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z"),
                    vec![Made {
                        with: timing.clone(),
                        lines: 0,
                        findings,
                    }],
                )
                .unwrap()
        };
        save(&mut store, vec![late(2, 1), late(4, 1)]);
        store
            .decide(&[
                decision(1, State::Accepted, None),
                decision(2, State::Rejected, None),
            ])
            .unwrap();
        // Each track's finding takes its own track's decision, never the other's.
        let again = save(&mut store, vec![late(4, 1), late(2, 1)]);
        let carried: Vec<(u64, State)> =
            again.carried.iter().map(|d| (d.finding, d.state)).collect();
        assert_eq!(carried, [(3, State::Rejected), (4, State::Accepted)]);
        // Another fix of the same track is another finding.
        let other = save(&mut store, vec![late(2, 2)]);
        assert_eq!(other.carried, []);
    }

    #[test]
    fn a_failed_attempt_replaces_nothing_and_is_retried_until_a_save_succeeds() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        let a = attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z");
        let timing = CheckedWith::timing(&["eng".into()], 1);
        let both = || {
            vec![
                Made {
                    with: llm(),
                    lines: 4,
                    findings: vec![typo(A, "go trough it", "go through it")],
                },
                Made {
                    with: CheckedWith::timing(&["eng".into()], 1),
                    lines: 4,
                    findings: vec![],
                },
            ]
        };
        store.save(&a, both()).unwrap();
        assert!(store.is_fresh(A, 10, a.mtime, &llm()).unwrap());
        assert!(store.is_fresh(A, 10, a.mtime, &timing).unwrap());
        store.fail(&a, &[(llm(), 4)], "LLM: boom").unwrap();
        let checks = checks_of(&store, A);
        assert_eq!(
            checks,
            [
                (1, None, None),
                (2, None, None),
                (3, None, Some("LLM: boom".into())),
            ]
        );
        // The failed LLM attempt is newer than the LLM check, so a scan makes it again; the
        // timing check has no failed attempt of its own and stays fresh.
        assert!(!store.is_fresh(A, 10, a.mtime, &llm()).unwrap());
        assert!(store.is_fresh(A, 10, a.mtime, &timing).unwrap());
        // A later save of that kind is newer than the failure.
        store.save(&a, both()).unwrap();
        assert!(store.is_fresh(A, 10, a.mtime, &llm()).unwrap());
        assert!(store.is_fresh(A, 10, a.mtime, &timing).unwrap());
    }

    #[test]
    fn saving_one_kind_of_check_leaves_the_current_check_of_another_kind_and_its_decisions() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        let a = attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z");
        // An LLM-style finding under a timing check, on purpose: with no kind guard in the carry,
        // the LLM check saved next would take its decision from it.
        store
            .save(
                &a,
                vec![Made {
                    with: CheckedWith::timing(&["eng".into()], 1),
                    lines: 0,
                    findings: vec![typo(A, "go trough it", "go through it")],
                }],
            )
            .unwrap();
        store.decide(&[decision(1, State::Accepted, None)]).unwrap();
        let saved = store
            .save(
                &a,
                vec![Made {
                    with: llm(),
                    lines: 4,
                    findings: vec![typo(A, "go trough it", "go through it")],
                }],
            )
            .unwrap();
        assert_eq!(saved.carried, []);
        assert_eq!(checks_of(&store, A), [(1, None, None), (2, None, None)]);
        assert_eq!(current(&store, 1), Some((State::Accepted, None)));
        assert_eq!(current(&store, 2), None);
    }

    #[test]
    fn a_failed_attempt_between_two_saves_is_not_replaced_and_the_old_size_is_stale() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        let a = attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z");
        let made = || {
            vec![Made {
                with: llm(),
                lines: 4,
                findings: vec![typo(A, "go trough it", "go through it")],
            }]
        };
        store.save(&a, made()).unwrap();
        store.decide(&[decision(1, State::Accepted, None)]).unwrap();
        store.fail(&a, &[(llm(), 4)], "LLM: boom").unwrap();
        let bigger = Attempt { size: 11, ..a };
        let saved = store.save(&bigger, made()).unwrap();
        assert_eq!(
            checks_of(&store, A),
            [
                (1, Some(3), None),
                (2, None, Some("LLM: boom".into())),
                (3, None, None),
            ]
        );
        let carried: Vec<(u64, State)> =
            saved.carried.iter().map(|d| (d.finding, d.state)).collect();
        assert_eq!(carried, [(2, State::Accepted)]);
        assert!(store.is_fresh(A, 11, a.mtime, &llm()).unwrap());
        assert!(!store.is_fresh(A, 10, a.mtime, &llm()).unwrap());
    }

    #[test]
    fn of_findings_with_one_carry_key_the_one_decided_last_carries_unless_it_was_cleared() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        let a = attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z");
        let twins = || {
            vec![Made {
                with: llm(),
                lines: 4,
                findings: vec![
                    typo(A, "go trough it", "go through it"),
                    typo(A, "go trough it", "go through it"),
                ],
            }]
        };
        store.save(&a, twins()).unwrap();
        store
            .decide(&[
                decision(1, State::Accepted, None),
                decision(2, State::Rejected, None),
            ])
            .unwrap();
        // One decision for each new finding, the later-decided one.
        let again = store.save(&a, twins()).unwrap();
        let carried: Vec<(u64, State)> =
            again.carried.iter().map(|d| (d.finding, d.state)).collect();
        assert_eq!(carried, [(3, State::Rejected), (4, State::Rejected)]);
        // A later decision cleared back to pending is none to carry: the earlier one is.
        store
            .decide(&[
                decision(3, State::Accepted, None),
                decision(4, State::Rejected, None),
                decision(4, State::Pending, None),
            ])
            .unwrap();
        let last = store.save(&a, twins()).unwrap();
        let carried: Vec<(u64, State)> =
            last.carried.iter().map(|d| (d.finding, d.state)).collect();
        assert_eq!(carried, [(5, State::Accepted), (6, State::Accepted)]);
    }

    #[test]
    fn a_check_is_fresh_for_the_file_as_it_is_and_what_checked_it() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[])
            .unwrap();
        let cues = [track("cues")];
        let save = |store: &mut Store, path: &str, tracks: &[TrackRecord], truncated| {
            let a = Attempt {
                truncated,
                ..attempt(scan.id, path, tracks, "2026-10-17T10:01:00Z")
            };
            store
                .save(
                    &a,
                    vec![Made {
                        with: llm(),
                        lines: 4,
                        findings: vec![],
                    }],
                )
                .unwrap();
        };
        save(&mut store, A, &cues, None);
        let mtime = at("2026-10-17T10:00:00Z");
        let fresh = |store: &Store, path, size, mtime, with: &CheckedWith| {
            store.is_fresh(path, size, mtime, with).unwrap()
        };
        assert!(fresh(&store, A, 10, mtime, &llm()));
        assert!(!fresh(&store, A, 11, mtime, &llm()));
        assert!(!fresh(&store, A, 10, at("2026-10-17T10:00:01Z"), &llm()));
        assert!(!fresh(&store, "/m/Show/E02.mkv", 10, mtime, &llm()));
        // Another backend, other languages, or a new prompt is another check.
        let gpt = CheckedWith::new("gpt", &["eng".into()], 1);
        let und = CheckedWith::new("claude", &["und".into()], 1);
        let v2 = CheckedWith::new("claude", &["eng".into()], 2);
        for with in [&gpt, &und, &v2] {
            assert!(!fresh(&store, A, 10, mtime, with), "{with:?}");
        }
        // Languages are a set; and the timing check is a check of its own.
        let repeated = CheckedWith::new("claude", &["eng".into(), "eng".into()], 1);
        assert!(fresh(&store, A, 10, mtime, &repeated));
        let timing = CheckedWith::timing(&["eng".into()], 1);
        assert!(!fresh(&store, A, 10, mtime, &timing));
        // A partial copy, or a track that could not be read: checked on every scan.
        let cut = "/m/Show/cut.mkv";
        let truncated = Truncated {
            declared_end: 20,
            file_size: 10,
        };
        save(&mut store, cut, &cues, Some(truncated));
        assert!(!fresh(&store, cut, 10, mtime, &llm()));
        let lost = "/m/Show/lost.mkv";
        save(
            &mut store,
            lost,
            &[track("skipped: unsupported: bzlib")],
            None,
        );
        assert!(!fresh(&store, lost, 10, mtime, &llm()));
    }

    #[test]
    fn llm_calls_read_back_with_their_numbers_or_none() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[])
            .unwrap();
        let calls = [
            LlmCall {
                file: A.into(),
                lines: 4,
                usage: Some(Usage {
                    input_tokens: 1203,
                    output_tokens: 40,
                    cache_read_tokens: Some(200),
                    cache_write_tokens: Some(1000),
                    cost_usd: Some(0.0125),
                }),
            },
            LlmCall {
                file: A.into(),
                lines: 4,
                usage: Some(Usage {
                    input_tokens: 1500,
                    output_tokens: 40,
                    ..Usage::default()
                }),
            },
            LlmCall {
                file: A.into(),
                lines: 4,
                usage: None,
            },
        ];
        store.add_usage(scan.id, &calls).unwrap();
        assert_eq!(store.usage(scan.id).unwrap(), calls);
        assert_eq!(store.usage(scan.id + 1).unwrap(), []);
    }

    #[test]
    fn two_writers_wait_for_each_other_within_the_busy_timeout() {
        let (_dir, mut store) = store();
        let scan = store
            .begin_scan(&header("2026-10-17T10:00:00Z"), &[A.into()])
            .unwrap();
        let tracks = [track("cues")];
        store
            .save(
                &attempt(scan.id, A, &tracks, "2026-10-17T10:01:00Z"),
                vec![Made {
                    with: llm(),
                    lines: 1,
                    findings: vec![typo(A, "go trough it", "go through it")],
                }],
            )
            .unwrap();
        let mut other = store.reopen().unwrap();
        let held = store.write().unwrap();
        let start = std::time::Instant::now();
        let waited = std::thread::spawn(move || {
            let done = other.decide(&[decision(1, State::Accepted, None)]);
            (done, start.elapsed(), other)
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        held.commit().unwrap();
        let (done, waited_for, mut other) = waited.join().unwrap();
        done.unwrap();
        assert!(
            waited_for >= std::time::Duration::from_millis(250),
            "{waited_for:?}"
        );
        assert_eq!(current(&store, 1), Some((State::Accepted, None)));
        // Past the timeout, the write fails as busy.
        other
            .conn
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let held = store.write().unwrap();
        let e = other
            .decide(&[decision(1, State::Rejected, None)])
            .unwrap_err();
        assert!(is_busy(&e), "{e}");
        drop(held);
    }
}

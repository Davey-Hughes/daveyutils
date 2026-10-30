//! The database: where it is, how it is opened, and its schema.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use rusqlite::{Connection, TransactionBehavior};

use super::state_dir_from;

/// The schema this subcheck makes and reads, kept in the database's `user_version`. A later
/// change is a migration keyed on the number.
pub const SCHEMA_VERSION: i64 = 1;

/// How long a writer waits for another's transaction before it gives up.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Version 1. A nested value is JSON, written by the serde type the rest of subcheck has for it;
/// a time is a `jiff::Timestamp` as text. `check`, `before` and `after` are quoted because SQLite
/// reads them as keywords: `CHECK` starts a constraint, `BEFORE` and `AFTER` belong to triggers.
///
/// A check's `replaced_by` is deferred to the commit: a check made again points the current one at
/// the new one's ID before the new row exists, since the partial unique index allows only one
/// current check of a kind per file at a time (see `write::insert_check`).
const SCHEMA: &str = r#"
CREATE TABLE scans (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    started TEXT NOT NULL,
    backend TEXT NOT NULL,
    langs TEXT NOT NULL,
    prompt_version INTEGER NOT NULL,
    version TEXT NOT NULL,
    command TEXT NOT NULL
);
CREATE TABLE scan_files (
    scan INTEGER NOT NULL REFERENCES scans(id),
    path TEXT NOT NULL,
    PRIMARY KEY (scan, path)
);
CREATE TABLE checks (
    id INTEGER PRIMARY KEY,
    scan INTEGER NOT NULL REFERENCES scans(id),
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('llm', 'timing')),
    size INTEGER NOT NULL,
    mtime TEXT NOT NULL,
    backend TEXT NOT NULL,
    langs TEXT NOT NULL,
    version INTEGER NOT NULL,
    lines INTEGER NOT NULL,
    tracks TEXT NOT NULL,
    truncated TEXT,
    error TEXT,
    at TEXT NOT NULL,
    replaced_by INTEGER REFERENCES checks(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE UNIQUE INDEX checks_current ON checks(path, kind)
    WHERE replaced_by IS NULL AND error IS NULL;
CREATE INDEX checks_path ON checks(path);
CREATE TABLE findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    "check" INTEGER NOT NULL REFERENCES checks(id),
    path TEXT NOT NULL,
    style TEXT NOT NULL,
    original TEXT NOT NULL,
    suggestion TEXT NOT NULL,
    reason TEXT NOT NULL,
    source TEXT NOT NULL,
    occurrences TEXT NOT NULL,
    "before" TEXT NOT NULL,
    "after" TEXT NOT NULL,
    timing TEXT
);
CREATE INDEX findings_check ON findings("check");
CREATE TABLE decisions (
    id INTEGER PRIMARY KEY,
    finding INTEGER NOT NULL REFERENCES findings(id),
    state TEXT NOT NULL CHECK (state IN ('pending', 'accepted', 'rejected')),
    edited TEXT,
    at TEXT NOT NULL
);
CREATE INDEX decisions_finding ON decisions(finding);
CREATE TABLE applied (
    finding INTEGER NOT NULL REFERENCES findings(id),
    at TEXT NOT NULL
);
CREATE INDEX applied_finding ON applied(finding);
CREATE TABLE usage (
    id INTEGER PRIMARY KEY,
    scan INTEGER NOT NULL REFERENCES scans(id),
    path TEXT NOT NULL,
    lines INTEGER NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    cost_usd REAL
);
CREATE INDEX usage_scan ON usage(scan);
"#;

/// Where the database is, and whether `SUBCHECK_DB` said so: an error opening it names both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub from_env: bool,
}

impl Location {
    /// The path as an error names it.
    fn named(&self) -> String {
        if self.from_env {
            format!("{} (from SUBCHECK_DB)", self.path.display())
        } else {
            self.path.display().to_string()
        }
    }
}

/// Where the database is, from explicit inputs: `SUBCHECK_DB` (`db`) when it is set and not
/// empty, which must be absolute, as a relative one would resolve against the cwd; else
/// `subcheck.db` in the state directory ([`state_dir_from`]). `HOME` is needed only for that.
pub fn location_from(
    db: Option<&Path>,
    home: Option<&Path>,
    xdg_state: Option<&Path>,
    macos: bool,
) -> Result<Location, String> {
    if let Some(db) = db.filter(|p| !p.as_os_str().is_empty()) {
        if !db.is_absolute() {
            return Err("SUBCHECK_DB must be an absolute path".into());
        }
        return Ok(Location {
            path: db.to_path_buf(),
            from_env: true,
        });
    }
    let home = home
        .filter(|h| h.is_absolute())
        .ok_or("HOME is not set to an absolute path")?;
    Ok(Location {
        path: state_dir_from(home, xdg_state, macos).join("subcheck.db"),
        from_env: false,
    })
}

/// Where this process's database is.
pub fn location() -> Result<Location, String> {
    let var = |name| std::env::var_os(name).map(PathBuf::from);
    location_from(
        var("SUBCHECK_DB").as_deref(),
        var("HOME").as_deref(),
        var("XDG_STATE_HOME").as_deref(),
        cfg!(target_os = "macos"),
    )
}

/// The database, open. One per thread that writes: a scan's thread has its own (see
/// [`Store::reopen`]), and SQLite's locks keep the writers apart.
pub struct Store {
    pub(super) conn: Connection,
    path: PathBuf,
    /// The path came from `SUBCHECK_DB`, which errors on it say.
    from_env: bool,
}

impl Store {
    /// This process's database ([`location`]), made if there is none yet: in the state directory,
    /// with the directory; a `SUBCHECK_DB` whose directory is missing is an error. The error names
    /// the path, and says when it came from `SUBCHECK_DB`.
    pub fn open() -> anyhow::Result<Store> {
        Store::open_location(&location().map_err(anyhow::Error::msg)?)
    }

    /// [`Store::open`] once the location is known.
    pub(crate) fn open_location(at: &Location) -> anyhow::Result<Store> {
        if !at.from_env
            && let Some(dir) = at.path.parent()
        {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut store =
            Store::open_at(&at.path).with_context(|| format!("opening {}", at.named()))?;
        store.from_env = at.from_env;
        Ok(store)
    }

    /// The database at `path`, made at [`SCHEMA_VERSION`] if it is new. WAL, so readers never
    /// wait for a writer; a writer waits [`BUSY_TIMEOUT`] for another; foreign keys enforced.
    pub fn open_at(path: &Path) -> anyhow::Result<Store> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // A plain read first: WAL is stored in the file, so another program's database must be
        // refused before it is switched. Subcheck commits its tables and `user_version` together,
        // so this never sees subcheck's own half-made schema; `make_schema` checks again under
        // its lock for two first opens at once.
        // Both reads are in one transaction, or a first open could commit between them.
        let read = conn.unchecked_transaction()?;
        let version: i64 = read.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 && Store::has_tables(&read)? {
            bail!("not a subcheck database");
        }
        drop(read);
        // Switching a new file to WAL fails at once, not after the busy timeout, when another
        // connection is doing the same, so it is retried here for as long as one would wait.
        let began = Instant::now();
        loop {
            match conn.pragma_update(None, "journal_mode", "WAL") {
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy
                        && began.elapsed() < BUSY_TIMEOUT =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                done => break done?,
            }
        }
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                // In one transaction, so a database is either made whole or left as it was: two
                // subchecks making it at once cannot both make the tables, and the one that
                // loses finds the other's schema in place.
                let foreign = Store::make_schema(&mut conn).or_else(|e| {
                    // The other one made it meanwhile.
                    let now: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
                    if now == SCHEMA_VERSION {
                        Ok(false)
                    } else {
                        Err(e)
                    }
                })?;
                if foreign {
                    bail!("not a subcheck database");
                }
            }
            SCHEMA_VERSION => {}
            v if v > SCHEMA_VERSION => bail!("subcheck.db was made by a later subcheck"),
            v => bail!("subcheck.db has schema version {v}, which no subcheck made"),
        }
        Ok(Store {
            conn,
            path: path.to_path_buf(),
            from_env: false,
        })
    }

    /// Whether the file has any table, index or view.
    fn has_tables(conn: &Connection) -> rusqlite::Result<bool> {
        conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master)", [], |r| {
            r.get(0)
        })
    }

    /// Make the tables in a file with a `user_version` of 0. `true` when the file has tables
    /// already and is not subcheck's, which is left as it was. Everything is read inside the
    /// write lock: a second subcheck that read a 0 before the first committed finds the
    /// first's schema here, and takes it as made, not as foreign.
    fn make_schema(conn: &mut Connection) -> rusqlite::Result<bool> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(false);
        }
        if Store::has_tables(&tx)? {
            return Ok(true);
        }
        tx.execute_batch(&format!("{SCHEMA} PRAGMA user_version = {SCHEMA_VERSION};"))?;
        tx.commit()?;
        Ok(false)
    }

    /// Another connection to the same database, for another thread.
    pub fn reopen(&self) -> anyhow::Result<Store> {
        let at = Location {
            path: self.path.clone(),
            from_env: self.from_env,
        };
        let mut store =
            Store::open_at(&self.path).with_context(|| format!("opening {}", at.named()))?;
        store.from_env = self.from_env;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Wait `timeout` for another writer instead of [`BUSY_TIMEOUT`], so a test of a save that
    /// stays busy need not wait five seconds.
    #[cfg(test)]
    pub(crate) fn set_busy_timeout(&self, timeout: Duration) -> rusqlite::Result<()> {
        self.conn.busy_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subcheck.db");
        (dir, path)
    }

    #[test]
    fn a_new_database_is_made_at_version_1_in_wal_with_foreign_keys() {
        let (_dir, path) = temp();
        let store = Store::open_at(&path).unwrap();
        let pragma = |name: &str| -> String {
            store
                .conn
                .pragma_query_value(None, name, |r| r.get::<_, rusqlite::types::Value>(0))
                .map(|v| match v {
                    rusqlite::types::Value::Integer(n) => n.to_string(),
                    rusqlite::types::Value::Text(t) => t,
                    other => format!("{other:?}"),
                })
                .unwrap()
        };
        assert_eq!(pragma("user_version"), "1");
        assert_eq!(pragma("journal_mode"), "wal");
        assert_eq!(pragma("foreign_keys"), "1");
        assert_eq!(pragma("busy_timeout"), "5000");
        // Opened again, it is kept as it is.
        store
            .conn
            .execute(
                "INSERT INTO scans(name, started, backend, langs, prompt_version, version, \
                 command) VALUES ('r', 't', 'claude', '[]', 1, '0', '[]')",
                [],
            )
            .unwrap();
        drop(store);
        let again = Store::open_at(&path).unwrap();
        let n: i64 = again
            .conn
            .query_row("SELECT COUNT(*) FROM scans", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(again.path(), path);
        assert_eq!(again.reopen().unwrap().path(), path);
    }

    #[test]
    fn a_file_with_tables_that_are_not_subchecks_is_refused_and_left_as_it_was() {
        let (_dir, path) = temp();
        let other = Connection::open(&path).unwrap();
        other
            .execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        drop(other);
        let Err(e) = Store::open_at(&path) else {
            panic!("opened a database that is not subcheck's");
        };
        assert_eq!(e.to_string(), "not a subcheck database");
        let at = Location {
            path: path.clone(),
            from_env: true,
        };
        let text = format!("{:#}", Store::open_location(&at).err().unwrap());
        assert_eq!(
            text,
            format!(
                "opening {} (from SUBCHECK_DB): not a subcheck database",
                path.display()
            )
        );
        let tables: Vec<String> = Connection::open(&path)
            .unwrap()
            .prepare("SELECT name FROM sqlite_master")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(tables, ["notes"]);
        let mode: String = Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }

    #[test]
    fn an_empty_sqlite_file_is_taken_as_new() {
        let (_dir, path) = temp();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "application_id", 7)
            .unwrap();
        Store::open_at(&path).unwrap();
    }

    #[test]
    fn reopening_a_database_that_has_gone_names_where_its_path_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/subcheck.db");
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        let mut store = Store::open_at(&path).unwrap();
        store.from_env = true;
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        let text = format!("{:#}", store.reopen().err().unwrap());
        assert!(
            text.starts_with(&format!("opening {} (from SUBCHECK_DB): ", path.display())),
            "{text}"
        );
    }

    #[test]
    fn a_database_a_later_subcheck_made_is_refused() {
        let (_dir, path) = temp();
        Store::open_at(&path).unwrap();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let Err(e) = Store::open_at(&path) else {
            panic!("opened a later subcheck's database");
        };
        assert_eq!(e.to_string(), "subcheck.db was made by a later subcheck");
    }

    #[test]
    fn a_database_that_cannot_be_opened_is_named_with_where_its_path_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let at = Location {
            path: dir.path().join("missing/subcheck.db"),
            from_env: true,
        };
        let text = format!("{:#}", Store::open_location(&at).err().unwrap());
        assert!(
            text.starts_with(&format!(
                "opening {} (from SUBCHECK_DB): ",
                at.path.display()
            )),
            "{text}"
        );
    }

    #[test]
    fn only_the_state_directory_is_made_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        // A SUBCHECK_DB whose directory is missing is an error, and makes nothing.
        let env = Location {
            path: dir.path().join("gone/subcheck.db"),
            from_env: true,
        };
        assert!(Store::open_location(&env).is_err());
        assert!(!dir.path().join("gone").exists());
        // The state directory is made.
        let state = Location {
            path: dir.path().join("state/subcheck/subcheck.db"),
            from_env: false,
        };
        let store = Store::open_location(&state).unwrap();
        assert_eq!(store.path(), state.path);
        assert!(state.path.exists());
    }

    #[test]
    fn many_opening_a_new_database_at_once_all_succeed() {
        const THREADS: usize = 8;
        for round in 0..30 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("subcheck.db");
            let barrier = std::sync::Barrier::new(THREADS);
            let results: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..THREADS)
                    .map(|_| {
                        s.spawn(|| {
                            barrier.wait();
                            Store::open_at(&path)
                                .map(drop)
                                .map_err(|e| format!("{e:#}"))
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for r in results {
                assert_eq!(r, Ok(()), "round {round}");
            }
            let store = Store::open_at(&path).unwrap();
            let mode: String = store
                .conn
                .pragma_query_value(None, "journal_mode", |r| r.get(0))
                .unwrap();
            let version: i64 = store
                .conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!((mode.as_str(), version), ("wal", SCHEMA_VERSION));
        }
    }

    #[test]
    fn a_file_has_at_most_one_current_check_of_each_kind() {
        let (_dir, path) = temp();
        let store = Store::open_at(&path).unwrap();
        let c = &store.conn;
        c.execute(
            "INSERT INTO scans(name, started, backend, langs, prompt_version, version, command) \
             VALUES ('r', 't', 'claude', '[]', 1, '0', '[]')",
            [],
        )
        .unwrap();
        let check = |kind: &str, error: Option<&str>, replaced_by: Option<i64>| {
            c.execute(
                "INSERT INTO checks(scan, path, kind, size, mtime, backend, langs, version, lines, \
                 tracks, error, at, replaced_by) \
                 VALUES (1, '/m/a.mkv', ?1, 1, 't', '', '[]', 1, 0, '[]', ?2, 't', ?3)",
                rusqlite::params![kind, error, replaced_by],
            )
        };
        check("llm", None, None).unwrap();
        let e = check("llm", None, None).unwrap_err();
        assert!(e.to_string().contains("UNIQUE constraint failed"), "{e}");
        // A failed attempt, a check of the other kind, and one replaced are no current check.
        check("llm", Some("boom"), None).unwrap();
        check("timing", None, None).unwrap();
        check("llm", None, Some(1)).unwrap();
        // A kind there is none of.
        assert!(check("audio", None, None).is_err());
    }

    #[test]
    fn the_database_is_where_subcheck_db_says_else_in_the_state_directory() {
        let home = Some(Path::new("/home/u"));
        let xdg = Some(Path::new("/x/state"));
        let at =
            |db: Option<&str>, home, xdg, macos| location_from(db.map(Path::new), home, xdg, macos);
        assert_eq!(
            at(Some("/d/mine.db"), home, xdg, false),
            Ok(Location {
                path: "/d/mine.db".into(),
                from_env: true
            })
        );
        // No HOME is needed then.
        assert!(at(Some("/d/mine.db"), None, None, false).is_ok());
        assert_eq!(
            at(Some("mine.db"), home, xdg, false),
            Err("SUBCHECK_DB must be an absolute path".into())
        );
        // Empty is not set.
        let state = |path: &str| {
            Ok(Location {
                path: path.into(),
                from_env: false,
            })
        };
        assert_eq!(
            at(Some(""), home, xdg, false),
            state("/x/state/subcheck/subcheck.db")
        );
        assert_eq!(
            at(None, home, None, false),
            state("/home/u/.local/state/subcheck/subcheck.db")
        );
        assert_eq!(
            at(None, home, None, true),
            state("/home/u/Library/Application Support/subcheck/subcheck.db")
        );
        assert_eq!(
            at(None, home, xdg, true),
            state("/x/state/subcheck/subcheck.db")
        );
        assert_eq!(
            at(None, None, xdg, false),
            Err("HOME is not set to an absolute path".into())
        );
    }
}

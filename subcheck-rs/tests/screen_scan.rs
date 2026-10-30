//! A scan run by review's screen (`review::jobs::scan`), its messages fed to the model as the
//! screen's loop feeds them: findings join the list as each file is done, and rows decided before a
//! file's findings arrive pass their decisions on, to the store too. And `scan --no-tui`, which
//! keeps to the log.

mod common;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

use common::fixture;
use crossterm::event::KeyCode;
use subcheck::llm::Backend;
use subcheck::player::Player;
use subcheck::review::model::Model;
use subcheck::review::progress::{Scan, Stage};
use subcheck::review::update::{self, Msg};
use subcheck::review::{jobs, opening, run_effect};
use subcheck::scan::{self, Event, Planned};
use subcheck::store::{Selector, State, Store};

/// The loop's inner part: a message, then every effect it leads to, run for real.
fn send(m: &mut Model, msg: Msg, store: &mut Store, player: &mut Player) {
    let now = jiff::Timestamp::now();
    let mut queue: VecDeque<_> = update::update(m, msg, now).into();
    while let Some(effect) = queue.pop_front() {
        let msg = run_effect(effect, store, player);
        queue.extend(update::update(m, msg, now));
    }
}

/// A database of its own, in a directory that goes with it.
fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_at(&dir.path().join("subcheck.db")).unwrap();
    (dir, store)
}

#[test]
fn a_scan_on_the_screen_fills_the_list_and_passes_decisions_on() {
    let (_db, mut store) = store();
    let dir = tempfile::tempdir().unwrap();
    let (e01, e02) = (
        dir.path().join("Show - E01.mkv"),
        dir.path().join("Show - E02.mkv"),
    );
    for f in [&e01, &e02] {
        std::fs::copy(fixture("typos.mkv"), f).unwrap();
    }
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
    let backend = Backend::shell(&format!("'{}'", fake.display()));
    let opts = scan::Options {
        chunk_lines: backend.chunk_lines,
        backend,
        jobs: 1,
        langs: vec![scan::DEFAULT_LANG.to_string()],
        force: false,
        verbose: false,
        into: None,
    };
    let Planned::Scan(plan) = scan::plan(
        &[dir.path().to_path_buf()],
        &opts,
        &mut store,
        &mut std::io::sink(),
    )
    .unwrap() else {
        panic!("nothing to check");
    };
    let name = plan.scan().name.clone();
    let mut model = Model::new(opening(&store, &plan).unwrap(), HashMap::new());
    let mut player = Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock"));
    let (tx, rx) = mpsc::channel();
    jobs::scan(plan, opts, store.reopen().unwrap(), tx).unwrap();

    let mut accepted = false;
    loop {
        let msg = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the scan goes on");
        let ended = matches!(msg, Msg::ScanEnded(_));
        let e01_done = matches!(&msg, Msg::Scan(Event::Finished { path, .. }) if *path == e01);
        send(&mut model, msg, &mut store, &mut player);
        if e01_done {
            // E01's two findings are rows of their own: accept both before E02's arrive (they are
            // in the channel already, behind this one). E01's alone so far: were E02's here too,
            // there would be no late join to see.
            assert_eq!(model.run.findings.len(), 2);
            assert_eq!(model.rows.len(), 2);
            model.selected = 0;
            for _ in 0..2 {
                send(
                    &mut model,
                    Msg::Key(KeyCode::Char('a')),
                    &mut store,
                    &mut player,
                );
            }
            accepted = true;
        }
        if ended {
            break;
        }
    }
    assert!(accepted);
    // E02's findings joined E01's rows and took their decisions.
    assert_eq!(model.run.findings.len(), 4);
    assert!(model.rows.iter().all(|r| r.members.len() == 2));
    let run = store.view(&Selector::Run(name)).unwrap();
    assert_eq!(run.findings.len(), 4);
    assert!(
        run.findings
            .iter()
            .all(|f| run.state_of(f.id) == State::Accepted)
    );
    let stages: Vec<&Stage> = model.progress.files.iter().map(|f| &f.stage).collect();
    assert_eq!(stages, [&Stage::Done { findings: 2 }; 2]);
    assert!(matches!(&model.progress.scan, Scan::Ended(Ok(s)) if s.findings == 4));
}

#[test]
fn a_file_whose_llm_check_stands_shows_the_timing_check_alone() {
    let (_db, mut store) = store();
    let (_dir, file) = common::copied("timing.mkv");
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
    let options = |into| {
        let backend = Backend::shell(&format!("'{}'", fake.display()));
        scan::Options {
            chunk_lines: backend.chunk_lines,
            backend,
            jobs: 1,
            langs: vec![scan::DEFAULT_LANG.to_string()],
            force: false,
            verbose: false,
            into,
        }
    };
    let plan = |store: &mut Store, opts: &scan::Options| {
        let planned = scan::plan(
            std::slice::from_ref(&file),
            opts,
            store,
            &mut std::io::sink(),
        );
        match planned.unwrap() {
            Planned::Scan(plan) => plan,
            Planned::Nothing(_) => panic!("nothing to check"),
        }
    };
    let opts = options(None);
    let first = plan(&mut store, &opts);
    let name = first.scan().name.clone();
    scan::execute(first, &opts, &mut store, &mut std::io::sink(), &|_| {}).unwrap();
    // The timing check as rules of a later version would leave it: no longer fresh.
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute("UPDATE checks SET version = 0 WHERE kind = 'timing'", [])
        .unwrap();

    // `--run` makes the timing check again, on the screen.
    let opts = options(Some(name));
    let adding = plan(&mut store, &opts);
    assert_eq!(adding.todo()[0].1, [subcheck::store::Check::Timing]);
    let mut model = Model::new(opening(&store, &adding).unwrap(), HashMap::new());
    assert_eq!(
        model.run.findings.len(),
        0,
        "its timing findings join as it is saved"
    );
    let (tx, rx) = mpsc::channel();
    jobs::scan(adding, opts, store.reopen().unwrap(), tx).unwrap();
    loop {
        let msg = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the scan goes on");
        let ended = matches!(msg, Msg::ScanEnded(_));
        assert_eq!(update::update(&mut model, msg, jiff::Timestamp::now()), []);
        if ended {
            break;
        }
    }
    let labels: Vec<String> = model.progress.files.iter().map(|f| f.label('⠙')).collect();
    assert_eq!(labels, ["✓ timing only · 2 findings"]);
    assert_eq!(model.run.findings.len(), 2);
}

#[test]
fn a_skipped_files_findings_are_on_the_screen_from_the_start() {
    let (_db, mut store) = store();
    let dir = tempfile::tempdir().unwrap();
    let (e01, e02) = (
        dir.path().join("Show - E01.mkv"),
        dir.path().join("Show - E02.mkv"),
    );
    std::fs::copy(fixture("typos.mkv"), &e01).unwrap();
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
    let opts = || {
        let backend = Backend::shell(&format!("'{}'", fake.display()));
        scan::Options {
            chunk_lines: backend.chunk_lines,
            backend,
            jobs: 1,
            langs: vec![scan::DEFAULT_LANG.to_string()],
            force: false,
            verbose: false,
            into: None,
        }
    };
    let plan = |store: &mut Store, opts: &scan::Options| {
        let planned = scan::plan(
            &[dir.path().to_path_buf()],
            opts,
            store,
            &mut std::io::sink(),
        );
        match planned.unwrap() {
            Planned::Scan(plan) => plan,
            Planned::Nothing(_) => panic!("nothing to check"),
        }
    };
    let first_opts = opts();
    let first = plan(&mut store, &first_opts);
    scan::execute(
        first,
        &first_opts,
        &mut store,
        &mut std::io::sink(),
        &|_| {},
    )
    .unwrap();

    // A second file joins; E01 is unchanged, so this scan skips it and checks E02 alone.
    std::fs::copy(fixture("typos.mkv"), &e02).unwrap();
    let second_opts = opts();
    let second = plan(&mut store, &second_opts);
    let todo: Vec<&Path> = second.todo().iter().map(|(p, _)| p.as_path()).collect();
    assert_eq!(todo, [e02.as_path()]);
    let run = opening(&store, &second).unwrap();
    assert_eq!(run.findings.len(), 2, "E01's findings, before any event");
    assert!(run.findings.iter().all(|f| Path::new(&f.file) == e01));
}

/// `subcheck scan` as a process, stdout and stderr pipes, with tests/fake-llm.sh as its LLM,
/// `state` as XDG_STATE_HOME and `state/subcheck.db` as SUBCHECK_DB.
fn scan_cli(state: &Path, args: &[&str]) -> Output {
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
    Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .arg("scan")
        .args(args)
        .arg("--llm-cmd")
        .arg(format!("'{}'", fake.display()))
        .env("XDG_STATE_HOME", state)
        .env("SUBCHECK_DB", state.join("subcheck.db"))
        .output()
        .unwrap()
}

#[test]
fn no_tui_prints_the_log_a_scan_on_a_pipe_prints() {
    let dir = tempfile::tempdir().unwrap();
    for ep in ["E01", "E02"] {
        let to = dir.path().join(format!("Show - {ep}.mkv"));
        std::fs::copy(fixture("typos.mkv"), to).unwrap();
    }
    let arg = dir.path().to_string_lossy().into_owned();
    // Each in a state directory of its own, so neither finds the files checked already: the run's
    // name, the time it started, is all that may differ.
    let scanned = |extra: &[&str]| {
        let state = tempfile::tempdir().unwrap();
        let mut args = vec![arg.as_str()];
        args.extend(extra);
        let out = scan_cli(state.path(), &args);
        let db = Store::open_at(&state.path().join("subcheck.db")).unwrap();
        let name = db.view(&Selector::Newest).unwrap().name;
        let text = |b: Vec<u8>| String::from_utf8(b).unwrap().replace(&name, "<run>");
        (out.status.code(), text(out.stdout), text(out.stderr))
    };
    let piped = scanned(&[]);
    assert_eq!(scanned(&["--no-tui"]), piped);
    let (code, out, err) = piped;
    assert_eq!((code, out.as_str()), (Some(0), ""));
    assert_eq!(
        err,
        "[1/2] Show - E01.mkv: 4 lines, 2 findings\n\
         [2/2] Show - E02.mkv: 4 lines, 2 findings\n\
         run <run>: 2 files, 2 checked, 0 failed, 0 unchanged since an earlier scan; 4 findings\n\
         Review them with: subcheck review <run> (or print them: subcheck list <run>)\n"
    );
}

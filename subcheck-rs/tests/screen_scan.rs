//! A scan run by review's screen (`review::jobs::scan`), its messages fed to the model as the
//! screen's loop feeds them: findings join the list as each file is done and a file checked again
//! keeps its findings until then, and rows decided before a file's findings arrive pass their
//! decisions on, to the store too. And `scan --no-tui`, which keeps to the log.

mod common;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

use common::fixture;
use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use subcheck::llm::Backend;
use subcheck::player::Player;
use subcheck::review::model::Model;
use subcheck::review::progress::{Scan, Stage};
use subcheck::review::update::{self, Msg};
use subcheck::review::{jobs, opening, run_effect};
use subcheck::scan::{self, Event, Planned};
use subcheck::store::{Decision, Selector, State, Store};

/// The loop's inner part: a message, then every effect it leads to, run for real.
fn send(m: &mut Model, msg: Msg, store: &mut Store, player: &mut Player, tx: &mpsc::Sender<Msg>) {
    let now = jiff::Timestamp::now();
    // Where apply would write its tracks: no test here starts apply, so nothing goes there.
    let state = Path::new("/no-state");
    let mut queue: VecDeque<_> = update::update(m, msg, now).into();
    while let Some(effect) = queue.pop_front() {
        let msg = run_effect(effect, store, player, state, tx);
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
    jobs::scan(plan, opts, store.reopen().unwrap(), tx.clone()).unwrap();

    let mut accepted = false;
    loop {
        let msg = rx
            .recv_timeout(Duration::from_mins(1))
            .expect("the scan goes on");
        let ended = matches!(msg, Msg::ScanEnded(_));
        let e01_done = matches!(&msg, Msg::Scan(Event::Finished { path, .. }) if *path == e01);
        send(&mut model, msg, &mut store, &mut player, &tx);
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
                    &tx,
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
        2,
        "its earlier timing findings, until the new ones are saved"
    );
    let dir = tempfile::tempdir().unwrap();
    let mut player = Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock"));
    let (tx, rx) = mpsc::channel();
    jobs::scan(adding, opts, store.reopen().unwrap(), tx.clone()).unwrap();
    loop {
        let msg = rx
            .recv_timeout(Duration::from_mins(1))
            .expect("the scan goes on");
        let ended = matches!(msg, Msg::ScanEnded(_));
        send(&mut model, msg, &mut store, &mut player, &tx);
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

fn fake_llm() -> Backend {
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
    Backend::shell(&format!("'{}'", fake.display()))
}

fn options(backend: Backend, force: bool) -> scan::Options {
    scan::Options {
        chunk_lines: backend.chunk_lines,
        backend,
        jobs: 1,
        langs: vec![scan::DEFAULT_LANG.to_string()],
        force,
        verbose: false,
        into: None,
    }
}

/// `file` scanned with tests/fake-llm.sh to the end, and its first finding rejected: that finding.
fn scanned_and_rejected(store: &mut Store, file: &Path) -> subcheck::store::Finding {
    let opts = options(fake_llm(), false);
    let Planned::Scan(plan) =
        scan::plan(&[file.to_path_buf()], &opts, store, &mut std::io::sink()).unwrap()
    else {
        panic!("nothing to check");
    };
    let name = plan.scan().name.clone();
    scan::execute(plan, &opts, store, &mut std::io::sink(), &|_| {}).unwrap();
    let first = store.view(&Selector::Run(name)).unwrap().findings.remove(0);
    store
        .decide(&[Decision {
            finding: first.id,
            state: State::Rejected,
            edited: None,
            at: jiff::Timestamp::now(),
        }])
        .unwrap();
    first
}

/// `file` checked again (`--force`) on the screen, from `opening` to the scan's end, with
/// `before_job` run between planning and starting it: the model, and the IDs it opened with.
fn checked_again(
    store: &mut Store,
    file: &Path,
    before_job: impl FnOnce(),
    mut on_msg: impl FnMut(&Model, &Msg),
) -> (Model, Vec<u64>) {
    let opts = options(fake_llm(), true);
    let Planned::Scan(plan) =
        scan::plan(&[file.to_path_buf()], &opts, store, &mut std::io::sink()).unwrap()
    else {
        panic!("nothing to check");
    };
    let mut model = Model::new(opening(store, &plan).unwrap(), HashMap::new());
    let opened: Vec<u64> = model.run.findings.iter().map(|f| f.id).collect();
    before_job();
    let dir = tempfile::tempdir().unwrap();
    let mut player = Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock"));
    let (tx, rx) = mpsc::channel();
    jobs::scan(plan, opts, store.reopen().unwrap(), tx.clone()).unwrap();
    loop {
        let msg = rx
            .recv_timeout(Duration::from_mins(1))
            .expect("the scan goes on");
        let ended = matches!(msg, Msg::ScanEnded(_));
        let seen = msg.clone();
        send(&mut model, msg, store, &mut player, &tx);
        on_msg(&model, &seen);
        if ended {
            break;
        }
    }
    (model, opened)
}

#[test]
fn a_file_checked_again_keeps_its_findings_until_its_new_ones_are_saved() {
    let (_db, mut store) = store();
    let (_dir, file) = common::copied("typos.mkv");
    let first = scanned_and_rejected(&mut store, &file);
    let mut while_reading: Option<Vec<u64>> = None;
    let (model, opened) = checked_again(
        &mut store,
        &file,
        || {},
        |m, msg| {
            if matches!(msg, Msg::Scan(Event::Reading(_))) {
                while_reading = Some(m.run.findings.iter().map(|f| f.id).collect());
            }
        },
    );
    assert!(
        opened.contains(&first.id),
        "the earlier findings, from the start"
    );
    assert_eq!(
        while_reading.as_ref(),
        Some(&opened),
        "still the earlier check's while it reads"
    );
    assert!(
        !model.run.findings.iter().any(|f| opened.contains(&f.id)),
        "the earlier ones went once the new ones were saved"
    );
    let back = model
        .run
        .findings
        .iter()
        .find(|f| f.original == first.original)
        .expect("the rejected line came back");
    assert_eq!(model.run.state_of(back.id), State::Rejected);
}

#[test]
fn a_check_made_again_that_fails_keeps_the_files_findings() {
    let (_db, mut store) = store();
    let (_dir, file) = common::copied("typos.mkv");
    let first = scanned_and_rejected(&mut store, &file);
    // The file goes bad between planning and reading: its check fails.
    let (model, opened) = checked_again(
        &mut store,
        &file,
        || std::fs::write(&file, b"not a Matroska file").unwrap(),
        |_, _| {},
    );
    assert!(
        opened.contains(&first.id),
        "the earlier findings, from the start"
    );
    let ids: Vec<u64> = model.run.findings.iter().map(|f| f.id).collect();
    assert_eq!(ids, opened);
    assert_eq!(model.run.state_of(first.id), State::Rejected);
    assert!(matches!(model.progress.files[0].stage, Stage::Failed(_)));
}

#[test]
fn c_checks_a_file_again_and_a_decision_on_a_finding_that_comes_back_is_kept() {
    let (_db, mut store) = store();
    let (_dir, file) = common::copied("typos.mkv");
    let first = scanned_and_rejected(&mut store, &file);
    let run = store.view(&Selector::Newest).unwrap();
    let mut model = Model::new(run, HashMap::new());
    let dir = tempfile::tempdir().unwrap();
    let mut player = Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock"));
    let (tx, rx) = mpsc::channel();
    for key in [KeyCode::Tab, KeyCode::Char('c')] {
        send(&mut model, Msg::Key(key), &mut store, &mut player, &tx);
    }
    let asked = model.status_text(Rect::new(0, 0, 200, 1)).unwrap();
    assert!(
        asked.starts_with("check ") && asked.ends_with("? y/n"),
        "{asked}"
    );
    send(
        &mut model,
        Msg::Key(KeyCode::Char('y')),
        &mut store,
        &mut player,
        &tx,
    );
    assert!(model.progress.running());
    loop {
        let msg = rx
            .recv_timeout(Duration::from_mins(1))
            .expect("the check goes on");
        let ended = matches!(msg, Msg::ScanEnded(_));
        send(&mut model, msg, &mut store, &mut player, &tx);
        if ended {
            break;
        }
    }
    assert!(matches!(&model.progress.scan, Scan::Ended(Ok(_))));
    let back = model
        .run
        .findings
        .iter()
        .find(|f| f.original == first.original)
        .expect("the rejected line came back");
    assert_ne!(back.id, first.id);
    assert_eq!(model.run.state_of(back.id), State::Rejected);
}

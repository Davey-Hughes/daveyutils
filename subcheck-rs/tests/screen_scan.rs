//! A scan run by review's screen (`review::jobs::scan`), its messages fed to the model as the
//! screen's loop feeds them: findings join the list as each file is done, and rows decided before a
//! file's findings arrive pass their decisions on, to the run file too. And `scan --no-tui`, which
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
use subcheck::review::{jobs, run_effect};
use subcheck::scan::{self, Event, Planned};
use subcheck::store::{self, RunWriter, State};

/// The loop's inner part: a message, then every effect it leads to, run for real.
fn send(m: &mut Model, msg: Msg, writer: &mut RunWriter, player: &mut Player) {
    let now = jiff::Timestamp::now();
    let mut queue: VecDeque<_> = update::update(m, msg, now).into();
    while let Some(effect) = queue.pop_front() {
        let msg = run_effect(effect, writer, player);
        queue.extend(update::update(m, msg, now));
    }
}

#[test]
fn a_scan_on_the_screen_fills_the_list_and_passes_decisions_on() {
    let state = tempfile::tempdir().unwrap();
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
        state.path(),
        &mut std::io::sink(),
    )
    .unwrap() else {
        panic!("nothing to check");
    };
    let path = plan.run_path().to_path_buf();
    let mut model = Model::new(store::read_run(&path).unwrap(), HashMap::new());
    let mut writer = RunWriter::open(&path).unwrap();
    let mut player = Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock"));
    let (tx, rx) = mpsc::channel();
    jobs::scan(plan, opts, state.path().to_path_buf(), tx).unwrap();

    let mut accepted = false;
    loop {
        let msg = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the scan goes on");
        let ended = matches!(msg, Msg::ScanEnded(_));
        let e01_done = matches!(&msg, Msg::Scan(Event::Finished { path, .. }) if *path == e01);
        send(&mut model, msg, &mut writer, &mut player);
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
                    &mut writer,
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
    let run = store::read_run(&path).unwrap();
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
fn a_file_whose_llm_check_the_run_has_shows_the_timing_check_alone() {
    let state = tempfile::tempdir().unwrap();
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
    let plan = |opts: &scan::Options| {
        let planned = scan::plan(
            std::slice::from_ref(&file),
            opts,
            state.path(),
            &mut std::io::sink(),
        );
        match planned.unwrap() {
            Planned::Scan(plan) => plan,
            Planned::Nothing(_) => panic!("nothing to check"),
        }
    };
    let opts = options(None);
    let first = plan(&opts);
    let path = first.run_path().to_path_buf();
    scan::execute(first, &opts, state.path(), &mut std::io::sink(), &|_| {}).unwrap();
    // The run as a scan from before the timing check left it: the LLM's check alone.
    let text = std::fs::read_to_string(&path).unwrap();
    let kept: Vec<String> = text
        .lines()
        .filter(|l| !l.contains(r#""source":"timing""#))
        .map(|l| l.replace(r#""checks":["llm","timing"]"#, r#""checks":["llm"]"#))
        .collect();
    std::fs::write(&path, kept.join("\n") + "\n").unwrap();

    // `--run` adds the timing check, on the screen.
    let opts = options(Some(path.clone()));
    let adding = plan(&opts);
    let mut model = Model::new(store::read_run(&path).unwrap(), HashMap::new());
    let (tx, rx) = mpsc::channel();
    jobs::scan(adding, opts, state.path().to_path_buf(), tx).unwrap();
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
}

/// `subcheck scan` as a process, stdout and stderr pipes, with tests/fake-llm.sh as its LLM and
/// `state` as XDG_STATE_HOME.
fn scan_cli(state: &Path, args: &[&str]) -> Output {
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
    Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .arg("scan")
        .args(args)
        .arg("--llm-cmd")
        .arg(format!("'{}'", fake.display()))
        .env("XDG_STATE_HOME", state)
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
        let run = store::find_run(&state.path().join("subcheck"), None).unwrap();
        let name = run.file_stem().unwrap().to_string_lossy().into_owned();
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

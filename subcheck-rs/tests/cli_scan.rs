//! `subcheck scan` and `subcheck list` end to end, with tests/fake-llm.sh as the LLM and a
//! temporary directory as XDG_STATE_HOME.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::fixture;
use subcheck::store::{self, State};

struct Env {
    state: tempfile::TempDir,
    log: PathBuf,
}

impl Env {
    fn new() -> Env {
        let state = tempfile::tempdir().unwrap();
        let log = state.path().join("llm-calls.log");
        Env { state, log }
    }

    fn subcheck(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_subcheck"));
        cmd.args(args)
            .env("XDG_STATE_HOME", self.state.path())
            .env("FAKE_LLM_LOG", &self.log);
        if args.first() == Some(&"scan") {
            cmd.arg("--llm-cmd").arg(format!("'{}'", fake.display()));
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn newest_run(&self) -> store::Run {
        let dir = self.state.path().join("subcheck");
        store::read_run(&store::find_run(&dir, None).unwrap()).unwrap()
    }
}

fn path(name: &str) -> String {
    fixture(name).to_string_lossy().into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn scan_records_the_planted_typos() {
    let env = Env::new();
    let out = env.subcheck(&["scan", &path("typos.mkv")], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "1 files, 1 checked, 0 failed, 0 unchanged since an earlier scan; 2 findings"
        ),
        "{}",
        stderr(&out)
    );

    let run = env.newest_run();
    assert_eq!(
        run.header.as_ref().unwrap().prompt_version,
        subcheck::llm::PROMPT_VERSION
    );
    let pairs: Vec<(&str, &str)> = run
        .findings
        .iter()
        .map(|f| (f.original.as_str(), f.suggestion.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            (
                "We should leave befor it rains.",
                "We should leave before it rains."
            ),
            ("go trough it", "go through it"),
        ]
    );
    let karaoke = &run.findings[1];
    assert_eq!(karaoke.style, "Song");
    assert_eq!(
        karaoke.before,
        ["We should leave befor it rains.", "Nothing wrong here."]
    );
    assert_eq!(karaoke.after, ["The last line."]);
    assert!(run
        .findings
        .iter()
        .all(|f| run.state_of(f.id) == State::Pending));

    let file = &run.files[0];
    assert_eq!((file.error.as_deref(), file.lines), (None, 4));
    assert_eq!(file.tracks[0].sid, 1);
    assert_eq!(file.tracks[0].read, "cues");
}

#[test]
fn an_empty_track_costs_no_llm_call() {
    let env = Env::new();
    let out = env.subcheck(&["scan", &path("empty.mkv")], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("empty.mkv: 0 lines, 0 findings"),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.calls(), [] as [String; 0]);
    assert_eq!(env.newest_run().files[0].tracks[0].read, "empty");
}

#[test]
fn an_unchanged_file_is_skipped_until_forced() {
    let env = Env::new();
    assert!(env
        .subcheck(&["scan", &path("typos.mkv")], &[])
        .status
        .success());
    assert_eq!(env.calls().len(), 1);

    let out = env.subcheck(&["scan", &path("typos.mkv")], &[]);
    assert!(
        stderr(&out).contains("Nothing to check: 1 file unchanged since an earlier scan"),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.calls().len(), 1, "a cached file must not reach the LLM");

    let out = env.subcheck(&["scan", &path("typos.mkv"), "--force"], &[]);
    assert!(stderr(&out).contains("1 checked"), "{}", stderr(&out));
    assert_eq!(env.calls().len(), 2);
}

#[test]
fn a_scan_with_nothing_to_check_records_no_run() {
    let env = Env::new();
    assert!(env
        .subcheck(&["scan", &path("typos.mkv")], &[])
        .status
        .success());
    assert!(env
        .subcheck(&["scan", &path("typos.mkv")], &[])
        .status
        .success());
    let runs = std::fs::read_dir(env.state.path().join("subcheck/runs"))
        .unwrap()
        .count();
    assert_eq!(
        runs, 1,
        "an empty run would hide the real one from `subcheck list`"
    );
    assert_eq!(env.newest_run().findings.len(), 2);
}

#[test]
fn changing_the_languages_checks_a_file_again() {
    let env = Env::new();
    assert!(env
        .subcheck(&["scan", &path("typos.mkv")], &[])
        .status
        .success());
    let out = env.subcheck(
        &["scan", &path("typos.mkv"), "--lang", "eng", "--lang", "und"],
        &[],
    );
    assert!(stderr(&out).contains("1 checked"), "{}", stderr(&out));
    assert_eq!(env.calls().len(), 2);
}

#[test]
fn dry_run_leaves_out_files_a_scan_would_skip() {
    let env = Env::new();
    assert!(env
        .subcheck(&["scan", &path("typos.mkv")], &[])
        .status
        .success());
    let out = env.subcheck(&["scan", &path("typos.mkv"), "--dry-run"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("(chunk"), "{stdout}");
    assert!(
        stderr(&out).contains("1 file unchanged since an earlier scan"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_llm_failure_fails_the_file_after_one_retry_and_is_not_cached() {
    let env = Env::new();
    let out = env.subcheck(&["scan", &path("typos.mkv")], &[("FAKE_LLM_FAIL", "1")]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("FAILED: LLM: sh exited with exit status: 3: fake-llm: told to fail"),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.calls().len(), 2, "one call plus one retry");
    let run = env.newest_run();
    assert!(run.files[0].error.is_some());

    // Not cached: the next scan tries again.
    let out = env.subcheck(&["scan", &path("typos.mkv")], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.newest_run().findings.len(), 2);
}

#[test]
fn a_reply_with_neither_findings_nor_none_is_a_failure() {
    let env = Env::new();
    let out = env.subcheck(
        &["scan", &path("typos.mkv")],
        &[("FAKE_LLM_REPLY", "These lines look fine to me.")],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("reply has no findings and no NONE"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn lines_shared_between_files_are_sent_once() {
    let env = Env::new();
    // ass-plain's one track (language und) and multi's English Full track hold the same five
    // lines; multi's Signs track changes one of them. Six distinct lines in all.
    let out = env.subcheck(
        &[
            "scan",
            &path("ass-plain.mkv"),
            &path("multi.mkv"),
            "--lang",
            "eng",
            "--lang",
            "und",
        ],
        &[],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    // Two workers: the calls can land in either order.
    let mut calls = env.calls();
    calls.sort();
    assert_eq!(calls, ["1 lines", "5 lines"]);
    let run = env.newest_run();
    assert_eq!(
        run.files.iter().map(|f| f.lines).collect::<Vec<_>>(),
        [5, 6]
    );
}

#[test]
fn a_directory_is_walked_and_a_bad_file_does_not_stop_the_scan() {
    let env = Env::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(fixture("typos.mkv"), dir.path().join("a.mkv")).unwrap();
    std::fs::write(dir.path().join("b.mkv"), "not a matroska file").unwrap();
    let out = env.subcheck(&["scan", &dir.path().to_string_lossy()], &[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("a.mkv: 4 lines, 2 findings"), "{err}");
    assert!(
        err.contains("b.mkv: FAILED: parse error at byte 0"),
        "{err}"
    );
}

#[test]
fn dry_run_prints_prompts_and_records_nothing() {
    let env = Env::new();
    let out = env.subcheck(&["scan", &path("typos.mkv"), "--dry-run"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("(chunk 0, 4 lines)"), "{stdout}");
    assert!(
        stdout.contains("L3 [0:00:20.00] (Song) go trough it\n"),
        "{stdout}"
    );
    assert_eq!(env.calls(), [] as [String; 0]);
    assert!(!Path::new(&env.state.path().join("subcheck/runs")).exists());
}

#[test]
fn list_shows_the_newest_run_with_mpv_commands() {
    let env = Env::new();
    assert!(env
        .subcheck(&["scan", &path("typos.mkv")], &[])
        .status
        .success());
    let out = env.subcheck(&["list", "--mpv"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("  #1  0:00:01.00  Default  [pending]\n      - We should leave befor it rains.\n      + We should leave before it rains.\n      why: planted typo\n"), "{text}");
    assert!(
        text.contains(&format!(
            "      mpv --start=0:00:18.00 --sid=1 '{}'\n",
            path("typos.mkv")
        )),
        "{text}"
    );

    let out = env.subcheck(&["list", "--accepted"], &[]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("No findings."));
}

#[test]
fn list_without_any_run_says_so() {
    let env = Env::new();
    let out = env.subcheck(&["list"], &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no runs yet"), "{}", stderr(&out));
}

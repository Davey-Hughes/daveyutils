//! `subcheck scan` and `subcheck list` end to end, with tests/fake-llm.sh as the LLM and a
//! temporary directory as XDG_STATE_HOME.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::fixture;
use subcheck::store::{self, Check, State};

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

    /// `subcheck scan` with the preset `llm`, whose program (`claude`, `codex`) is the fake LLM by
    /// that name, first on the PATH.
    fn scan_preset(&self, llm: &str, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let bin = self.fake_bin();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_subcheck"));
        cmd.arg("scan")
            .args(args)
            .args(["--llm", llm])
            .env("XDG_STATE_HOME", self.state.path())
            .env("FAKE_LLM_LOG", &self.log)
            .env("PATH", path);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// A directory of links to the fake LLM named `claude` and `codex`.
    fn fake_bin(&self) -> PathBuf {
        let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
        let bin = self.state.path().join("bin");
        if !bin.exists() {
            std::fs::create_dir(&bin).unwrap();
            for name in ["claude", "codex"] {
                std::os::unix::fs::symlink(&fake, bin.join(name)).unwrap();
            }
        }
        bin
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
    assert!(
        run.findings
            .iter()
            .all(|f| run.state_of(f.id) == State::Pending)
    );

    let file = &run.files[0];
    assert_eq!((file.error.as_deref(), file.lines), (None, 4));
    assert_eq!(file.tracks[0].sid, 1);
    assert_eq!(file.tracks[0].read, "cues");
}

#[test]
fn the_claude_and_gpt_presets_read_their_replies_from_json() {
    for llm in ["claude", "gpt"] {
        let env = Env::new();
        let out = env.scan_preset(llm, &[&path("typos.mkv")], &[]);
        assert!(out.status.success(), "{llm}: {}", stderr(&out));
        let run = env.newest_run();
        let pairs: Vec<(&str, &str, &str)> = run
            .findings
            .iter()
            .map(|f| {
                (
                    f.original.as_str(),
                    f.suggestion.as_str(),
                    f.reason.as_str(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [
                (
                    "We should leave befor it rains.",
                    "We should leave before it rains.",
                    "planted typo"
                ),
                ("go trough it", "go through it", "planted typo"),
            ],
            "{llm}"
        );
        assert_eq!(run.header.unwrap().backend, llm);
    }
}

#[test]
fn the_fake_claude_escapes_every_control_character_in_its_json() {
    use std::io::Write as _;
    let env = Env::new();
    let mut child = Command::new(env.fake_bin().join("claude"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // A CRLF subtitle line keeps its CR; a stray control character rides along. ESC, not \x01:
    // that is bash's own quoting byte, which bash 3.2 mishandles in `read` and `${s:i:1}`.
    let prompt = "L1 [0:00:01.00] (D) leave befor it\r now\u{1b}\n";
    child
        .stdin
        .take()
        .unwrap()
        .write_all(prompt.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!(
        reply["result"],
        "L1\tleave before it\r now\u{1b}\tplanted typo"
    );
}

/// The run file's usage records.
fn usage_lines(run: &Path) -> Vec<String> {
    std::fs::read_to_string(run)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with(r#"{"type":"usage""#))
        .map(String::from)
        .collect()
}

#[test]
fn each_llm_call_is_recorded_with_what_it_used_and_the_scan_says_what_they_cost() {
    let env = Env::new();
    let typos = path("typos.mkv");
    let out = env.scan_preset("claude", &[&typos], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    // The summary, then what the calls cost; the fake claude's price, 1.25 cents a call.
    assert!(
        stderr(&out).contains(
            "; 2 findings\nLLM: 1 call, 1.20k in / 40 out tokens, $0.013 at API prices\n"
        ),
        "{}",
        stderr(&out)
    );
    let run_path = store::find_run(&env.state.path().join("subcheck"), None).unwrap();
    assert_eq!(
        usage_lines(&run_path),
        [format!(
            r#"{{"type":"usage","file":"{typos}","lines":4,"input_tokens":1203,"output_tokens":40,"cache_read_tokens":200,"cache_write_tokens":1000,"cost_usd":0.0125}}"#
        )]
    );
    assert_eq!(env.newest_run().spent.calls, 1);
    let list = env.subcheck(&["list"], &[]);
    let text = String::from_utf8_lossy(&list.stdout);
    assert_eq!(
        text.lines().nth(1),
        Some("LLM: 1 call, 1.20k in / 40 out tokens, $0.013 at API prices"),
        "{text}"
    );

    // codex reports tokens and no price: none is shown.
    let env = Env::new();
    let out = env.scan_preset("gpt", &[&typos], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("\nLLM: 1 call, 1.50k in / 40 out tokens\n"),
        "{}",
        stderr(&out)
    );
    let run_path = store::find_run(&env.state.path().join("subcheck"), None).unwrap();
    assert_eq!(
        usage_lines(&run_path),
        [format!(
            r#"{{"type":"usage","file":"{typos}","lines":4,"input_tokens":1500,"output_tokens":40,"cache_read_tokens":1000}}"#
        )]
    );

    // A command LLM reports nothing, and nothing is said.
    let env = Env::new();
    let out = env.subcheck(&["scan", &typos], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("LLM:"), "{}", stderr(&out));
    let run_path = store::find_run(&env.state.path().join("subcheck"), None).unwrap();
    assert_eq!(usage_lines(&run_path), [] as [String; 0]);
}

#[test]
fn a_failed_call_is_recorded_too_and_so_is_a_call_adding_to_a_run() {
    let env = Env::new();
    let typos = path("typos.mkv");
    // A reply the parser cannot read, twice: both calls cost.
    let out = env.scan_preset(
        "claude",
        &[&typos],
        &[("FAKE_LLM_REPLY", "These lines look fine to me.")],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("\nLLM: 2 calls, 2.41k in / 80 out tokens, $0.025 at API prices\n"),
        "{}",
        stderr(&out)
    );
    let run = env.newest_run();
    assert!(run.files[0].error.is_some());
    assert_eq!(run.spent.calls, 2);

    // Adding the failed file to the run again: the run's calls are both scans'.
    let out = env.scan_preset("claude", &[&typos, "--run", &run.name], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "; 2 findings\nLLM: 1 call, 1.20k in / 40 out tokens, $0.013 at API prices\n"
        ),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.newest_run().spent.calls, 3);
}

#[test]
fn a_call_that_says_nothing_of_what_it_used_is_counted_and_said_so() {
    let typos = path("typos.mkv");
    let silent = format!(r#"{{"type":"usage","file":"{typos}","lines":4}}"#);
    // The fake claude replies in plain text, not its JSON; or fails with nothing on stdout. Twice
    // either way (a call and its retry): two calls, each counted with no numbers.
    for (fake, failed) in [
        ("FAKE_LLM_PLAIN", "FAILED: LLM: reply is not claude's JSON"),
        (
            "FAKE_LLM_FAIL",
            "FAILED: LLM: claude exited with exit status: 3: fake-llm: told to fail\n",
        ),
    ] {
        let env = Env::new();
        let out = env.scan_preset("claude", &[&typos], &[(fake, "1")]);
        assert!(!out.status.success());
        let err = stderr(&out);
        assert!(err.contains(failed), "{fake}: {err}");
        assert!(
            err.contains("\nLLM: 2 calls (none reported usage)\n"),
            "{fake}: {err}"
        );
        let run_path = store::find_run(&env.state.path().join("subcheck"), None).unwrap();
        assert_eq!(
            usage_lines(&run_path),
            [silent.clone(), silent.clone()],
            "{fake}"
        );
    }

    // A command LLM that fails is not counted: it never reports.
    let env = Env::new();
    let out = env.subcheck(&["scan", &typos], &[("FAKE_LLM_FAIL", "1")]);
    assert!(
        stderr(&out).contains("FAILED: LLM: sh exited"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("\nLLM:"), "{}", stderr(&out));
}

#[test]
fn scan_records_timing_findings_beside_the_llms() {
    let env = Env::new();
    let out = env.subcheck(&["scan", &path("timing.mkv")], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unchanged since an earlier scan; 2 findings (2 timing)"),
        "{}",
        stderr(&out)
    );
    let run = env.newest_run();
    let found: Vec<(usize, &str, &str, &str)> = run
        .findings
        .iter()
        .map(|f| {
            (
                f.timing.as_ref().unwrap().track,
                f.original.as_str(),
                f.suggestion.as_str(),
                f.reason.as_str(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (
                2,
                "1 frame late",
                "shift 1 frame earlier",
                "cuts: 85 times, +1 in 2 of 2 windows"
            ),
            (
                4,
                "1 frame late",
                "shift 1 frame earlier",
                "shares 34 times with track 2, all at the same frame"
            ),
        ]
    );
    assert!(run.findings.iter().all(|f| f.source == "timing"));
    assert_eq!(run.files[0].checks, [Check::Llm, Check::Timing]);
    assert_eq!(run.header.unwrap().langs, ["eng"]);
}

#[test]
fn a_file_checked_before_the_timing_check_existed_gets_that_check_alone() {
    let env = Env::new();
    assert!(
        env.subcheck(&["scan", &path("timing.mkv")], &[])
            .status
            .success()
    );
    // The cache as a scan from before the timing check left it: the LLM's entries only.
    let cache = env.state.path().join("subcheck/cache.jsonl");
    let text = std::fs::read_to_string(&cache).unwrap();
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !l.contains(r#""check":"timing""#))
        .collect();
    std::fs::write(&cache, kept.join("\n") + "\n").unwrap();
    let calls = env.calls().len();

    let out = env.subcheck(&["scan", &path("timing.mkv")], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("timing.mkv: timing only, 2 findings"),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.calls().len(), calls, "the LLM check is cached");
    let run = env.newest_run();
    assert_eq!(run.files[0].checks, [Check::Timing]);
    assert_eq!(run.findings.len(), 2);

    let out = env.subcheck(&["scan", &path("timing.mkv")], &[]);
    assert!(
        stderr(&out).contains("Nothing to check: 1 file unchanged since an earlier scan"),
        "{}",
        stderr(&out)
    );
}

/// The run file's lines as a scan from before the timing check wrote them: no timing findings,
/// file records without their checks, a run record without its languages.
fn as_before_timing(run: &Path) {
    let text = std::fs::read_to_string(run).unwrap();
    let kept: Vec<String> = text
        .lines()
        .filter(|l| !l.contains(r#""source":"timing""#))
        .map(|l| {
            l.replace(r#","checks":["llm","timing"]"#, "")
                .replace(r#","langs":["eng"]"#, "")
        })
        .collect();
    std::fs::write(run, kept.join("\n") + "\n").unwrap();
}

#[test]
fn scan_run_adds_only_the_checks_a_run_lacks() {
    let env = Env::new();
    let files = [path("timing.mkv"), path("typos.mkv")];
    let scan = |extra: &[&str]| {
        let mut args = vec!["scan", files[0].as_str(), files[1].as_str()];
        args.extend(extra);
        env.subcheck(&args, &[])
    };
    assert!(scan(&[]).status.success());
    let run_path = store::find_run(&env.state.path().join("subcheck"), None).unwrap();
    as_before_timing(&run_path);
    let run = env.newest_run();
    // Two LLM workers: either file can finish first and take the first IDs.
    let last_id = run.findings.iter().map(|f| f.id).max().unwrap();
    assert_eq!(run.findings.len(), 2);
    let calls = env.calls().len();

    let out = scan(&["--run", &run.name]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "run {}: added to 2 files, 0 failed; 2 findings (2 timing)",
            run.name
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        env.calls().len(),
        calls,
        "no LLM call: the run has that check"
    );
    let run = env.newest_run();
    let added: Vec<(u64, &str)> = run.findings[2..]
        .iter()
        .map(|f| (f.id, f.source.as_str()))
        .collect();
    assert_eq!(added, [(last_id + 1, "timing"), (last_id + 2, "timing")]);
    assert!(
        run.files
            .iter()
            .all(|f| f.checks == [Check::Llm, Check::Timing])
    );
    // typos.mkv's record keeps the lines its LLM check counted.
    let typos = run.files.iter().find(|f| f.path.ends_with("typos.mkv"));
    assert_eq!(typos.unwrap().lines, 4);
    // The run now says which languages it checked, for the next --run.
    assert_eq!(run.header.unwrap().langs, ["eng"]);

    let out = scan(&["--run", &run.name]);
    assert!(
        stderr(&out).contains(&format!(
            "Nothing to add to run {}: 2 files with every check already",
            run.name
        )),
        "{}",
        stderr(&out)
    );
    let runs = std::fs::read_dir(env.state.path().join("subcheck/runs"))
        .unwrap()
        .count();
    assert_eq!(runs, 1);
}

#[test]
fn scan_run_refuses_another_llm_or_languages_and_leaves_out_a_changed_file() {
    let env = Env::new();
    let (_copy, file) = common::copied("timing.mkv");
    let file = file.to_string_lossy().into_owned();
    assert!(env.subcheck(&["scan", &file], &[]).status.success());
    let name = env.newest_run().name;

    let out = env.subcheck(&["scan", &file, "--run", &name, "--force"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));

    // The test harness passes --llm-cmd with the fake LLM's path, so this --llm conflicts with
    // it; call the binary with --llm alone.
    let out = Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .args(["scan", &file, "--run", &name, "--llm", "gpt"])
        .env("XDG_STATE_HOME", env.state.path())
        .output()
        .unwrap();
    assert!(
        stderr(&out).contains(&format!("run {name} was scanned with cmd:")),
        "{}",
        stderr(&out)
    );
    let out = env.subcheck(&["scan", &file, "--run", &name, "--lang", "und"], &[]);
    assert!(
        stderr(&out).contains(&format!(
            "run {name} checked --lang eng; leave out --lang to use it"
        )),
        "{}",
        stderr(&out)
    );

    let later = std::time::SystemTime::now() + std::time::Duration::from_mins(1);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let out = env.subcheck(&["scan", &file, "--run", &name], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains(&format!(
            "{file}: changed since run {name}; scan it into a new run"
        )),
        "{err}"
    );
    assert!(
        err.contains(&format!(
            "Nothing to add to run {name}: 1 file changed since the run"
        )),
        "{err}"
    );

    // A backend this subcheck does not know, with no command recorded, stops a --run only where a
    // file needs the LLM: here a file new to the run.
    let run_file = env.state.path().join(format!("subcheck/runs/{name}.jsonl"));
    let text = std::fs::read_to_string(&run_file).unwrap();
    let header = text.lines().next().unwrap();
    let mut gone: serde_json::Value = serde_json::from_str(header).unwrap();
    gone["backend"] = "gemini".into();
    gone["command"] = serde_json::json!([]);
    std::fs::write(&run_file, text.replacen(header, &gone.to_string(), 1)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .args(["scan", &file, &path("typos.mkv"), "--run", &name])
        .env("XDG_STATE_HOME", env.state.path())
        .output()
        .unwrap();
    assert!(
        stderr(&out).contains(&format!(
            "run {name} was scanned with gemini, which this subcheck does not know"
        )),
        "{}",
        stderr(&out)
    );
}

#[test]
fn scan_run_does_not_repeat_an_llm_check_the_cache_has_from_another_run() {
    let env = Env::new();
    assert!(
        env.subcheck(&["scan", &path("typos.mkv")], &[])
            .status
            .success()
    );
    let first = env.newest_run().name;
    let runs = env.state.path().join("subcheck/runs");
    // A run that has the file's timing check but not the LLM's, as a plain scan makes when the
    // cache has the one and not the other.
    let text = std::fs::read_to_string(runs.join(format!("{first}.jsonl"))).unwrap();
    let timing_only: Vec<String> = text
        .lines()
        .filter(|l| !l.contains(r#""source":"llm""#))
        .map(|l| l.replace(r#""checks":["llm","timing"]"#, r#""checks":["timing"]"#))
        .collect();
    let other = "2026-01-01T00-00-00";
    std::fs::write(
        runs.join(format!("{other}.jsonl")),
        timing_only.join("\n") + "\n",
    )
    .unwrap();
    let calls = env.calls().len();

    let out = env.subcheck(&["scan", &path("typos.mkv"), "--run", other], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains(&format!(
            "typos.mkv: its LLM check is in run {first}, unchanged since; not made again"
        )),
        "{err}"
    );
    assert!(
        err.contains(&format!(
            "Nothing to add to run {other}: 1 file with the LLM check in another run"
        )),
        "{err}"
    );
    assert_eq!(env.calls().len(), calls);

    // A file new to a run: its timing check is added, its LLM check is not, and the file is not
    // also called left out.
    let empty = "2026-01-02T00-00-00";
    let header = timing_only.first().unwrap();
    std::fs::write(runs.join(format!("{empty}.jsonl")), format!("{header}\n")).unwrap();
    let out = env.subcheck(&["scan", &path("typos.mkv"), "--run", empty], &[]);
    let err = stderr(&out);
    assert!(err.contains("typos.mkv: its LLM check is in run"), "{err}");
    assert!(
        err.contains(&format!(
            "run {empty}: added to 1 file, 0 failed; 0 findings\n"
        )),
        "{err}"
    );
    assert_eq!(env.calls().len(), calls);

    // Once the run the cache names no longer holds the file's LLM record (its lines lost, or the
    // run deleted), the LLM check is made.
    let first_path = runs.join(format!("{first}.jsonl"));
    let first_text = std::fs::read_to_string(&first_path).unwrap();
    let first_header = first_text.lines().next().unwrap();
    std::fs::write(&first_path, format!("{first_header}\n")).unwrap();
    let out = env.subcheck(&["scan", &path("typos.mkv"), "--run", other], &[]);
    assert!(
        stderr(&out).contains(&format!(
            "run {other}: added to 1 file, 0 failed; 2 findings"
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.calls().len(), calls + 1);
}

#[test]
fn a_failure_adding_a_check_leaves_the_files_record_as_it_was() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return; // root reads a 0o000 file anyway (CI containers run as root)
    }
    let env = Env::new();
    let (_copy, file) = common::copied("timing.mkv");
    let name = file.to_string_lossy().into_owned();
    assert!(env.subcheck(&["scan", &name], &[]).status.success());
    let run_path = store::find_run(&env.state.path().join("subcheck"), None).unwrap();
    as_before_timing(&run_path);
    let calls = env.calls().len();

    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&file, mode(0o000)).unwrap();
    let run = env.newest_run().name;
    let out = env.subcheck(&["scan", &name, "--run", &run], &[]);
    std::fs::set_permissions(&file, mode(0o644)).unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("FAILED"), "{}", stderr(&out));
    let record = &env.newest_run().files[0];
    assert_eq!(
        (record.error.as_deref(), record.checks.as_slice()),
        (None, [Check::Llm].as_slice())
    );

    // So the next --run adds the timing check alone, and the LLM's is not made again.
    let out = env.subcheck(&["scan", &name, "--run", &run], &[]);
    assert!(
        stderr(&out).contains("added to 1 file, 0 failed; 2 findings (2 timing)"),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.calls().len(), calls);
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
    assert!(
        env.subcheck(&["scan", &path("typos.mkv")], &[])
            .status
            .success()
    );
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
fn a_truncated_file_is_warned_about_on_every_scan() {
    let env = Env::new();
    // One byte short: the Tags at the end are cut, the subtitles are all there.
    let (_copy, file) = common::copied("typos.mkv");
    let full = std::fs::metadata(&file).unwrap().len();
    common::cut(&file, full - 1);
    let file = file.to_string_lossy().into_owned();
    let warning = format!(
        "warning: {file}: truncated: file has {} of the {full} bytes",
        full - 1
    );
    for _ in 0..2 {
        let out = env.subcheck(&["scan", &file], &[]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(stderr(&out).contains(&warning), "{}", stderr(&out));
        assert!(stderr(&out).contains("1 checked"), "{}", stderr(&out));
    }
    // Not cached: the second scan checked it again (an_unchanged_file_is_skipped_until_forced
    // shows the whole file is skipped).
    assert_eq!(env.calls().len(), 2);
    // The run file keeps it, for list.
    assert!(env.newest_run().files[0].truncated.is_some());
    let list = env.subcheck(&["list"], &[]);
    assert!(
        String::from_utf8_lossy(&list.stdout).contains(&format!(
            "Truncated (replace these copies):\n  {file}: file has {} of the {full} bytes",
            full - 1
        )),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );
    let out = env.subcheck(&["scan", "--dry-run", &file], &[]);
    assert!(stderr(&out).contains(&warning), "{}", stderr(&out));
}

#[test]
fn a_scan_with_nothing_to_check_records_no_run() {
    let env = Env::new();
    assert!(
        env.subcheck(&["scan", &path("typos.mkv")], &[])
            .status
            .success()
    );
    assert!(
        env.subcheck(&["scan", &path("typos.mkv")], &[])
            .status
            .success()
    );
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
    assert!(
        env.subcheck(&["scan", &path("typos.mkv")], &[])
            .status
            .success()
    );
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
    assert!(
        env.subcheck(&["scan", &path("typos.mkv")], &[])
            .status
            .success()
    );
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
fn a_track_tagged_only_in_bcp_47_is_named_by_it() {
    let env = Env::new();
    // multi.mkv's track 3 (`jpn`, `ja`, unnamed) loses its Language to a Void of the same length.
    let language = [0x22, 0xB5, 0x9C, 0x83, b'j', b'p', b'n'];
    let (_dir, file) = common::patched("multi.mkv", &language, 1, |b| {
        b.copy_from_slice(&[0xEC, 0x85, 0, 0, 0, 0, 0]);
    });
    let file = file.to_string_lossy();
    let out = env.subcheck(&["scan", &file, "--lang", "jpn", "-v"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("    track 3 (ja): "),
        "{}",
        stderr(&out)
    );
    let run = env.newest_run();
    let tracks = &run.files[0].tracks;
    assert_eq!(
        tracks
            .iter()
            .map(|t| (t.id, t.language.as_str(), t.language_bcp47.as_deref()))
            .collect::<Vec<_>>(),
        [(3, "eng", Some("ja"))]
    );
    assert_eq!(
        subcheck::list::track_name_in(tracks, 3),
        Some(("(ja)".into(), false))
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
    assert!(
        env.subcheck(&["scan", &path("typos.mkv")], &[])
            .status
            .success()
    );
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

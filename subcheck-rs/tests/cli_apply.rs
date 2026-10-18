//! `subcheck apply` end to end: the real binary on a copy of the apply fixture, with a run whose
//! findings are accepted. The tests that remux need mkvmerge and are skipped without it.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{fixture, have_mkvmerge};
use subcheck::store::{Decision, Finding, Record, RunWriter, State};
use subcheck::text::Occurrence;

/// A finding: style, original, fix, and occurrences as (track, start ms, end ms).
type Line<'a> = (&'a str, &'a str, &'a str, &'a [(usize, i64, i64)]);

/// A copy of a fixture in `media/`, a state dir with one run of findings for it, and the paths.
struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
    file: PathBuf,
}

/// The apply fixture's findings: every kind of fix, one finding not there, and one (10) that the
/// run rejects.
const APPLY_LINES: &[Line] = &[
    (
        "Default",
        "We should leave befor it rains.",
        "We should leave before it rains.",
        &[(2, 1000, 2500)],
    ),
    (
        "Default",
        "Ihave to go.",
        "I have to go.",
        &[(2, 3000, 4500)],
    ),
    (
        "Default",
        "Two rows here and teh second.",
        "Two rows here and the second.",
        &[(2, 5000, 6500)],
    ),
    (
        "Song",
        "We happen share the road",
        "We happen to share the road",
        &[(2, 9000, 10000), (3, 9000, 10000)],
    ),
    ("Sign", "Libary", "Library", &[(3, 2000, 4000)]),
    ("OP", "go trough it", "go through it", &[(3, 5000, 7000)]),
    ("", "Hello tehre", "Hello there", &[(4, 1005, 2500)]),
    (
        "",
        "A line split acros two rows",
        "A line split across two rows",
        &[(4, 3000, 4000)],
    ),
    (
        "Default",
        "Something missing.",
        "Something mising.",
        &[(2, 20000, 21000)],
    ),
    (
        "Default",
        "Nothing wrong here.",
        "Nothing wrong hear.",
        &[(2, 7000, 8500)],
    ),
];

/// Records of findings for `path`, numbered from `first`, each accepted but number `rejected`.
fn records(path: &Path, lines: &[Line], first: u64, rejected: u64) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, (style, original, fix, at)) in lines.iter().enumerate() {
        let id = first + i as u64;
        out.push(Record::Finding(Finding {
            id,
            file: path.to_string_lossy().into_owned(),
            style: (*style).into(),
            original: (*original).into(),
            suggestion: (*fix).into(),
            reason: "typo".into(),
            source: "llm".into(),
            occurrences: at
                .iter()
                .map(|&(track, start_ms, end_ms)| Occurrence {
                    track,
                    start_ms,
                    end_ms,
                })
                .collect(),
            before: vec![],
            after: vec![],
        }));
        out.push(Record::Decision(Decision {
            finding: id,
            state: if id == rejected {
                State::Rejected
            } else {
                State::Accepted
            },
            edited: None,
            at: jiff::Timestamp::UNIX_EPOCH,
        }));
    }
    out
}

impl Setup {
    /// A copy of apply.mkv with the apply findings.
    fn new() -> Setup {
        Setup::with("apply.mkv", APPLY_LINES, 10)
    }

    /// A copy of fixture `name` in `media/`, and a run of `lines` for it.
    fn with(name: &str, lines: &[Line], rejected: u64) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("media")).unwrap();
        let file = root.join("media").join(name);
        std::fs::copy(fixture(name), &file).unwrap();
        let s = Setup {
            _dir: dir,
            root,
            file,
        };
        let mut w = RunWriter::create(&s.state()).unwrap();
        w.append(&records(&s.file, lines, 1, rejected)).unwrap();
        s
    }

    /// More findings, for `path`, in the same run.
    fn add(&self, path: &Path, lines: &[Line], first: u64) {
        let mut w = RunWriter::open(&self.run_file()).unwrap();
        w.append(&records(path, lines, first, 0)).unwrap();
    }

    fn state(&self) -> PathBuf {
        self.root.join("state/subcheck")
    }

    fn apply(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_subcheck"))
            .arg("apply")
            .args(args)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .output()
            .unwrap()
    }

    fn run_file(&self) -> PathBuf {
        let dir = self.state().join("runs");
        std::fs::read_dir(dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
}

/// Track `id` of `file` now, as extract writes it.
fn track(file: &Path, id: usize) -> String {
    let (r, layout) = subcheck::mkv::open(file).unwrap();
    let mut out = Vec::new();
    subcheck::subs::extract(&r, &layout, &layout.tracks[id], false, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn expected(id: usize, ext: &str) -> String {
    std::fs::read_to_string(fixture(&format!("expected/apply.track{id}.{ext}"))).unwrap()
}

#[test]
fn a_dry_run_says_what_would_change_and_writes_nothing() {
    let s = Setup::new();
    let run_before = std::fs::read(s.run_file()).unwrap();
    let o = s.apply(&["--dry-run"]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains("  0:00:01.00  befor → before  ok\n"),
        "{text}"
    );
    assert!(
        text.contains("  0:00:05.00  trough → through  by hand: per-letter karaoke\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "  0:00:20.00  missing. → mising.  not found: the file changed since the scan\n"
        ),
        "{text}"
    );
    assert!(
        !text.contains("hear."),
        "a rejected finding is not a fix: {text}"
    );
    assert!(
        text.contains(
            "    track 2: {\\i1}Ihave{\\i0} to go.\n         now: {\\i1}I have{\\i0} to go.\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with(
            "to apply 7, already applied 0, by hand 1, not found 1; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&s.file).unwrap(),
        std::fs::read(fixture("apply.mkv")).unwrap()
    );
    assert_eq!(std::fs::read(s.run_file()).unwrap(), run_before);
}

#[test]
fn apply_remuxes_checks_and_swaps_keeping_the_original() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let backup = format!("{}.orig", s.file.display());
    assert!(
        text.contains(&format!(
            "  remuxed, verified (quick), swapped; backup: {backup}\n"
        )),
        "{text}"
    );
    assert!(
        text.ends_with(
            "applied 7, already applied 0, by hand 1, not found 1; 1 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        std::fs::read(fixture("apply.mkv")).unwrap()
    );
    assert_eq!(
        track(&s.file, 2),
        expected(2, "ass")
            .replace("befor it", "before it")
            .replace("Ihave", "I have")
            .replace("teh second", "the second")
            .replace("happen share", "happen to share")
    );
    assert_eq!(
        track(&s.file, 3),
        expected(3, "ass")
            .replace("Libary", "Library")
            .replace("happen share", "happen to share")
    );
    assert_eq!(
        track(&s.file, 4),
        expected(4, "srt")
            .replace("tehre", "there")
            .replace("acros ", "across ")
    );
    assert!(!subcheck::apply::remux_path(&s.file).exists());
    let run = subcheck::store::read_run(&s.run_file()).unwrap();
    assert_eq!(run.applied, [1, 2, 3, 4, 5, 7, 8].into());

    // Again: every fix is in already, so nothing is remuxed, and the backup is no obstacle.
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.contains("  0:00:01.00  befor → before  already applied\n"),
        "{text}"
    );
    assert!(
        text.ends_with(
            "applied 0, already applied 7, by hand 1, not found 1; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
}

#[test]
fn no_backup_keeps_nothing_and_checks_every_frame() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    let o = s.apply(&["--no-backup"]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains("  remuxed, verified (full), swapped; no backup kept\n"),
        "{text}"
    );
    assert!(!subcheck::apply::backup_path(&s.file).exists());
}

#[test]
fn an_earlier_backup_is_never_overwritten() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    let backup = subcheck::apply::backup_path(&s.file);
    std::fs::write(&backup, "an earlier backup").unwrap();
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(
        text.contains("exists: an earlier backup is never overwritten"),
        "{text}"
    );
    assert!(text.ends_with("; 0 remuxed, 1 failed\n"), "{text}");
    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        "an earlier backup"
    );
    assert_eq!(
        std::fs::read(&s.file).unwrap(),
        std::fs::read(fixture("apply.mkv")).unwrap()
    );
}

/// An executable script `name` in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    path
}

#[test]
fn a_bad_remux_leaves_the_original_and_no_remux_behind() {
    let s = Setup::new();
    // Says it worked, but writes garbage where the remux goes (-o is its third argument).
    let garbage = script(
        &s.root,
        "garbage",
        "[ \"$1\" = --version ] && exit 0\necho junk > \"$3\"\n",
    );
    let o = s.apply(&["--mkvmerge", garbage.to_str().unwrap()]);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(text.contains("  failed: reading "), "{text}");
    // Fails outright.
    let fails = script(
        &s.root,
        "fails",
        "[ \"$1\" = --version ] && exit 0\necho 'Error: no' >&2\nexit 2\n",
    );
    let o = s.apply(&["--mkvmerge", fails.to_str().unwrap()]);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(
        text.contains("  failed: mkvmerge failed (exit status: 2): Error: no\n"),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&s.file).unwrap(),
        std::fs::read(fixture("apply.mkv")).unwrap()
    );
    assert!(!subcheck::apply::remux_path(&s.file).exists());
    assert!(!subcheck::apply::backup_path(&s.file).exists());
    assert!(subcheck::store::read_run(&s.run_file())
        .unwrap()
        .applied
        .is_empty());
}

#[test]
fn no_mkvmerge_stops_before_any_file() {
    let s = Setup::new();
    let o = s.apply(&["--mkvmerge", s.root.join("none").to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("not found: install mkvtoolnix"));
    assert_eq!(stdout(&o), "");
}

#[test]
fn an_unwritable_directory_points_to_tracks_only() {
    if !have_mkvmerge() {
        return;
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root writes anywhere");
        return;
    }
    let s = Setup::new();
    let media = s.file.parent().unwrap();
    let mode = |m| {
        std::fs::set_permissions(media, std::os::unix::fs::PermissionsExt::from_mode(m)).unwrap();
    };
    mode(0o555);
    let o = s.apply(&[]);
    mode(0o755);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(text.contains("(use --tracks-only)"), "{text}");
}

#[test]
fn tracks_only_then_a_remux_elsewhere_then_check_then_apply_records() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    let out = s.root.join("tracks");
    let o = s.apply(&["--tracks-only", out.to_str().unwrap()]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    for (id, ext) in [(2, "ass"), (3, "ass"), (4, "srt")] {
        let wrote = out.join(format!("apply.track{id}.{ext}"));
        assert!(
            text.contains(&format!("  wrote {}\n", wrote.display())),
            "{text}"
        );
    }
    assert_eq!(
        std::fs::read(&s.file).unwrap(),
        std::fs::read(fixture("apply.mkv")).unwrap()
    );
    // Nothing built yet.
    let o = s.apply(&["--check"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stdout(&o).contains("  failed: not built yet: "),
        "{}",
        stdout(&o)
    );
    // The printed command, run as given, as on the server.
    let command = text.lines().find(|l| l.starts_with("  mkvmerge ")).unwrap();
    let ran = Command::new("sh")
        .arg("-c")
        .arg(command.trim())
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stdout)
    );
    let o = s.apply(&["--check"]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    let remuxed = subcheck::apply::remux_path(&s.file);
    assert!(
        text.contains(&format!("  {} checked (quick): OK\n", remuxed.display())),
        "{text}"
    );
    assert!(
        subcheck::store::read_run(&s.run_file())
            .unwrap()
            .applied
            .is_empty(),
        "check records nothing"
    );
    // The swap, done by hand; then apply finds every fix in and records it.
    std::fs::rename(&remuxed, &s.file).unwrap();
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.ends_with(
            "applied 0, already applied 7, by hand 1, not found 1; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    assert_eq!(
        subcheck::store::read_run(&s.run_file())
            .unwrap()
            .applied
            .len(),
        7
    );
}

#[test]
fn a_run_with_nothing_accepted_has_nothing_to_do() {
    let s = Setup::new();
    let mut w = RunWriter::open(&s.run_file()).unwrap();
    let rejected: Vec<Record> = (1..=10)
        .map(|finding| {
            Record::Decision(Decision {
                finding,
                state: State::Rejected,
                edited: None,
                at: jiff::Timestamp::UNIX_EPOCH,
            })
        })
        .collect();
    w.append(&rejected).unwrap();
    let o = s.apply(&["--dry-run"]);
    assert!(o.status.success());
    assert!(
        stdout(&o).starts_with("No accepted fixes in run "),
        "{}",
        stdout(&o)
    );
}

#[test]
fn files_of_one_name_get_track_files_of_their_own() {
    let s = Setup::new();
    std::fs::create_dir(s.root.join("media/b")).unwrap();
    let other = s.root.join("media/b/apply.mkv");
    std::fs::copy(fixture("apply.mkv"), &other).unwrap();
    s.add(&other, APPLY_LINES, 11);
    let out = s.root.join("tracks");
    let o = s.apply(&["--tracks-only", out.to_str().unwrap()]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    for n in [1, 2] {
        for (id, ext) in [(2, "ass"), (3, "ass"), (4, "srt")] {
            let wrote = out.join(format!("apply.{n}.track{id}.{ext}"));
            assert!(
                text.contains(&format!("  wrote {}\n", wrote.display())),
                "{text}"
            );
            assert!(wrote.exists());
        }
    }
}

#[test]
fn an_ffmpeg_file_with_cues_for_only_some_events_loses_none() {
    if !have_mkvmerge() {
        return;
    }
    let lines: &[Line] = &[(
        "Default",
        "Out of order in the file",
        "Out of order in this file",
        &[(0, 30000, 31000)],
    )];
    let s = Setup::with("ass-ffmpeg.mkv", lines, 0);
    // The last CuePoint (the event at 1:05.07) now names track 2: the Cues lead to only four of
    // the five events, and an ffmpeg file has no statistics to say they are short.
    let mut bytes = std::fs::read(&s.file).unwrap();
    let cue_track_1 = [0xF7, 0x81, 0x01];
    let last = (0..bytes.len())
        .rfind(|&i| bytes[i..].starts_with(&cue_track_1))
        .unwrap();
    bytes[last + 2] = 0x02;
    std::fs::write(&s.file, bytes).unwrap();
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        track(&s.file, 0),
        std::fs::read_to_string(fixture("expected/ass-ffmpeg.track0.ass"))
            .unwrap()
            .replace("in the file", "in this file")
    );
}

#[test]
fn a_replaced_tracks_own_tags_are_carried_over() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    // The Signs/Songs track (3), which the fixes rewrite, given tags of its own: a source note as
    // some releases carry on every track, the language-suffixed statistics ffmpeg leaves, and a
    // comment of two rows with a CRLF between them.
    let xml = s.root.join("tags.xml");
    std::fs::write(
        &xml,
        "<Tags><Tag><Simple><Name>Source</Name><String>JPN Blu-ray</String>\
         <TagLanguageIETF>und</TagLanguageIETF></Simple><Simple><Name>BPS-eng</Name>\
         <String>123</String></Simple><Simple><Name>COMMENT</Name>\
         <String>two&#13;\nrows</String></Simple></Tag></Tags>",
    )
    .unwrap();
    let tagged = s.root.join("tagged.mkv");
    let made = Command::new("mkvmerge")
        .args(["--quiet", "-o"])
        .arg(&tagged)
        .args(["--tags", &format!("3:{}", xml.display())])
        .arg(&s.file)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stdout)
    );
    std::fs::rename(&tagged, &s.file).unwrap();
    let note = "  note: track 3's own tags may not carry over exactly: \
                a tag value is not plain UTF-8 text\n";
    // Said by the dry run already, before anything is written.
    let dry = stdout(&s.apply(&["--dry-run"]));
    assert!(dry.contains(note), "{dry}");
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains("  remuxed, verified (quick), swapped;"),
        "{text}"
    );
    // The CR is noted: XML would not give it back as it is, and nothing checks the tags.
    assert!(text.contains(note), "{text}");
    let bytes = std::fs::read(&s.file).unwrap();
    for kept in [b"JPN Blu-ray".as_slice(), b"BPS-eng"] {
        assert!(
            bytes.windows(kept.len()).any(|w| w == kept),
            "{}",
            String::from_utf8_lossy(kept)
        );
    }
}

#[test]
fn a_file_tracks_only_could_not_write_is_left_alone() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root writes anywhere");
        return;
    }
    let s = Setup::new();
    let out = s.root.join("tracks");
    std::fs::create_dir(&out).unwrap();
    // Someone else's file where the first track would go, read-only: the write fails, and the
    // file must still be there afterwards.
    let theirs = out.join("apply.track2.ass");
    std::fs::write(&theirs, "theirs").unwrap();
    std::fs::set_permissions(&theirs, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
    let o = s.apply(&["--tracks-only", out.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
}

#[test]
fn a_probe_left_by_an_interrupted_run_is_no_obstacle() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    let mut probe = subcheck::apply::remux_path(&s.file).into_os_string();
    probe.push(".link");
    std::fs::write(&probe, "left behind").unwrap();
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!Path::new(&probe).exists());
}

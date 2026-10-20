//! `subcheck apply` end to end: the real binary on a copy of the apply fixture, with a run whose
//! findings are accepted. The tests that remux need mkvmerge and are skipped without it.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{fixture, have_mkvmerge, patch};
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
            timing: None,
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

    /// A copy of fixture `name` in `media/`, scanned with the fake LLM (which finds nothing in
    /// it), and every finding accepted.
    fn scanned(name: &str) -> Setup {
        Setup::scanned_with(name, |_| {})
    }

    /// As [`Setup::scanned`], the copy changed by `change` before the scan.
    fn scanned_with(name: &str, change: impl FnOnce(&Path)) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("media")).unwrap();
        let file = root.join("media").join(name);
        std::fs::copy(fixture(name), &file).unwrap();
        change(&file);
        let s = Setup {
            _dir: dir,
            root,
            file,
        };
        let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
        let o = Command::new(env!("CARGO_BIN_EXE_subcheck"))
            .arg("scan")
            .arg(&s.file)
            .arg("--llm-cmd")
            .arg(format!("'{}'", fake.display()))
            .env("XDG_STATE_HOME", s.root.join("state"))
            .env("FAKE_LLM_LOG", s.root.join("llm-calls.log"))
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let run = subcheck::store::read_run(&s.run_file()).unwrap();
        let accepted: Vec<Record> = run
            .findings
            .iter()
            .map(|f| {
                Record::Decision(Decision {
                    finding: f.id,
                    state: State::Accepted,
                    edited: None,
                    at: jiff::Timestamp::UNIX_EPOCH,
                })
            })
            .collect();
        RunWriter::open(&s.run_file())
            .unwrap()
            .append(&accepted)
            .unwrap();
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
        text.contains(
            "  0:00:05.00  trough → through  by hand: per-letter karaoke (try --karaoke)\n"
        ),
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
fn header_values_mkvmerge_writes_its_own_way_are_no_difference() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    let mut bytes = std::fs::read(&s.file).unwrap();
    // Track 1 (`jpn`, `ja`) tagged only `fra`, which mkvmerge writes as `fre`.
    patch(
        &mut bytes,
        b"\x22\xB5\x9C\x83jpn",
        1,
        b"\x22\xB5\x9C\x83fra",
    );
    patch(&mut bytes, b"\x22\xB5\x9D\x82ja", 1, b"\xEC\x84\0\0\0\0");
    // Track 2, a fixed one (`en` only), tagged only `fr`, which mkvmerge writes as `fre` and `fr`.
    let en = b"\x22\xB5\x9D\x82en";
    patch(&mut bytes, en, 3, b"\x22\xB5\x9C\x82fr");
    // Track 3, fixed too, tagged `iw`, a deprecated tag mkvmerge writes as `he` (and `heb`).
    patch(&mut bytes, en, 2, b"\x22\xB5\x9D\x82iw");
    // Track 4, fixed too, tagged only `xy`, which mkvmerge reads as `und` and refuses as an option.
    patch(&mut bytes, en, 1, b"\x22\xB5\x9C\x82xy");
    // Track 1's name (copied) and fixed track 3's empty, which mkvmerge leaves out; a Void keeps
    // the length.
    let empty = |left: u8| [b"\x53\x6E\x80\xEC".as_slice(), &[0x80 | left]].concat();
    let commentary = b"\x53\x6E\x8ACommentary";
    patch(&mut bytes, commentary, 1, &[empty(8), vec![0; 8]].concat());
    let name = b"\x53\x6E\x93English Signs/Songs";
    patch(&mut bytes, name, 1, &[empty(17), vec![0; 17]].concat());
    std::fs::write(&s.file, bytes).unwrap();
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
    // mkvmerge did write them its own way, and the check took that as no change.
    let (_, layout) = subcheck::mkv::open(&s.file).unwrap();
    let header = |id: usize| {
        let t = &layout.tracks[id];
        (t.language.clone(), t.language_bcp47.clone(), t.name.clone())
    };
    assert_eq!(header(1), ("fre".into(), Some("fr".into()), None));
    assert_eq!(
        header(2),
        ("fre".into(), Some("fr".into()), Some("English Full".into()))
    );
    assert_eq!(header(3), ("heb".into(), Some("he".into()), None));
    assert_eq!(
        header(4),
        ("und".into(), Some("und".into()), Some("English SDH".into()))
    );
}

#[test]
fn a_file_whose_text_tracks_are_timed_by_their_default_duration_is_fixed() {
    if !have_mkvmerge() {
        return;
    }
    // The remux leaves out both tracks' DefaultDuration, the one fixed and the one not.
    let s = Setup::with(
        "timed.mkv",
        &[("", "Over tehre", "Over there", &[(0, 0, 2000)])],
        0,
    );
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.ends_with(
            "applied 1, already applied 0, by hand 0, not found 0; 1 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    let expected = |name| std::fs::read_to_string(fixture(&format!("expected/{name}"))).unwrap();
    assert_eq!(
        track(&s.file, 0),
        expected("timed.track0.srt").replace("tehre", "there")
    );
    assert_eq!(track(&s.file, 1), expected("timed.track1.ass"));
    let o = s.apply(&[]);
    assert!(
        stdout(&o).ends_with(
            "applied 0, already applied 1, by hand 0, not found 0; 0 remuxed, 0 failed\n"
        ),
        "{}",
        stdout(&o)
    );
}

#[test]
fn a_fix_on_a_track_whose_times_its_format_cannot_keep_is_left_for_a_hand_fix() {
    if !have_mkvmerge() {
        return;
    }
    // Track 0's lines last 2 s, which its .srt keeps; track 1's a 23.976 fps frame, which a remux
    // makes 42 ms and its .ass 40. The file's other fix is still made, in the one remux.
    let s = Setup::with(
        "timed.mkv",
        &[
            ("", "Over tehre", "Over there", &[(0, 0, 2000)]),
            ("Default", "Over tehre", "Over there", &[(1, 0, 41)]),
        ],
        0,
    );
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains(
            "  0:00:00.00  tehre → there  by hand: track 1's .ass file cannot keep its times (its lines last the DefaultDuration, 41.708333 ms)\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with(
            "applied 1, already applied 0, by hand 1, not found 0; 1 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    let expected = |name| std::fs::read_to_string(fixture(&format!("expected/{name}"))).unwrap();
    assert_eq!(
        track(&s.file, 0),
        expected("timed.track0.srt").replace("tehre", "there")
    );
    assert_eq!(track(&s.file, 1), expected("timed.track1.ass"));

    // A track timed to the millisecond: nothing left to make, so no remux, and the file as it was.
    let s = Setup::with(
        "ass-sync7.mkv",
        &[(
            "Default",
            "Out of order in the file",
            "Out of order in this file",
            &[(0, 30_007, 31_007)],
        )],
        0,
    );
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.contains(
            "  by hand: track 0's .ass file cannot keep its times (0:00:01.007 is finer than centiseconds)\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with(
            "applied 0, already applied 0, by hand 1, not found 0; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&s.file).unwrap(),
        std::fs::read(fixture("ass-sync7.mkv")).unwrap()
    );
}

/// The apply fixture's per-letter OP row ("go trough it", letters 20 px apart from x 200 in track
/// 3) as `--karaoke` leaves it: an h after the t, the row spread 10 px each way about its centre.
fn karaoke_row() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, c) in "go through it".chars().enumerate() {
        let x = 190 + 20 * i;
        let c = c.to_string().replace(' ', "");
        writeln!(
            out,
            "Dialogue: 0,0:00:05.00,0:00:07.00,OP,,0,0,0,fx,{{\\an5\\pos({x},40)}}{c}"
        )
        .unwrap();
    }
    out
}

#[test]
fn karaoke_is_re_spaced_with_the_flag() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::new();
    let dry = stdout(&s.apply(&["--dry-run", "--karaoke"]));
    assert!(
        dry.contains(
            "  0:00:05.00  trough → through  karaoke re-spaced by +20 px: check it (review, o)\n"
        ),
        "{dry}"
    );
    assert!(
        dry.contains(
            "    track 3: go trough it\n         now: go through it (karaoke letters: 1 added, 12 moved)\n"
        ),
        "{dry}"
    );
    // Remuxed elsewhere from the tracks it writes: the check needs --karaoke too, and says so.
    let out = s.root.join("tracks");
    let text = stdout(&s.apply(&["--tracks-only", out.to_str().unwrap(), "--karaoke"]));
    assert!(text.ends_with(" --check --karaoke\n"), "{text}");
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
    let o = s.apply(&["--check", "--karaoke"]);
    assert!(o.status.success(), "{}", stdout(&o));
    assert_eq!(s.apply(&["--check"]).status.code(), Some(1));
    std::fs::remove_file(subcheck::apply::remux_path(&s.file)).unwrap();
    let o = s.apply(&["--karaoke"]);
    let text = stdout(&o);
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.ends_with(
            "applied 8, already applied 0, by hand 0, not found 1; 1 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    let before = expected(3, "ass");
    let start = before.find("Dialogue: 0,0:00:05.00").unwrap();
    let end = before.rfind("}t\n").unwrap() + 3;
    let after = format!("{}{}{}", &before[..start], karaoke_row(), &before[end..])
        .replace("Libary", "Library")
        .replace("happen share", "happen to share");
    assert_eq!(track(&s.file, 3), after);
    // In now: a second run finds it so, karaoke or not.
    let text = stdout(&s.apply(&[]));
    assert!(
        text.contains("  0:00:05.00  trough → through  already applied\n"),
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
    // Each reads no languages (`-J`), as mkvmerge reads a file it knows no tracks in.
    let known = "[ \"$1\" = --version ] && exit 0\n[ \"$1\" = -J ] && echo '{}' && exit 0\n";
    // Says it worked, but writes garbage where the remux goes (-o is its third argument).
    let garbage = script(&s.root, "garbage", &format!("{known}echo junk > \"$3\"\n"));
    let o = s.apply(&["--mkvmerge", garbage.to_str().unwrap()]);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(text.contains("  failed: reading "), "{text}");
    // Fails outright.
    let fails = script(
        &s.root,
        "fails",
        &format!("{known}echo 'Error: no' >&2\nexit 2\n"),
    );
    let o = s.apply(&["--mkvmerge", fails.to_str().unwrap()]);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(
        text.contains("  failed: mkvmerge failed (exit status: 2): Error: no\n"),
        "{text}"
    );
    // Cannot read the original's languages: told before any remux.
    let unread = script(
        &s.root,
        "unread",
        "[ \"$1\" = --version ] && exit 0\n[ \"$1\" = -J ] && echo 'Error: cannot read' >&2 && exit 2\necho junk > \"$3\"\n",
    );
    let o = s.apply(&["--mkvmerge", unread.to_str().unwrap()]);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(
        text.contains("  failed: mkvmerge -J failed (exit status: 2): Error: cannot read\n"),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&s.file).unwrap(),
        std::fs::read(fixture("apply.mkv")).unwrap()
    );
    assert!(!subcheck::apply::remux_path(&s.file).exists());
    assert!(!subcheck::apply::backup_path(&s.file).exists());
    assert!(
        subcheck::store::read_run(&s.run_file())
            .unwrap()
            .applied
            .is_empty()
    );
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
    if rustix::process::geteuid().is_root() {
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
    let name = s
        .run_file()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        text.ends_with(&format!(
            "\nOnce they are remuxed, check them with: subcheck apply {name} --check\n"
        )),
        "{text}"
    );
    // A run given as a file elsewhere is named by its path, quoted for the shell.
    let elsewhere = s.root.join("a run.jsonl");
    std::fs::copy(s.run_file(), &elsewhere).unwrap();
    let other = s.root.join("other tracks");
    let text = stdout(&s.apply(&[
        elsewhere.to_str().unwrap(),
        "--tracks-only",
        other.to_str().unwrap(),
    ]));
    assert!(
        text.ends_with(&format!(
            "check them with: subcheck apply '{}' --check\n",
            elsewhere.display()
        )),
        "{text}"
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
    if rustix::process::geteuid().is_root() {
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

/// A typo on timing.mkv's late track 2, in its first line: the line the timing fix is found by.
const LATE_TYPO: &[Line] = &[(
    "Default",
    "Line 0 on the cut.",
    "Line zero on the cut.",
    &[(2, 500, 7250)],
)];

/// `subcheck timing` on `file`.
fn timing(file: &Path) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_subcheck"))
        .arg("timing")
        .arg(file)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    stdout(&o)
}

#[test]
fn timing_fixes_shift_their_tracks_with_the_typos_in_one_remux() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::scanned("timing.mkv");
    s.add(&s.file, LATE_TYPO, 3);
    let o = s.apply(&["--dry-run"]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.contains(
            "  0:00:00.50  0 → zero  ok\n\
             \x20 track 2  1 frame late → shift 1 frame earlier  ok (68 events moved)\n\
             \x20   track 2: 0:00:00.50 – 0:00:07.25 → 0:00:00.00 – 0:00:06.75  Line 0 on the cut.\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("  track 4  1 frame late → shift 1 frame earlier  ok ("),
        "{text}"
    );
    assert!(
        text.ends_with(
            "to apply 3, already applied 0, by hand 0, not found 0; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.ends_with(
            "applied 3, already applied 0, by hand 0, not found 0; 1 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    assert_eq!(
        subcheck::store::read_run(&s.run_file())
            .unwrap()
            .applied
            .len(),
        3
    );
    let now = track(&s.file, 2);
    assert!(
        now.contains("Dialogue: 0,0:00:00.00,0:00:06.75,Default,,0,0,0,,Line zero on the cut.\n"),
        "{now}"
    );
    let report = timing(&s.file);
    assert!(!report.contains("fix:"), "{report}");
    assert!(
        report.contains("track 2  Dialogue a frame late  in sync"),
        "{report}"
    );
    // Run again: every fix is in, the typo found at its line's new times.
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.ends_with(
            "applied 0, already applied 3, by hand 0, not found 0; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
}

#[test]
fn shifted_tracks_written_remuxed_elsewhere_then_checked() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::scanned("timing.mkv");
    let out = s.root.join("tracks");
    let text = stdout(&s.apply(&["--tracks-only", out.to_str().unwrap()]));
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
    assert!(!timing(&remuxed).contains("fix:"));
}

#[test]
fn a_timing_fix_that_cannot_be_made_is_left_for_a_hand_fix() {
    // A file whose video has too few keyframes to give the frame grid.
    let s = Setup::with("apply.mkv", &[], 0);
    let mut late = subcheck::store::read_run(&Setup::scanned("timing.mkv").run_file())
        .unwrap()
        .findings
        .remove(0);
    late.id = 1;
    late.file = s.file.to_string_lossy().into_owned();
    // Its one stretch's ends: apply.mkv's first line on track 2, where the scan found it.
    let first = subcheck::store::LineRef {
        raw: "0,Default,,0,0,0,,We should leave befor it rains.".into(),
        start_ms: 1000,
        end_ms: 2500,
    };
    let fix = late.timing.as_mut().unwrap();
    fix.stretches.truncate(1);
    fix.stretches[0].first = Some(first.clone());
    fix.stretches[0].last = Some(first);
    let accepted = |finding| {
        Record::Decision(Decision {
            finding,
            state: State::Accepted,
            edited: None,
            at: jiff::Timestamp::UNIX_EPOCH,
        })
    };
    let mut w = RunWriter::open(&s.run_file()).unwrap();
    w.append(&[Record::Finding(late.clone()), accepted(1)])
        .unwrap();
    // A typo on that line: with the fix's lines where the scan found them, no grid is needed to
    // know it is there too.
    s.add(&s.file, &APPLY_LINES[..1], 2);
    let text = stdout(&s.apply(&["--dry-run"]));
    assert!(
        text.contains(
            "  track 2  1 frame late → shift 1 frame earlier  by hand: only 2 video keyframes in the Cues\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("  0:00:01.00  befor → before  ok\n"),
        "{text}"
    );
    // Two timing findings on one track: neither is made.
    let s = Setup::scanned("timing.mkv");
    let mut again = subcheck::store::read_run(&s.run_file()).unwrap().findings[0].clone();
    again.id = 3;
    let mut w = RunWriter::open(&s.run_file()).unwrap();
    w.append(&[Record::Finding(again), accepted(3)]).unwrap();
    let text = stdout(&s.apply(&["--dry-run"]));
    let hand =
        "  track 2  1 frame late → shift 1 frame earlier  by hand: two timing fixes on one track\n";
    assert_eq!(text.matches(hand).count(), 2, "{text}");
    assert!(
        text.contains("  track 4  1 frame late → shift 1 frame earlier  ok ("),
        "{text}"
    );
}

#[test]
fn a_typo_is_looked_for_at_shifted_times_only_on_a_track_shifted_already() {
    // Recorded a frame after where the line is: on the track as it is, not yet shifted, it is not
    // there, though the line sits where the shift would move its times.
    let s = Setup::scanned("timing.mkv");
    let early: &[Line] = &[(
        "Default",
        "Line 0 on the cut.",
        "Line zero on the cut.",
        &[(2, 1000, 7750)],
    )];
    s.add(&s.file, early, 3);
    let not_found = "  0:00:01.00  0 → zero  not found: the file changed since the scan\n";
    let text = stdout(&s.apply(&["--dry-run"]));
    assert!(text.contains(not_found), "{text}");
    // Nor with the track's timing fix left for a hand fix: two of them on it.
    let mut again = subcheck::store::read_run(&s.run_file()).unwrap().findings[0].clone();
    again.id = 4;
    let accepted = Record::Decision(Decision {
        finding: 4,
        state: State::Accepted,
        edited: None,
        at: jiff::Timestamp::UNIX_EPOCH,
    });
    let mut w = RunWriter::open(&s.run_file()).unwrap();
    w.append(&[Record::Finding(again), accepted]).unwrap();
    // Nor with the track's two timing fixes left for a hand fix: not knowing whether the track
    // is shifted, a line they move is left for one too.
    let text = stdout(&s.apply(&["--dry-run"]));
    assert!(
        text.contains("by hand: two timing fixes on one track"),
        "{text}"
    );
    let unsure = "by hand: its track's timing fix leaves its time unknown\n";
    assert!(
        text.contains(&format!("  0:00:01.00  0 → zero  {unsure}")),
        "{text}"
    );
}

#[test]
fn a_typo_on_a_line_a_timing_fix_reading_not_found_moves_is_left_for_a_hand_fix() {
    if !have_mkvmerge() {
        return;
    }
    let s = Setup::scanned("timing.mkv");
    assert!(s.apply(&[]).status.success());
    // Track 2's first line, as the run recorded it, reads otherwise now (its style changed by
    // hand, say): its timing fix reads neither in nor to do, and a typo on a line it moves could
    // be at either time.
    let run = std::fs::read_to_string(s.run_file()).unwrap();
    let changed = run.replacen(
        "\"raw\":\"0,Default,,0,0,0,,Line 0 on the cut.\"",
        "\"raw\":\"0,Sign,,0,0,0,,Line 0 on the cut.\"",
        1,
    );
    assert_ne!(changed, run);
    std::fs::write(s.run_file(), changed).unwrap();
    s.add(&s.file, LATE_TYPO, 3);
    // At the times where the line is now, too.
    let where_now: &[Line] = &[(
        "Default",
        "Line 0 on the cut.",
        "Line nought on the cut.",
        &[(2, 0, 6750)],
    )];
    s.add(&s.file, where_now, 4);
    let text = stdout(&s.apply(&["--dry-run"]));
    assert!(
        text.contains("  track 2  1 frame late → shift 1 frame earlier  not found: "),
        "{text}"
    );
    let unsure = "by hand: its track's timing fix leaves its time unknown\n";
    assert!(
        text.contains(&format!("  0:00:00.50  0 → zero  {unsure}")),
        "{text}"
    );
    assert!(
        text.contains(&format!("  0:00:00.00  0 → nought  {unsure}")),
        "{text}"
    );
}

#[test]
fn a_timing_fix_landing_a_track_timed_to_the_millisecond_on_centiseconds_is_made() {
    if !have_mkvmerge() {
        return;
    }
    // Track 2, a frame late, 7 ms later still: no line of it is on a centisecond. Shifted, every
    // line is, so the track can be written out.
    let s = Setup::scanned_with("timing.mkv", |file| {
        let synced = file.with_extension("synced.mkv");
        let o = Command::new("mkvmerge")
            .arg("-o")
            .arg(&synced)
            .args(["--sync", "2:7"])
            .arg(file)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        std::fs::rename(&synced, file).unwrap();
    });
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.contains("  track 2  2 frames late → shift 2 frames earlier  ok (68 events moved)\n"),
        "{text}"
    );
    assert!(text.contains("; 1 remuxed, 0 failed\n"), "{text}");
    assert!(
        track(&s.file, 2)
            .contains("Dialogue: 0,0:00:00.00,0:00:06.26,Default,,0,0,0,,Line 0 on the cut.\n"),
        "{}",
        track(&s.file, 2)
    );
}

/// Make one line of track `id` of `file` Latin-1: its first block's last byte. The track must be
/// stored uncompressed.
fn latin1_line(file: &Path, id: usize) {
    let (r, layout) = subcheck::mkv::open(file).unwrap();
    let read = subcheck::blocks::read_tracks(&r, &layout, &[layout.tracks[id].number], false)
        .unwrap()
        .pop()
        .unwrap();
    let first = &read.blocks[0];
    let mut bytes = std::fs::read(file).unwrap();
    // Inside its BlockGroup, which starts at its position.
    let from = usize::try_from(first.position).unwrap();
    let at = bytes[from..]
        .windows(first.payload.len())
        .position(|w| w == first.payload)
        .unwrap();
    bytes[from + at + first.payload.len() - 1] = 0xE9;
    std::fs::write(file, bytes).unwrap();
}

#[test]
fn a_timing_fix_on_a_track_that_is_not_all_utf8_is_left_for_a_hand_fix() {
    if !have_mkvmerge() {
        return;
    }
    // Written out, the Latin-1 line would come back changed; remuxed at all, too, so track 4's fix
    // is left as well.
    let s = Setup::scanned_with("timing.mkv", |file| latin1_line(file, 2));
    let o = s.apply(&["--dry-run"]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.contains(
            "  track 2  1 frame late → shift 1 frame earlier  by hand: track 2 is not all UTF-8, which any remux rewrites\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "  track 4  1 frame late → shift 1 frame earlier  by hand: track 2 is not all UTF-8, which any remux rewrites\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with(
            "to apply 0, already applied 0, by hand 2, not found 0; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    // Nothing is written.
    let out = s.root.join("tracks");
    let text = stdout(&s.apply(&["--tracks-only", out.to_str().unwrap()]));
    assert!(!text.contains("  wrote "), "{text}");
}

#[test]
fn a_file_with_a_text_track_that_is_not_all_utf8_is_left_for_hand_fixes() {
    if !have_mkvmerge() {
        return;
    }
    // A Latin-1 line in track 4, which no fix touches: any remux rewrites it, so the fix on track
    // 2 is left for a hand fix too, and the file as it was.
    let s = Setup::with(
        "apply.mkv",
        &[(
            "Default",
            "We should leave befor it rains.",
            "We should leave before it rains.",
            &[(2, 1000, 2500)],
        )],
        0,
    );
    latin1_line(&s.file, 4);
    let changed = std::fs::read(&s.file).unwrap();
    let o = s.apply(&[]);
    let text = stdout(&o);
    assert!(o.status.success(), "{text}");
    assert!(
        text.contains(
            "  0:00:01.00  befor → before  by hand: track 4 is not all UTF-8, which any remux rewrites\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with(
            "applied 0, already applied 0, by hand 1, not found 0; 0 remuxed, 0 failed\n"
        ),
        "{text}"
    );
    assert_eq!(std::fs::read(&s.file).unwrap(), changed);
}

#[test]
fn a_webvtt_track_that_is_not_all_utf8_counts_too() {
    if !have_mkvmerge() {
        return;
    }
    // Any S_TEXT track: mkvmerge rewrites a WebVTT line that is not UTF-8 as it does an ASS one.
    let s = Setup::with(
        "apply.mkv",
        &[(
            "Default",
            "We should leave befor it rains.",
            "We should leave before it rains.",
            &[(2, 1000, 2500)],
        )],
        0,
    );
    let vtt = s.root.join("notes.vtt");
    std::fs::write(&vtt, "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nA note\n").unwrap();
    let with_vtt = s.root.join("with-vtt.mkv");
    let o = Command::new("mkvmerge")
        .arg("-o")
        .arg(&with_vtt)
        .arg(&s.file)
        .arg(&vtt)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    std::fs::rename(&with_vtt, &s.file).unwrap();
    latin1_line(&s.file, 5);
    let text = stdout(&s.apply(&["--dry-run"]));
    assert!(
        text.contains(
            "  0:00:01.00  befor → before  by hand: track 5 is not all UTF-8, which any remux rewrites\n"
        ),
        "{text}"
    );
}

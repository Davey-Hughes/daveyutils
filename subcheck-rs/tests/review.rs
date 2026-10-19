//! The review's decisions through a real run file: written by `review::run_effect`, read back by
//! `store::read_run` and `list`. The screen itself is not drawn here; `review::view` has its own
//! tests.

use std::collections::{HashMap, VecDeque};

use crossterm::event::KeyCode;
use subcheck::list::{self, Filter};
use subcheck::player::Player;
use subcheck::review::model::Model;
use subcheck::review::run_effect;
use subcheck::review::update::{self, Msg};
use subcheck::store::{self, Check, FileRecord, Finding, Record, RunWriter, State, TrackRecord};
use subcheck::text::Occurrence;

const FILE: &str = "/m/K-On!/K-On! - E01 - Episode.mkv";
const OTHER: &str = "/m/K-On!/K-On! - E02 - Episode.mkv";

fn finding(id: u64, file: &str, original: &str, suggestion: &str) -> Record {
    Record::Finding(Finding {
        id,
        file: file.into(),
        style: "Dialogue".into(),
        original: original.into(),
        suggestion: suggestion.into(),
        reason: "typo".into(),
        source: "llm".into(),
        occurrences: vec![Occurrence {
            track: 2,
            start_ms: 10_000,
            end_ms: 12_000,
        }],
        before: vec![],
        after: vec![],
        timing: None,
    })
}

fn file(path: &str) -> Record {
    Record::File(FileRecord {
        path: path.into(),
        size: 1,
        mtime: jiff::Timestamp::UNIX_EPOCH,
        error: None,
        tracks: vec![TrackRecord {
            id: 2,
            name: Some("English".into()),
            language: "eng".into(),
            sid: 1,
            default: true,
            read: "cues".into(),
        }],
        lines: 10,
        truncated: None,
        checks: vec![Check::Llm],
    })
}

/// The event loop's inner part: a message, then every effect it leads to, run for real.
fn send(m: &mut Model, msg: Msg, writer: &mut RunWriter, player: &mut Player) {
    let now = jiff::Timestamp::now();
    let mut queue: VecDeque<_> = update::update(m, msg, now).into();
    while let Some(effect) = queue.pop_front() {
        let msg = run_effect(effect, writer, player);
        queue.extend(update::update(m, msg, now));
    }
}

#[test]
fn decisions_made_in_review_are_in_the_run_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut scan = RunWriter::create(dir.path()).unwrap();
    scan.append(&[
        file(FILE),
        file(OTHER),
        finding(1, FILE, "gets trough", "gets through"),
        finding(2, OTHER, "gets trough", "gets through"),
        finding(3, OTHER, "befor it", "before it"),
    ])
    .unwrap();
    let path = scan.path.clone();
    let mut model = Model::new(store::read_run(&path).unwrap(), HashMap::new());
    let mut writer = RunWriter::open(&path).unwrap();
    let mut player = Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock"));
    let key = |c| Msg::Key(KeyCode::Char(c));

    // Accept the group of two, then reword the other finding.
    send(&mut model, key('a'), &mut writer, &mut player);
    send(&mut model, key('e'), &mut writer, &mut player);
    send(&mut model, Msg::KillLine, &mut writer, &mut player);
    for c in "Before it.".chars() {
        send(&mut model, key(c), &mut writer, &mut player);
    }
    send(
        &mut model,
        Msg::Key(KeyCode::Enter),
        &mut writer,
        &mut player,
    );

    let run = store::read_run(&path).unwrap();
    assert_eq!([1, 2, 3].map(|id| run.state_of(id)), [State::Accepted; 3]);
    assert_eq!(run.decisions[&3].edited.as_deref(), Some("Before it."));
    let text = list::render(&run, Filter::Only(State::Accepted), false);
    assert!(
        text.contains("      - befor it\n      + Before it.\n"),
        "{text}"
    );

    // Undo is a write too, and the newest decision wins on reading back.
    send(&mut model, key('u'), &mut writer, &mut player);
    let run = store::read_run(&path).unwrap();
    assert_eq!(run.state_of(3), State::Pending);
    assert_eq!(run.state_of(1), State::Accepted);

    // With no mpv to start, `o` says so in the status line.
    model.filter = Filter::All;
    send(&mut model, key('o'), &mut writer, &mut player);
    assert!(
        model
            .status
            .as_deref()
            .is_some_and(|s| s.contains("not found")),
        "{:?}",
        model.status
    );
}

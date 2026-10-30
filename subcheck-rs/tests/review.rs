//! The review's decisions through a real store: written by `review::run_effect`, read back as the
//! run's view and by `list`. The screen itself is not drawn here; `review::view` has its own tests.

use std::collections::{HashMap, VecDeque};

use crossterm::event::KeyCode;
use subcheck::list::{self, Filter};
use subcheck::player::Player;
use subcheck::review::model::Model;
use subcheck::review::run_effect;
use subcheck::review::update::{self, Msg};
use subcheck::store::{
    Attempt, CheckedWith, Finding, Made, RunHeader, ScanRow, State, Store, TrackRecord,
};
use subcheck::text::Occurrence;

const FILE: &str = "/m/K-On!/K-On! - E01 - Episode.mkv";
const OTHER: &str = "/m/K-On!/K-On! - E02 - Episode.mkv";

fn finding(file: &str, original: &str, suggestion: &str) -> Finding {
    Finding {
        id: 0,
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
    }
}

/// An LLM check of `path` by `scan`, as the scan saves it, with `findings`.
fn checked(store: &mut Store, scan: &ScanRow, path: &str, findings: Vec<Finding>) {
    let tracks = [TrackRecord {
        id: 2,
        name: Some("English".into()),
        language: "eng".into(),
        language_bcp47: None,
        sid: 1,
        default: true,
        read: "cues".into(),
    }];
    let attempt = Attempt {
        scan: scan.id,
        path,
        size: 1,
        mtime: jiff::Timestamp::UNIX_EPOCH,
        tracks: &tracks,
        truncated: None,
        at: jiff::Timestamp::UNIX_EPOCH,
    };
    let with = CheckedWith::new("claude", &["eng".into()], 1);
    store
        .save(
            &attempt,
            vec![Made {
                with,
                lines: 10,
                findings,
            }],
        )
        .unwrap();
}

/// The event loop's inner part: a message, then every effect it leads to, run for real.
fn send(m: &mut Model, msg: Msg, store: &mut Store, player: &mut Player) {
    let now = jiff::Timestamp::now();
    let mut queue: VecDeque<_> = update::update(m, msg, now).into();
    while let Some(effect) = queue.pop_front() {
        let msg = run_effect(effect, store, player);
        queue.extend(update::update(m, msg, now));
    }
}

#[test]
fn decisions_made_in_review_are_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_at(&dir.path().join("subcheck.db")).unwrap();
    let header = RunHeader {
        started: jiff::Timestamp::UNIX_EPOCH,
        backend: "claude".into(),
        command: vec![],
        prompt_version: 1,
        version: "0".into(),
        langs: vec!["eng".into()],
    };
    let scan = store
        .begin_scan(&header, &[FILE.into(), OTHER.into()])
        .unwrap();
    checked(
        &mut store,
        &scan,
        FILE,
        vec![finding(FILE, "gets trough", "gets through")],
    );
    checked(
        &mut store,
        &scan,
        OTHER,
        vec![
            finding(OTHER, "gets trough", "gets through"),
            finding(OTHER, "befor it", "before it"),
        ],
    );
    let mut model = Model::new(store.run_view(&scan).unwrap(), HashMap::new());
    let mut player = Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock"));
    let key = |c| Msg::Key(KeyCode::Char(c));

    // Accept the group of two, then reword the other finding.
    send(&mut model, key('a'), &mut store, &mut player);
    send(&mut model, key('e'), &mut store, &mut player);
    send(&mut model, Msg::KillLine, &mut store, &mut player);
    for c in "Before it.".chars() {
        send(&mut model, key(c), &mut store, &mut player);
    }
    send(
        &mut model,
        Msg::Key(KeyCode::Enter),
        &mut store,
        &mut player,
    );

    let run = store.run_view(&scan).unwrap();
    assert_eq!([1, 2, 3].map(|id| run.state_of(id)), [State::Accepted; 3]);
    assert_eq!(run.decisions[&3].edited.as_deref(), Some("Before it."));
    let text = list::render(&run, Filter::Only(State::Accepted), false);
    assert!(
        text.contains("      - befor it\n      + Before it.\n"),
        "{text}"
    );

    // Undo is a write too, and the newest decision wins on reading back.
    send(&mut model, key('u'), &mut store, &mut player);
    let run = store.run_view(&scan).unwrap();
    assert_eq!(run.state_of(3), State::Pending);
    assert_eq!(run.state_of(1), State::Accepted);

    // With no mpv to start, `o` says so in the status line.
    model.filter = Filter::All;
    send(&mut model, key('o'), &mut store, &mut player);
    assert!(
        model
            .status
            .as_deref()
            .is_some_and(|s| s.contains("not found")),
        "{:?}",
        model.status
    );
}

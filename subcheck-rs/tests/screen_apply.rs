//! Apply run by review's screen (`review::jobs::apply`), its messages fed to the model as the
//! screen's loop feeds them: `D` marks what apply would do and writes nothing; `A`, once
//! confirmed, makes the fixes, shows them applied from the store, and moves the file's checks with
//! it, so the next scan has nothing to check.

mod common;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use subcheck::llm::Backend;
use subcheck::player::Player;
use subcheck::review::model::Model;
use subcheck::review::run_effect;
use subcheck::review::update::{self, Msg};
use subcheck::scan::{self, Planned};
use subcheck::store::{Decision, Selector, State, Store};

/// The loop's inner part: a message, then every effect it leads to, run for real.
fn send(
    m: &mut Model,
    msg: Msg,
    store: &mut Store,
    player: &mut Player,
    state: &Path,
    tx: &mpsc::Sender<Msg>,
) {
    let now = jiff::Timestamp::now();
    let mut queue: VecDeque<_> = update::update(m, msg, now).into();
    while let Some(effect) = queue.pop_front() {
        let msg = run_effect(effect, store, player, state, tx);
        queue.extend(update::update(m, msg, now));
    }
}

/// The screen and what its loop works with.
struct Screen {
    m: Model,
    store: Store,
    player: Player,
    state: PathBuf,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl Screen {
    fn key(&mut self, c: char) {
        send(
            &mut self.m,
            Msg::Key(KeyCode::Char(c)),
            &mut self.store,
            &mut self.player,
            &self.state,
            &self.tx,
        );
    }

    /// Feed the screen apply's messages until it ends.
    fn until_applied(&mut self) {
        loop {
            let msg = self
                .rx
                .recv_timeout(Duration::from_mins(1))
                .expect("apply goes on");
            let ended = matches!(msg, Msg::ApplyEnded(_));
            send(
                &mut self.m,
                msg,
                &mut self.store,
                &mut self.player,
                &self.state,
                &self.tx,
            );
            if ended {
                return;
            }
        }
    }

    /// Each finding's mark, in the list's order of findings.
    fn marks(&self) -> Vec<Option<&'static str>> {
        (0..self.m.run.findings.len())
            .map(|i| self.m.mark(i))
            .collect()
    }
}

#[test]
fn d_then_a_on_the_screen() {
    if !common::have_mkvmerge() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_at(&dir.path().join("subcheck.db")).unwrap();
    let file = dir.path().join("Show - E01.mkv");
    std::fs::copy(common::fixture("typos.mkv"), &file).unwrap();
    // The fake LLM finds "befor" (a plain fix) and "trough" (in per-letter karaoke, left by hand
    // without --karaoke).
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
        std::slice::from_ref(&file),
        &opts,
        &mut store,
        &mut std::io::sink(),
    )
    .unwrap() else {
        panic!("nothing to check");
    };
    scan::execute(plan, &opts, &mut store, &mut std::io::sink(), &|_| {}).unwrap();
    let run = store.view(&Selector::Newest).unwrap();
    let accepted: Vec<Decision> = run
        .findings
        .iter()
        .map(|f| Decision {
            finding: f.id,
            state: State::Accepted,
            edited: None,
            at: jiff::Timestamp::now(),
        })
        .collect();
    store.decide(&accepted).unwrap();
    let (tx, rx) = mpsc::channel();
    let mut s = Screen {
        m: Model::new(store.view(&Selector::Newest).unwrap(), HashMap::new()),
        store,
        player: Player::new(dir.path().join("no-mpv"), dir.path().join("mpv.sock")),
        state: dir.path().join("state"),
        tx,
        rx,
    };
    let stamp = scan::file_stamp(&file).unwrap();

    // D: what apply would do, and nothing written.
    s.key('D');
    s.until_applied();
    assert_eq!(s.marks(), [Some("would apply"), Some("would be by hand")]);
    assert_eq!(scan::file_stamp(&file).unwrap(), stamp);
    assert!(s.store.view(&Selector::Newest).unwrap().applied.is_empty());

    // A, then y: made, and shown applied from the store.
    s.key('A');
    assert_eq!(
        s.m.status_text(Rect::default()).as_deref(),
        Some("apply 2 fixes in 1 file, keeping each .orig? y/n")
    );
    s.key('y');
    s.until_applied();
    assert_eq!(s.marks(), [Some("applied"), Some("by hand")]);
    assert_eq!(s.store.view(&Selector::Newest).unwrap().applied.len(), 1);
    assert_eq!(
        s.m.progress.files[0].label('⠙'),
        "✓ swapped · 1 applied · 1 by hand"
    );
    let now = scan::file_stamp(&file).unwrap();
    assert_ne!(now, stamp);
    assert_eq!((s.m.run.files[0].size, s.m.run.files[0].mtime), now);
    assert!(dir.path().join("Show - E01.mkv.orig").exists());
    // The swapped file's checks moved with it.
    assert!(matches!(
        scan::plan(
            std::slice::from_ref(&file),
            &opts,
            &mut s.store,
            &mut std::io::sink()
        )
        .unwrap(),
        Planned::Nothing(_)
    ));
}

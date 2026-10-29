//! What `scan` tells its hook (`scan::execute`), as review's screen reads it, checked against the
//! run file the scan writes. tests/fake-llm.sh is the LLM.

mod common;

use std::path::PathBuf;
use std::sync::Mutex;

use common::fixture;
use subcheck::llm::Backend;
use subcheck::scan::{self, Event, Planned};
use subcheck::store::Record;

fn options() -> scan::Options {
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-llm.sh");
    let backend = Backend::shell(&format!("'{}'", fake.display()));
    scan::Options {
        chunk_lines: backend.chunk_lines,
        backend,
        jobs: 2,
        langs: vec![scan::DEFAULT_LANG.to_string()],
        force: false,
        verbose: false,
        into: None,
    }
}

#[test]
fn the_hook_hears_every_file_and_exactly_the_records_the_run_gets() {
    let state = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Two copies of one file: the second's lines were all sent with the first, so it waits on the
    // first's chunk.
    let files: Vec<PathBuf> = ["Show - E01.mkv", "Show - E02.mkv"]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
    for f in &files {
        std::fs::copy(fixture("typos.mkv"), f).unwrap();
    }
    let opts = options();
    let Planned::Scan(plan) = scan::plan(
        &[dir.path().to_path_buf()],
        &opts,
        state.path(),
        &mut std::io::sink(),
    )
    .unwrap() else {
        panic!("nothing to check");
    };
    let run_path = plan.run_path().to_path_buf();
    let heard = Mutex::new(Vec::new());
    let summary = scan::execute(plan, &opts, state.path(), &mut std::io::sink(), &|e| {
        heard.lock().unwrap().push(e);
    })
    .unwrap();
    let events = heard.into_inner().unwrap();

    assert_eq!(events[0], Event::Queued(files.clone()));
    let reading: Vec<&PathBuf> = events
        .iter()
        .filter_map(|e| match e {
            Event::Reading(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(reading, files.iter().collect::<Vec<_>>());
    // Each file's last word on the LLM: its one chunk answered (E02's is E01's).
    for f in &files {
        let last = events.iter().rev().find_map(|e| match e {
            Event::Asking {
                path,
                answered,
                chunks,
            } if path == f => Some((*answered, *chunks)),
            _ => None,
        });
        assert_eq!(last, Some((1, 1)), "{}", f.display());
    }
    // Exactly the records the run file has after its header, in its order.
    let appended: Vec<Record> = events
        .iter()
        .filter_map(|e| match e {
            Event::Appended(records) => Some(records.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    let text = std::fs::read_to_string(&run_path).unwrap();
    let on_disk: Vec<Record> = text
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(appended, on_disk);
    // Each file finished, after its records, with its findings and its log line.
    let finished: Vec<(&PathBuf, &Option<String>, usize, &Vec<String>)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Finished {
                path,
                error,
                findings,
                lines,
            } => Some((path, error, *findings, lines)),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 2);
    for (path, error, findings, lines) in &finished {
        assert_eq!((*error, *findings), (&None, 2), "{}", path.display());
        assert!(lines[0].ends_with(": 4 lines, 2 findings"), "{lines:?}");
    }
    assert_eq!(summary.findings, 4);
}

//! The first Ctrl-C's notice reaches the injected error stream while the
//! interrupted work is still blocked, not when the command returns.
//!
//! The interrupt is a real SIGINT sent to this test process only, so this
//! binary holds exactly this one test: no other test's work can see it.
#![cfg(unix)]

mod progress_support;

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use broscribe_cli::run;
use progress_support::{GatedModels, Lines, deps, fake_models, write_conversation};
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(60);

#[test]
fn first_interrupt_notice_is_written_while_reprocessing_is_still_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    let library = root.join("lib");
    let wav = root.join("meeting.wav");
    write_conversation(&wav);
    let argv = |args: &[&str]| -> Vec<OsString> {
        [
            "broscribe",
            "--library",
            library.to_str().unwrap(),
            "--json",
        ]
        .iter()
        .chain(args)
        .map(OsString::from)
        .collect()
    };

    let mut out = Vec::new();
    let code = run(
        argv(&["import", wav.to_str().unwrap()]),
        &deps(&root, Box::new(fake_models())),
        &mut out,
        &mut Vec::new(),
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    let imported: Value = serde_json::from_slice(&out).unwrap();
    let id = imported["meeting_id"].as_str().unwrap().to_owned();
    let meeting_id = broscribe_core::MeetingId::parse(&id).unwrap();
    let before = broscribe_core::Library::open_existing(&library)
        .unwrap()
        .load_meeting(&meeting_id)
        .unwrap();

    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let (line_tx, line_rx) = mpsc::channel();
    let reprocess = argv(&["reprocess", &id]);
    let worker_root = root.clone();
    let worker = std::thread::spawn(move || {
        let deps = deps(
            &worker_root,
            Box::new(GatedModels {
                entered: entered_tx,
                release: std::sync::Mutex::new(Some(release_rx)),
            }),
        );
        let mut out = Vec::new();
        let code = run(reprocess, &deps, &mut out, &mut Lines::new(line_tx));
        (code, out)
    });
    // Recognition is blocked, and the interrupt watcher installed its
    // handlers before the reprocessing started.
    entered_rx
        .recv_timeout(WAIT)
        .expect("reprocessing reached speech recognition");

    let mut lines: Vec<String> = Vec::new();
    let mut wait_for = |prefix: &str| {
        while !lines.iter().any(|l| l.starts_with(prefix)) {
            match line_rx.recv_timeout(WAIT) {
                Ok(line) => lines.push(line),
                Err(_) => panic!("no {prefix:?} line while blocked; so far: {lines:#?}"),
            }
        }
    };
    wait_for(&format!("{id}: analyzing stored recording tracks"));

    let signalled = std::process::Command::new("kill")
        .args(["-INT", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success());
    wait_for("Stopping: the reprocessing saves nothing; press Ctrl-C again to quit now");

    release_tx.send(()).unwrap();
    let (code, out) = worker.join().unwrap();
    assert_eq!(code, 1);
    // Standard output carries only the JSON error; nothing was saved.
    let error: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(error["error"]["code"], "interrupted", "{error}");
    let after = broscribe_core::Library::open_existing(&library)
        .unwrap()
        .load_meeting(&meeting_id)
        .unwrap();
    assert_eq!(after, before);
}

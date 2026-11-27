//! Progress reaches the injected error stream while the command is still
//! working (`run` in process, fake models, synthetic audio and temporary
//! folders): a manifest import blocked inside speech recognition has already
//! shown every line before it, however many, and a closed error stream
//! neither blocks nor fails the import.

mod progress_support;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use broscribe_cli::run;
use progress_support::{GatedModels, Lines, deps, fake_models, write_conversation};
use serde_json::Value;

/// Entries whose audio is missing, before the one real recording.
const MISSING: usize = 100;
const TOTAL: usize = MISSING + 1;
const WAIT: Duration = Duration::from_secs(60);

/// A manifest of `MISSING` entries whose audio does not exist, then one
/// synthetic two-voice recording.
fn manifest(root: &Path) -> PathBuf {
    let input = root.join("in");
    std::fs::create_dir_all(&input).unwrap();
    let good = input.join("good.wav");
    write_conversation(&good);
    let mut meetings: Vec<Value> = (1..=MISSING)
        .map(|n| {
            serde_json::json!({
                "source_id": format!("missing-{n}"),
                "audio_path": input.join(format!("missing-{n}.wav")),
            })
        })
        .collect();
    meetings.push(serde_json::json!({"source_id": "good", "audio_path": good}));
    let path = root.join("manifest.json");
    let manifest = serde_json::json!({
        "format": "broscribe-import-manifest",
        "version": 1,
        "meetings": meetings,
    });
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    path
}

fn import_args(root: &Path) -> Vec<OsString> {
    vec![
        "broscribe".into(),
        "--library".into(),
        root.join("lib").into_os_string(),
        "--json".into(),
        "import".into(),
        "--manifest".into(),
        manifest(root).into_os_string(),
    ]
}

/// An error stream whose reader went away.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
}

#[test]
fn manifest_progress_is_written_while_recognition_is_still_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let (line_tx, line_rx) = mpsc::channel();
    let importer = std::thread::spawn(move || {
        let deps = deps(
            &root,
            Box::new(GatedModels {
                entered: entered_tx,
                release: std::sync::Mutex::new(Some(release_rx)),
            }),
        );
        let mut out = Vec::new();
        let code = run(
            import_args(&root),
            &deps,
            &mut out,
            &mut Lines::new(line_tx),
        );
        (code, out)
    });
    entered_rx
        .recv_timeout(WAIT)
        .expect("recognition of the last entry started");

    // Recognition of the last entry is blocked: every line before it,
    // beyond any queue bound, is already on the error stream, in order.
    let last = format!("[{TOTAL}/{TOTAL}] ");
    let mut lines: Vec<String> = Vec::new();
    while !lines.iter().any(|l| l.starts_with(&last)) {
        match line_rx.recv_timeout(WAIT) {
            Ok(line) => lines.push(line),
            Err(_) => panic!("no live progress; so far: {lines:#?}"),
        }
    }
    let mut expected = 1;
    for line in &lines {
        if line.starts_with(&format!("[{expected}/{TOTAL}] ")) {
            expected += 1;
        }
    }
    assert_eq!(expected, TOTAL + 1, "{lines:#?}");
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("  failed: ")).count(),
        MISSING,
        "{lines:#?}"
    );

    release_tx.send(()).unwrap();
    let (code, out) = importer.join().unwrap();
    assert_eq!(code, 0);
    // Standard output carries only the JSON result.
    let summary: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(summary["meetings"], TOTAL, "{summary}");
}

#[test]
fn a_closed_error_stream_neither_blocks_nor_fails_a_long_import() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    let library = root.join("lib");
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let deps = deps(&root, Box::new(fake_models()));
        let mut out = Vec::new();
        let code = run(import_args(&root), &deps, &mut out, &mut Closed);
        let _ = done_tx.send((code, out));
    });
    let (code, out) = done_rx
        .recv_timeout(WAIT)
        .expect("the import finished with its error stream closed");
    assert_eq!(code, 0);
    let summary: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(summary["meetings"], TOTAL, "{summary}");
    // The recording was imported and kept.
    let saved = broscribe_core::Library::open_existing(library)
        .unwrap()
        .list_meeting_dirs()
        .unwrap();
    assert_eq!(saved.len(), 1);
}

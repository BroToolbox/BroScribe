//! CLI integration: `run` in process with fake models, a synthetic WAV and
//! temporary library and data folders (no real data).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use broscribe_cli::{Deps, run};
use broscribe_core::config::{CONFIRM_MAX_CAPTURE_S, CONFIRM_MIN_SPEECH_S, Config};
use broscribe_core::lock::LibraryLock;
use broscribe_core::queue::Queue;
use broscribe_core::{Library, MeetingId};
use broscribe_models::DiarSegment;
use broscribe_models::fakes::{FakeModelProvider, sine};
use broscribe_platform::fakes::{
    FakeMicCapture, FakePaths, FakeProcessProbe, FakeProcessTreeControl, FakeSecretStore,
    FakeTrustStore,
};
use broscribe_platform::symphonia_decoder::SymphoniaDecoder;
use broscribe_platform::{DecoderChain, resolve_paths};
use serde_json::Value;

/// Writes 16-bit PCM mono WAV at 16 kHz.
fn write_wav(path: &Path, samples: &[f32]) {
    let rate: u32 = 16_000;
    let data_len = u32::try_from(samples.len() * 2).unwrap();
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for &x in samples {
        let v = (x.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

fn silence(seconds: f64) -> Vec<f32> {
    vec![0.0; (seconds * 16_000.0) as usize]
}

/// Two synthetic voices (200 Hz, then 440 Hz) separated by silence.
fn conversation() -> Vec<f32> {
    let mut a = silence(0.5);
    a.extend(sine(200.0, 4.0, 0.5));
    a.extend(silence(1.0));
    a.extend(sine(440.0, 4.0, 0.5));
    a.extend(silence(0.5));
    a
}

fn schedule() -> Vec<DiarSegment> {
    vec![
        DiarSegment {
            start_s: 0.5,
            end_s: 4.5,
            speaker: 0,
        },
        DiarSegment {
            start_s: 5.5,
            end_s: 9.5,
            speaker: 1,
        },
    ]
}

struct Fixture {
    tmp: tempfile::TempDir,
    deps: Deps,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("in")).unwrap();
        let deps = Deps {
            clock: broscribe_cli::system_clock(),
            paths: Box::new(FakePaths::new(tmp.path().join("root"))),
            updates: broscribe_cli::UpdateDeps::with_transport(
                std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                    std::path::Path::new(&tmp.path().join("root")).join("install"),
                    "synthetic-test-target",
                )),
                std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
            ),
            models: Box::new(FakeModelProvider {
                asr_words: vec!["pricing".into(), "roadmap".into(), "review".into()],
                schedule: schedule(),
                embed_dim: 16,
            }),
            decoder: DecoderChain::new(vec![Box::new(SymphoniaDecoder::new())]),
            process: Box::new(FakeProcessProbe::new(std::process::id())),
            mic: Box::new(fake_mic()),
            secrets: std::sync::Arc::new(FakeSecretStore::default()),
            browser: std::sync::Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(broscribe_platform::fakes::FakeAutostart::default()),
            agents: broscribe_cli::AgentEnv::isolated(tmp.path()),
            execution: broscribe_models::ExecutionContext::new(
                std::sync::Arc::new(FakeProcessTreeControl::default()),
                std::sync::Arc::new(FakeTrustStore::default()),
            ),
        };
        Self { tmp, deps }
    }

    fn lib(&self) -> PathBuf {
        self.tmp.path().join("lib")
    }

    fn wav(&self, name: &str) -> PathBuf {
        let p = self.tmp.path().join("in").join(name);
        write_wav(&p, &conversation());
        p
    }

    /// Runs `broscribe --library <lib> <args…>`.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let lib = self.lib();
        let mut argv: Vec<OsString> = vec!["broscribe".into(), "--library".into(), lib.into()];
        argv.extend(args.iter().map(OsString::from));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &self.deps, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// Runs with `--json` and parses stdout; panics unless the exit code is 0.
    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let (code, out, err) = self.run(&all);
        assert_eq!(code, 0, "{args:?} failed: {out}{err}");
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"))
    }

    fn data_dir(&self) -> PathBuf {
        resolve_paths(self.deps.paths.as_ref()).unwrap().data_dir
    }

    fn cache_dir(&self) -> PathBuf {
        resolve_paths(self.deps.paths.as_ref()).unwrap().cache_dir
    }

    /// Sets `setup.voice_session` as the app does.
    fn set_session(&self, id: Option<&str>) {
        let dir = self.data_dir();
        let mut c = Config::load(&dir).unwrap();
        c.setup.voice_session = id.map(str::to_owned);
        c.save(&dir).unwrap();
    }

    /// A 20 s "read passage" in one voice (200 Hz) between short silences.
    fn passage_wav(&self, name: &str, freq: f32) -> PathBuf {
        let p = self.tmp.path().join("in").join(name);
        let mut a = silence(0.5);
        a.extend(sine(freq, 21.0, 0.5));
        a.extend(silence(0.5));
        write_wav(&p, &a);
        p
    }

    /// A 9 s confirmation: over the minimum detected speech, under the
    /// reading's ten-second `short_speech` warning.
    fn phrase_wav(&self, name: &str, freq: f32) -> PathBuf {
        let p = self.tmp.path().join("in").join(name);
        write_wav(&p, &sine(freq, 9.0, 0.5));
        p
    }

    fn library(&self) -> Library {
        Library::open_existing(self.lib()).unwrap()
    }
}

#[test]
fn library_meeting_and_transcript_filters_share_the_same_source_and_app() {
    use broscribe_core::schema::{Meeting, ProcessingStatus, Segment, Source, now};
    let fixture = Fixture::new();
    let library = Library::open(fixture.lib()).unwrap();
    let mut expected = None;
    for (source, app) in [(Source::Recording, "zoom"), (Source::Import, "teams")] {
        let mut meeting = Meeting::new(MeetingId::random(), source, now());
        meeting.title = Some("Synthetic orbit".into());
        meeting.app = Some(app.into());
        meeting.processing.status = ProcessingStatus::Done;
        meeting.segments.push(Segment {
            speech_overlap: Vec::new(),
            id: 1,
            track: "mic".into(),
            speaker: None,
            start: 0.0,
            end: 1.0,
            text: "Synthetic orbit".into(),
            words: vec![],
        });
        meeting.duration_s = 1.0;
        broscribe_core::ops::with_index(&library, |tx, index| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)
        })
        .unwrap();
        if source == Source::Recording {
            expected = Some(meeting.id);
        }
    }
    let expected = expected.unwrap();
    let meetings = fixture.json(&["meetings", "--app", "ZOOM", "--source", "recorded"]);
    let hits = meetings["meetings"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["meeting_id"], expected.as_str());
    assert_eq!(hits[0]["processing_status"], "done");
    assert_eq!(hits[0]["participant_count"], 0);
    let transcript = fixture.json(&[
        "search", "orbit", "--mode", "keyword", "--app", "zoom", "--source", "recorded",
    ]);
    assert_eq!(transcript["results"].as_array().unwrap().len(), 1);
    assert_eq!(transcript["results"][0]["meeting_id"], expected.as_str());
    let mismatch = fixture.json(&["meetings", "--app", "zoom", "--source", "imported"]);
    assert!(mismatch["meetings"].as_array().unwrap().is_empty());
}

#[test]
fn maintenance_refuses_active_clients_and_unreadable_admission_without_changing_mode() {
    use broscribe_core::maintenance::{Mode, ModeLease};
    let f = Fixture::new();
    let lease = ModeLease::acquire(&f.data_dir(), Duration::ZERO).unwrap();
    let (code, out, _) = f.run(&["maintenance", "enable", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["error"]["code"],
        "maintenance_busy"
    );
    assert_eq!(lease.mode(), Mode::Normal);
    drop(lease);
    let admission = broscribe_ipc::install_admission::Persisted::path(&f.data_dir());
    broscribe_platform_native::ipc::ensure_private_dir(admission.parent().unwrap()).unwrap();
    std::fs::write(&admission, b"damaged").unwrap();
    let (code, _, _) = f.run(&["maintenance", "enable", "--json"]);
    assert_eq!(code, 1);
    assert!(!f.data_dir().join("maintenance.request").exists());
}

#[test]
fn maintenance_denies_index_provider_and_write_commands_before_config_or_library_access() {
    let f = Fixture::new();
    f.json(&["maintenance", "enable"]);
    std::fs::write(f.data_dir().join("config.toml"), b"invalid = [").unwrap();
    for args in [
        vec!["meetings"],
        vec!["search", "synthetic"],
        vec!["people"],
        vec!["models", "list"],
        vec!["providers", "list"],
        vec!["queue", "work"],
        vec!["export", "m-synthetic", "--out", "private.txt"],
        vec!["export", "m-synthetic", "--spec-stdin"],
        vec!["update", "check"],
    ] {
        let mut command = vec!["--json"];
        command.extend(args);
        let (code, out, _) = f.run(&command);
        assert_eq!(code, 1, "{command:?}: {out}");
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["error"]["code"],
            "maintenance",
            "{command:?}"
        );
    }
    assert!(!f.lib().exists());
    assert!(!f.cache_dir().exists());
    let status = f.json(&["maintenance", "status"]);
    assert_eq!(status["configured_mode"], "maintenance");
    assert_eq!(status["inventory"], "unassessed");
    assert_eq!(status["agent"]["observation"], "absent");
    f.json(&["maintenance", "disable"]);
    assert!(!f.data_dir().join("maintenance.request").exists());
}

#[test]
fn offline_maintenance_distinguishes_ended_holds_from_durable_update_work() {
    let f = Fixture::new();
    let path = broscribe_ipc::install_admission::Persisted::path(&f.data_dir());
    broscribe_platform_native::ipc::ensure_private_dir(path.parent().unwrap()).unwrap();
    for held in [
        None,
        Some(broscribe_ipc::InstallHold {
            fence: 9,
            durable: false,
        }),
    ] {
        let record = broscribe_ipc::install_admission::Persisted {
            last_fence: 9,
            held,
        };
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        f.json(&["maintenance", "enable"]);
        f.json(&["maintenance", "disable"]);
    }
    let record = broscribe_ipc::install_admission::Persisted {
        last_fence: 9,
        held: Some(broscribe_ipc::InstallHold {
            fence: 9,
            durable: true,
        }),
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let (code, value, _) = f.run(&["maintenance", "enable", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&value).unwrap()["error"]["code"],
        "maintenance_busy"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!f.data_dir().join("maintenance.request").exists());
    std::fs::remove_file(&path).unwrap();
    let handoff = broscribe_update_runner::Handoff {
        schema: broscribe_update_runner::Handoff::SCHEMA,
        data_dir: f.data_dir(),
        executable: f.tmp.path().join("synthetic-cli"),
        installation_root: f.tmp.path().join("synthetic-installation"),
        protected_paths: Vec::new(),
        profile_dirs: Vec::new(),
        config_base: PathBuf::new(),
        agent_library: None,
        contained_paths: Vec::new(),
        wait_pids: Vec::new(),
        fence: 9,
        action: broscribe_update_runner::HandoffAction::Recover,
    };
    handoff.save().unwrap();
    let (code, value, _) = f.run(&["maintenance", "enable", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&value).unwrap()["error"]["code"],
        "maintenance_busy"
    );
    assert_eq!(
        broscribe_update_runner::Handoff::load(&f.data_dir()).unwrap(),
        Some(handoff)
    );
    assert!(!f.data_dir().join("maintenance.request").exists());
}

fn speaker_names(show: &Value) -> Vec<(String, String)> {
    show["speakers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().to_owned(),
                s["name"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn import_search_name_rebuild_forget_tag() {
    let f = Fixture::new();
    let wav = f.wav("weekly-sync.wav");

    let (code, output, notices) = f.run(&[
        "--json",
        "import",
        wav.to_str().unwrap(),
        "--title",
        "Weekly sync",
        "--lang",
        "en",
    ]);
    assert_eq!(code, 0, "{output}{notices}");
    let imported: Value = serde_json::from_str(&output).unwrap();
    // Pipeline callbacks belong to this invocation's injected stream,
    // never process-global stderr or the machine-readable result. The
    // notice may name the canonical path (Windows runners' TEMP is an 8.3
    // short name such as RUNNER~1, shown as \\?\C:\Users\runneradmin).
    let canonical = std::fs::canonicalize(&wav).unwrap();
    assert!(
        notices.contains(wav.to_str().unwrap()) || notices.contains(canonical.to_str().unwrap()),
        "{notices}"
    );
    assert!(!output.contains("decoding"), "{output}");
    let id = imported["meeting_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("m-"));
    assert!(
        imported["notes"][0].as_str().unwrap().contains("no effect"),
        "Parakeet ignores --lang: {imported}"
    );

    let found = f.json(&["search", "pricing"]);
    let results = found["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{found}");
    assert_eq!(results[0]["meeting_id"], id.as_str());
    assert!(
        results[0]["timestamp"]
            .as_str()
            .unwrap()
            .starts_with("00:00:")
    );

    let (code, text, _) = f.run(&["transcript", &id]);
    assert_eq!(code, 0);
    assert!(text.contains("pricing"), "{text}");
    assert!(text.lines().all(|l| l.starts_with("[00:00:")), "{text}");

    let show = f.json(&["show", &id]);
    let speakers = speaker_names(&show);
    assert!(speakers.len() >= 2, "{show}");
    let first = speakers[0].0.clone();

    let named = f.json(&["speakers", "name", &id, &first, "Ana Example"]);
    assert_eq!(named["created"], true);
    assert_eq!(named["person_name"], "Ana Example");
    let person_id = named["person_id"].as_str().unwrap().to_owned();

    let transcript = f.json(&["transcript", &id, "--speaker", "Ana Example"]);
    let segs = transcript["segments"].as_array().unwrap();
    assert!(!segs.is_empty());
    assert!(segs.iter().all(|s| s["speaker_name"] == "Ana Example"));
    assert!(segs.iter().all(|s| s["speaker"] == first.as_str()));

    // Rebuilding the index from the files gives the same answers.
    let before_search = f.json(&["search", "pricing"]);
    let before_show = speaker_names(&f.json(&["show", &id]));
    let before_people = f.json(&["people"]);
    std::fs::remove_dir_all(f.lib().join(".index")).unwrap();
    let rebuilt = f.json(&["index", "rebuild"]);
    assert_eq!(rebuilt["meetings"], 1);
    assert_eq!(f.json(&["search", "pricing"]), before_search);
    assert_eq!(speaker_names(&f.json(&["show", &id])), before_show);
    assert_eq!(f.json(&["people"]), before_people);
    assert!(before_show.iter().any(|(_, n)| n == "Ana Example"));

    let person = f.json(&["people", "show", "Ana Example"]);
    assert_eq!(person["id"], person_id.as_str());
    assert!(!person["voiceprints"].as_array().unwrap().is_empty());

    let forgot = f.json(&["people", "forget-voice", "Ana Example", "--yes"]);
    assert_eq!(forgot["voiceprints_deleted"], true);
    let person = f.json(&["people", "show", &person_id]);
    assert!(person["voiceprints"].as_array().unwrap().is_empty());
    // The name stays.
    assert!(
        speaker_names(&f.json(&["show", &id]))
            .iter()
            .any(|(_, n)| n == "Ana Example")
    );

    let tagged = f.json(&["tag", &id, "+decision", "+pricing", "-pricing"]);
    assert_eq!(tagged["tags"], serde_json::json!(["decision"]));
    let by_tag = f.json(&["search", "pricing", "--tag", "decision"]);
    assert!(!by_tag["results"].as_array().unwrap().is_empty());
    let other = f.json(&["search", "pricing", "--tag", "other"]);
    assert!(other["results"].as_array().unwrap().is_empty());
    let since = f.json(&[
        "search",
        "pricing",
        "--since",
        "30d",
        "--until",
        "2999-01-01",
    ]);
    assert_eq!(since["results"], by_tag["results"]);

    // Errors carry stable codes.
    let (code, out, _) = f.run(&["--json", "show", "m-000000"]);
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["error"]["code"], "not_found");
    // `--handling move` takes the file into the library.
    let moved = f.tmp.path().join("in").join("moved.wav");
    let mut audio = conversation();
    audio.extend(silence(1.0));
    write_wav(&moved, &audio);
    let v = f.json(&["import", moved.to_str().unwrap(), "--handling", "move"]);
    assert_eq!(v["already_exists"], false, "{v}");
    assert!(!moved.exists());
    let m = Library::open_existing(f.lib())
        .unwrap()
        .load_meeting(&MeetingId::parse(v["meeting_id"].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(
        m.recording.unwrap().handling,
        broscribe_core::schema::Handling::Move
    );
}

#[test]
fn library_busy_when_the_lock_is_held() {
    let f = Fixture::new();
    let wav = f.wav("a.wav");
    let id = f.json(&["import", wav.to_str().unwrap()])["meeting_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let guard = LibraryLock::acquire(&f.lib(), Duration::ZERO).unwrap();
    let (code, out, _) = f.run(&["--json", "tag", &id, "+x"]);
    drop(guard);
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["error"]["code"], "library_busy", "{out}");
    // Reads never take the lock.
    let _ = f.json(&["show", &id]);
}

#[test]
fn manifest_reports_a_bad_entry_and_imports_the_rest() {
    let f = Fixture::new();
    let good = f.wav("good.wav");
    let missing = f.tmp.path().join("in").join("missing.wav");
    let manifest = serde_json::json!({
        "format": "broscribe-import-manifest",
        "version": 1,
        "people": [{"name": "Ana Example", "email": "ana@example.com"}],
        "meetings": [
            {
                "source_id": "demo-1",
                "audio_path": good,
                "title": "Good one",
                "started_at": "2026-11-10T14:00:00Z",
                "participants": [{"name": "Ana Example", "email": "ana@example.com", "is_self": false}],
                "speaker_hints": [
                    {"name": "Ana Example", "email": "ana@example.com", "is_self": false, "ranges": [[0.5, 4.5]]}
                ]
            },
            {"source_id": "demo-2", "audio_path": missing, "title": "Missing file"},
            {"source_id": "demo-3", "title": "No audio path"}
        ]
    });
    let path = f.tmp.path().join("manifest.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    let summary = f.json(&["import", "--manifest", path.to_str().unwrap()]);
    let problems =
        summary["issues"].as_array().unwrap().len() + summary["failed"].as_array().unwrap().len();
    // The missing file and the entry without `audio_path`, each reported once
    // (as a manifest issue or a per-meeting failure).
    assert_eq!(problems, 2, "{summary}");
    assert!(summary.to_string().contains("demo-2"), "{summary}");

    let found = f.json(&["search", "pricing"]);
    let results = found["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{found}");
    assert_eq!(results[0]["meeting_title"], "Good one");
    let people = f.json(&["people"]);
    assert!(
        people["people"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Ana Example"),
        "{people}"
    );
}

#[test]
fn settings_remove_word_matches_the_saved_term() {
    let f = Fixture::new();
    let file = f.tmp.path().join("words.txt");
    std::fs::write(&file, "Sabiá release\nAcorn Labs\n").unwrap();
    f.json(&[
        "setup",
        "--non-interactive",
        "--words-file",
        file.to_str().unwrap(),
    ]);
    assert_eq!(
        f.json(&["settings", "remove-word", "sabiá release"])["removed"],
        false
    );
    assert_eq!(
        f.json(&["settings", "remove-word", "Sabiá release"])["removed"],
        true
    );
    assert_eq!(
        f.json(&["settings", "remove-word", "Sabiá release"])["removed"],
        false
    );
    assert_eq!(
        f.json(&["settings", "word-list"])["words"],
        serde_json::json!(["Acorn Labs"])
    );
}

#[test]
fn setup_flags_create_me_and_doctor_reports() {
    let f = Fixture::new();
    let wav = f.wav("me.wav");
    let words = f.tmp.path().join("words.txt");
    std::fs::write(&words, "Kubernetes\n# comment\nExampleCorp\n").unwrap();
    let contacts = f.tmp.path().join("people.csv");
    std::fs::write(&contacts, "Name,Email\nBo Test,bo@example.com\n").unwrap();

    let out = f.json(&[
        "setup",
        "--lang",
        "en",
        "--name",
        "Robin Example",
        "--email",
        "robin@example.com",
        "--words-file",
        words.to_str().unwrap(),
        "--people-file",
        contacts.to_str().unwrap(),
        "--enroll-me",
        wav.to_str().unwrap(),
        "--range",
        "0.5-4.5",
    ]);
    assert_eq!(out["words_added"], 2);
    assert_eq!(out["people"]["created"], 1);
    assert!(
        out["enrollment"]["voiceprints"].as_u64().unwrap() >= 1,
        "{out}"
    );
    let me_id = out["me"]["person_id"].as_str().unwrap().to_owned();
    assert_eq!(out["enrollment"]["person_id"], me_id.as_str());

    let doctor = f.json(&["doctor"]);
    assert_eq!(doctor["library_source"], "--library");
    let me = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "me")
        .unwrap();
    assert_eq!(me["ok"], true, "{doctor}");

    // The enrolled voice names the matching speaker of a new import.
    let id = f.json(&["import", f.wav("later.wav").to_str().unwrap()])["meeting_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let show = f.json(&["show", &id]);
    assert!(
        show["speakers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["person_id"] == me_id.as_str()
                || s["suggestion"]["person_id"] == me_id.as_str()),
        "{show}"
    );
}

#[test]
fn setup_profile_switches_the_asr_and_keeps_the_embedder() {
    let f = Fixture::new();
    let active = |v: &Value| -> Vec<String> {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["active"] == true)
            .filter_map(|p| p["id"].as_str().map(str::to_owned))
            .collect()
    };
    let v = f.json(&["models", "list"]);
    assert_eq!(active(&v), ["default"], "{v}");
    let multi = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "multilingual")
        .cloned()
        .unwrap();
    assert_eq!(multi["asr"], "whisper-large-v3-turbo");
    // PL02: the active profile's models, live preview included, each once.
    assert_eq!(
        v["profile_models"],
        serde_json::json!([
            "silero-vad",
            "parakeet-tdt-0.6b-v3",
            "pyannote-segmentation-3.0",
            "nemo-titanet-large"
        ]),
        "{v}"
    );
    assert!(v.get("profile_models_error").is_none(), "{v}");
    let before = Config::load(&f.data_dir()).unwrap().profile;

    let v = f.json(&["setup", "--non-interactive", "--profile", "multilingual"]);
    assert_eq!(v["profile"], "multilingual", "{v}");
    assert!(v["reembed"].is_null(), "{v}");
    let after = Config::load(&f.data_dir()).unwrap().profile;
    assert_eq!(after.asr, "whisper-large-v3-turbo");
    assert_eq!(after.speaker_embedding, before.speaker_embedding);
    assert_eq!(after.text_embedding, before.text_embedding);
    let v = f.json(&["models", "list"]);
    assert_eq!(active(&v), ["multilingual"]);
    // Whisper has no live role: the live model it declares is required
    // (and active for `live`), never another installed one.
    assert_eq!(
        v["profile_models"],
        serde_json::json!([
            "silero-vad",
            "whisper-large-v3-turbo",
            "parakeet-tdt-0.6b-v3",
            "pyannote-segmentation-3.0",
            "nemo-titanet-large"
        ]),
        "{v}"
    );
    let parakeet = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "parakeet-tdt-0.6b-v3")
        .unwrap();
    assert_eq!(parakeet["active_for"], serde_json::json!(["live"]), "{v}");

    let (code, out, err) = f.run(&[
        "--json",
        "setup",
        "--non-interactive",
        "--profile",
        "no-such-profile",
    ]);
    assert_ne!(code, 0, "{out}{err}");
}

/// A 200 Hz voice for 25 s: the confirmation capture's full length, so a
/// fixed-length capture never waits for chunks the fake will not send.
fn fake_mic() -> FakeMicCapture {
    let mut mic = FakeMicCapture::new(16_000);
    mic.frequency_hz = 200.0;
    mic.chunk_len = 1_600;
    mic.chunks = 250;
    mic
}

fn import_one(f: &Fixture, name: &str) -> String {
    let wav = f.wav(name);
    f.json(&["import", wav.to_str().unwrap()])["meeting_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn queue_work_runs_jobs_with_json_progress_and_status() {
    let f = Fixture::new();
    let id = import_one(&f, "q.wav");
    let named = f.json(&["speakers", "name", &id, "S1", "Ana Example"]);
    let person = named["person_id"].as_str().unwrap().to_owned();
    // Naming requested the speaker recheck; no agent runs in tests, so it
    // waits for a worker.
    assert_eq!(named["agent_notified"], false, "{named}");
    assert_eq!(named["learning"]["source_meeting"], id.as_str(), "{named}");
    let recheck = named["learning"]["job_id"].as_str().unwrap().to_owned();
    assert!(named.get("retro_candidates").is_none(), "{named}");
    let lib = f.library();
    let queue = Queue::open(lib.root()).unwrap();
    let model = Config::load(&f.data_dir())
        .unwrap()
        .profile
        .speaker_embedding;
    lib.write(|tx| {
        queue.enqueue(
            tx,
            "confirm-samples",
            &format!("{id}/S1/{person}/{model}"),
            serde_json::json!({
                "meeting_id": id, "speaker_id": "S1", "person_id": person, "model": model,
            }),
        )?;
        queue.enqueue(
            tx,
            "playback-cache",
            &format!("{id}/sha"),
            serde_json::json!({ "meeting_id": id }),
        )
    })
    .unwrap();

    let status = f.json(&["queue", "status"]);
    assert_eq!(status["jobs"].as_array().unwrap().len(), 3, "{status}");
    assert_eq!(status["counts"]["queued"], 3);
    let job = status["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == recheck.as_str())
        .unwrap();
    assert_eq!(job["kind"], "reidentify");
    assert_eq!(job["label"], "Checking other speakers");
    assert!(
        status["learning"]["requested"].as_u64() > status["learning"]["processed"].as_u64(),
        "{status}"
    );

    let (code, out, err) = f.run(&["queue", "work", "--json-progress"]);
    assert_eq!(code, 0, "{out}{err}");
    let events: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert_eq!(events.last().unwrap()["event"], "idle", "{out}");
    let done = events.iter().filter(|e| e["event"] == "job_done").count();
    assert_eq!(done, 3, "{out}");
    assert!(events.iter().all(|e| e["event"] != "job_failed"), "{out}");

    let sha = lib
        .load_meeting(&MeetingId::parse(&id).unwrap())
        .unwrap()
        .recording
        .unwrap()
        .sha256;
    assert!(
        f.cache_dir()
            .join("playback")
            .join(format!("{sha}.wav"))
            .is_file()
    );
    // Finished jobs are cleared, except the speaker recheck, which keeps
    // its committed counts for later readers.
    let finished_only_recheck = |status: &Value| {
        status["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["id"] == recheck.as_str() && j["state"] == "done")
    };
    let status = f.json(&["queue", "status"]);
    assert!(finished_only_recheck(&status), "{status}");
    assert_eq!(
        status["learning"]["requested"], status["learning"]["processed"],
        "{status}"
    );
    assert!(status["learning"]["report"].is_object(), "{status}");

    // A closed progress pipe does not stop the worker.
    lib.write(|tx| {
        queue.enqueue(
            tx,
            "playback-cache",
            &format!("{id}/again"),
            serde_json::json!({ "meeting_id": id }),
        )
    })
    .unwrap();
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }
    let argv: Vec<OsString> = ["broscribe", "--library"]
        .into_iter()
        .map(OsString::from)
        .chain([f.lib().into_os_string()])
        .chain(["queue", "work", "--json-progress"].map(OsString::from))
        .collect();
    let mut err = Vec::new();
    let code = run(argv, &f.deps, &mut Closed, &mut err);
    assert_eq!(code, 0);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("progress output closed")
    );
    assert!(finished_only_recheck(&f.json(&["queue", "status"])));
}

/// Naming says the name is saved and other speakers are being rechecked
/// (never that they were named); `identity status` reports the queued
/// recheck, this meeting first, then the committed counts once a worker
/// ran it: counts and meeting IDs only, no names.
#[test]
fn naming_reports_the_recheck_and_identity_status_follows_it() {
    let f = Fixture::new();
    let id = import_one(&f, "recheck.wav");
    let none = f.json(&["identity", "status"]);
    assert_eq!(none["learning"], Value::Null, "{none}");
    let (code, human, err) = f.run(&["speakers", "name", &id, "S1", "Ana Example"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        human.contains("Checking other speakers: this meeting first"),
        "{human}"
    );
    assert!(!human.contains("Similar speakers"), "{human}");
    assert!(err.contains("no agent is running"), "{err}");

    let queued = f.json(&["identity", "status"]);
    assert_eq!(
        queued["learning"]["source_meetings"],
        serde_json::json!([id]),
        "{queued}"
    );
    let (_, human, _) = f.run(&["identity", "status"]);
    assert!(human.contains("Checking other speakers: queued"), "{human}");
    assert!(human.contains(&id), "{human}");

    let (code, _, err) = f.run(&["queue", "work"]);
    assert_eq!(code, 0, "{err}");
    let done = f.json(&["identity", "status"]);
    assert_eq!(
        done["learning"]["requested"], done["learning"]["processed"],
        "{done}"
    );
    assert!(done["learning"]["report"].is_object(), "{done}");
    let (_, human, _) = f.run(&["identity", "status"]);
    assert!(human.contains("Other speakers are up to date."), "{human}");
    assert!(human.contains("Last results:"), "{human}");
    for text in [done.to_string(), human] {
        assert!(!text.contains("Ana Example"), "{text}");
    }
}

#[test]
fn audio_devices_lists_the_microphone() {
    let f = Fixture::new();
    let v = f.json(&["audio", "devices"]);
    assert_eq!(v["devices"][0]["id"], "fake-mic");
    assert_eq!(v["devices"][0]["is_default"], true);
}

#[test]
fn audio_pin_saves_a_connected_microphone_and_clears_it() {
    let f = Fixture::new();
    let pinned = || {
        broscribe_core::config::Config::load(&f.data_dir())
            .unwrap()
            .audio
            .pinned_mic
    };
    assert_eq!(
        f.json(&["audio", "pin", "fake-mic"])["pinned_mic"],
        "fake-mic"
    );
    assert_eq!(pinned().as_deref(), Some("fake-mic"));
    let (code, _, _) = f.run(&["audio", "pin", "no-such-mic"]);
    assert_ne!(code, 0);
    assert_eq!(pinned().as_deref(), Some("fake-mic"));
    assert!(f.json(&["audio", "pin", "--clear"])["pinned_mic"].is_null());
    assert_eq!(pinned(), None);
}

#[test]
fn setup_voice_from_file_commits_only_for_the_current_session() {
    let f = Fixture::new();
    f.set_session(Some("s-2"));

    // A superseded session commits nothing and preserves its input.
    let old = f.passage_wav("old.wav", 200.0);
    let v = f.json(&[
        "setup",
        "voice",
        "--from-file",
        old.to_str().unwrap(),
        "--session",
        "s-1",
        "--keep-as-enrollment",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(v["committed"], false, "{v}");
    assert!(old.exists());
    let lib = Library::open(f.lib()).unwrap();
    assert!(lib.self_person().unwrap().is_none());

    // The current session commits voiceprints and keeps the passage.
    let wav = f.passage_wav("p.wav", 200.0);
    let phrase = f.phrase_wav("c.wav", 200.0);
    let v = f.json(&[
        "setup",
        "voice",
        "--from-file",
        wav.to_str().unwrap(),
        "--confirm-file",
        phrase.to_str().unwrap(),
        "--session",
        "s-2",
        "--keep-as-enrollment",
        "--device",
        "Desk mic",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(v["committed"], true, "{v}");
    assert!(v["voiceprints"].as_u64().unwrap() >= 4, "{v}");
    assert_eq!(v["confirmation"]["passed"], true);
    assert!(wav.exists() && phrase.exists());
    let kept = PathBuf::from(v["enrollment_file"].as_str().unwrap());
    assert!(kept.is_file());
    let me = lib.self_person().unwrap().unwrap();
    let vps = lib.load_voiceprints(&me.id).unwrap();
    assert!(
        vps.embeddings
            .iter()
            .all(|e| e.device.as_deref() == Some("Desk mic"))
    );
    assert!(
        vps.embeddings
            .iter()
            .all(|e| e.source.sha256.as_deref() == kept.file_stem().and_then(|s| s.to_str()))
    );
    let config = Config::load(&f.data_dir()).unwrap();
    assert!(config.setup.completed_steps.iter().any(|s| s == "voice"));

    // A mismatched phrase changes nothing, and its quality diagnostics survive
    // the CLI error path for both ways of choosing the comparison threshold.
    for suggest in [None, Some(0.9)] {
        let mut config = Config::load(&f.data_dir()).unwrap();
        config.identity.insert(
            config.profile.speaker_embedding.clone(),
            broscribe_core::config::Thresholds {
                suggest,
                ..Default::default()
            },
        );
        config.save(&f.data_dir()).unwrap();
        let wav = f.passage_wav("p2.wav", 200.0);
        let other = f.phrase_wav("c2.wav", 440.0);
        write_wav(&other, &sine(440.0, 9.0, 1.0));
        let (code, out, _) = f.run(&[
            "--json",
            "setup",
            "voice",
            "--from-file",
            wav.to_str().unwrap(),
            "--confirm-file",
            other.to_str().unwrap(),
            "--session",
            "s-2",
        ]);
        assert_eq!(code, 1, "{out}");
        let e: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(e["error"]["code"], "invalid_input");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(message.contains("voice similarity"), "{message}");
        assert!(message.contains("threshold"), "{message}");
        assert!(
            message.contains(if suggest.is_some() {
                "calibrated"
            } else {
                "relative"
            }),
            "{message}"
        );
        assert!(
            message.contains("Audio warnings (confirmation reading): clipping"),
            "{message}"
        );
        // A short confirmation is expected, not a failed ten-second reading.
        assert!(!message.contains("short_speech"), "{message}");
        assert_eq!(lib.load_voiceprints(&me.id).unwrap(), vps);
        assert!(wav.exists() && other.exists());
    }

    // The kept passage can be deleted; the voiceprints stay.
    let v = f.json(&["setup", "voice", "--delete-enrollment"]);
    assert_eq!(v["deleted_files"], 1);
    assert!(!kept.exists());
    assert_eq!(lib.load_voiceprints(&me.id).unwrap(), vps);
}

#[test]
fn setup_voice_mic_and_from_meeting_and_passage() {
    let f = Fixture::new();
    let before = Config::load(&f.data_dir()).unwrap().default_language;
    let p = f.json(&["setup", "voice", "--show-passage", "--lang", "pt"]);
    assert_eq!(p["lang"], "pt-BR");
    assert!(!p["confirmation_phrase"].as_str().unwrap().is_empty());
    let (code, text, _) = f.run(&["setup", "voice", "--show-passage"]);
    assert_eq!(code, 0);
    assert!(text.contains("Confirmation reading:"));
    assert_eq!(
        Config::load(&f.data_dir()).unwrap().default_language,
        before
    );

    let v = f.json(&[
        "setup",
        "voice",
        "--mic",
        "--seconds",
        "22",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(v["committed"], true, "{v}");
    assert!(v["voiceprints"].as_u64().unwrap() >= 4, "{v}");
    assert!(v["enrollment_file"].is_null());
    let me = v["person_id"].as_str().unwrap().to_owned();

    let id = import_one(&f, "m.wav");
    let v = f.json(&["setup", "voice", "--from", &id, "--speaker", "S2"]);
    assert_eq!(v["person_id"], me.as_str());
    let show = f.json(&["show", &id]);
    assert!(
        show["speakers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == "S2" && s["person_id"] == me.as_str()),
        "{show}"
    );
}

/// `seconds` of one synthetic voice (200 Hz) between half-second silences.
fn spoken(seconds: f64, amplitude: f32) -> Vec<f32> {
    let mut a = silence(0.5);
    a.extend(sine(200.0, seconds, amplitude));
    a.extend(silence(0.5));
    a
}

/// Loads only the VAD and counts it; any other model load fails the test.
struct VadOnlyProvider(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl broscribe_models::ModelProvider for VadOnlyProvider {
    fn load(
        &self,
        _: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        panic!("the readiness check must not load the full model set")
    }
    fn load_vad(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        broscribe_models::ModelProvider::load_vad(&FakeModelProvider::default(), model)
    }
    fn load_embedder(
        &self,
        _: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        panic!("the readiness check must not load a speaker-embedding model")
    }
}

#[test]
fn setup_voice_check_recording_counts_parts_and_writes_nothing() {
    let mut f = Fixture::new();
    let loads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    f.deps.models = Box::new(VadOnlyProvider(loads.clone()));
    f.set_session(Some("configured-session"));
    let config_file = f.data_dir().join(broscribe_core::config::CONFIG_FILE);
    let config_before = std::fs::read(&config_file).unwrap();
    let input = |name: &str, samples: &[f32]| {
        let p = f.tmp.path().join("in").join(name);
        write_wav(&p, samples);
        p
    };
    let check = |file: &Path, extra: &[&str]| {
        let mut args = vec![
            "setup",
            "voice",
            "--check-recording",
            "--from-file",
            file.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let v = f.json(&args);
        assert!(file.exists(), "the input is preserved: {v}");
        v
    };

    // No library, no Me and no name: the check needs none of them, and a
    // session other than the configured one is only a cache folder.
    let session = ["--session", "other-session"];
    let v = check(&input("short.wav", &spoken(18.0, 0.5)), &session);
    assert_eq!(v["ready"], false, "{v}");
    assert_eq!(v["usable_parts"], 3, "{v}");
    assert_eq!(v["required_parts"], 4, "{v}");
    for key in [
        "duration_s",
        "speech_s",
        "speech_rms_dbfs",
        "noise_rms_dbfs",
        "noise_reference_s",
        "snr_db",
        "clipped_fraction",
        "warnings",
    ] {
        assert!(v["checks"].get(key).is_some(), "{key}: {v}");
    }
    assert!((v["checks"]["duration_s"].as_f64().unwrap() - 19.0).abs() < 0.01);

    let v = check(&input("enough.wav", &spoken(25.0, 0.5)), &session);
    assert_eq!(v["ready"], true, "{v}");
    assert_eq!(v["usable_parts"], 5, "{v}");
    assert_eq!(v["checks"]["warnings"], serde_json::json!([]), "{v}");

    // --range limits the analysis like enrollment's.
    let v = check(
        &input("ranged.wav", &spoken(25.0, 0.5)),
        &["--range", "0-12", "--session", "configured-session"],
    );
    assert_eq!(v["ready"], false, "{v}");
    assert_eq!(v["usable_parts"], 2, "{v}");

    // Silence and a voice too quiet to count as speech.
    for (name, samples) in [
        ("silent.wav", silence(26.0)),
        ("quiet.wav", spoken(25.0, 0.01)),
    ] {
        let v = check(&input(name, &samples), &[]);
        assert_eq!(v["ready"], false, "{name}: {v}");
        assert_eq!(v["usable_parts"], 0, "{name}: {v}");
        let warnings = v["checks"]["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|w| w == "low_level"), "{name}: {v}");
    }

    assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 5);
    assert_eq!(std::fs::read(&config_file).unwrap(), config_before);
    assert!(!f.lib().exists(), "no library was created");
    let cache = f.cache_dir();
    for session in ["other-session", "configured-session"] {
        let dir = cache.join("setup-capture").join(session);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "{session}");
    }
    assert!(
        !cache
            .join("setup-capture-cli")
            .join(std::process::id().to_string())
            .exists()
    );

    // Commit flags and other sources are refused before the file is touched.
    let kept = input("kept.wav", &spoken(25.0, 0.5));
    let kept = kept.to_str().unwrap();
    let refused: [&[&str]; 10] = [
        &["--name", "Robin Example"],
        &["--email", "robin@example.invalid"],
        &["--keep-as-enrollment"],
        &["--confirm-file", kept],
        &["--report-unconfirmed"],
        &["--confirm-file", kept, "--report-unconfirmed"],
        &["--device", "fake-mic"],
        &["--mic"],
        &["--show-passage"],
        &["--delete-enrollment"],
    ];
    for extra in refused {
        let mut args = vec!["setup", "voice", "--check-recording", "--from-file", kept];
        args.extend_from_slice(extra);
        let (code, out, err) = f.run(&args);
        assert_eq!(code, 2, "{extra:?}: {out}{err}");
    }
    let (code, _, _) = f.run(&["setup", "voice", "--check-recording", "--mic"]);
    assert_eq!(code, 2);
    assert!(Path::new(kept).is_file());
    assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 5);
}

/// The full fake model set with an embedder that never returns a vector, as
/// a speaker model does for audio it cannot embed.
struct UnembeddableProvider;
impl broscribe_models::ModelProvider for UnembeddableProvider {
    fn load(
        &self,
        profile: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        let mut set =
            broscribe_models::ModelProvider::load(&FakeModelProvider::default(), profile)?;
        set.embedder = Box::new(UnembeddableEmbedder(set.embedder.model_id().to_owned()));
        Ok(set)
    }
    fn load_vad(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        broscribe_models::ModelProvider::load_vad(&FakeModelProvider::default(), model)
    }
    fn load_embedder(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        Ok(Box::new(UnembeddableEmbedder(model.to_owned())))
    }
}
struct UnembeddableEmbedder(String);
impl broscribe_models::SpeakerEmbedder for UnembeddableEmbedder {
    fn model_id(&self) -> &str {
        &self.0
    }
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, _: &[f32]) -> broscribe_core::Result<Option<Vec<f32>>> {
        Ok(None)
    }
}

/// Sets the calibrated `suggest` threshold of the configured speaker model.
fn set_suggest(f: &Fixture, suggest: Option<f32>) {
    let mut config = Config::load(&f.data_dir()).unwrap();
    config.identity.insert(
        config.profile.speaker_embedding.clone(),
        broscribe_core::config::Thresholds {
            suggest,
            ..Default::default()
        },
    );
    config.save(&f.data_dir()).unwrap();
}

#[test]
fn setup_voice_report_unconfirmed_saves_nothing_until_an_explicit_save() {
    let f = Fixture::new();
    f.set_session(Some("s-1"));
    let config_file = f.data_dir().join(broscribe_core::config::CONFIG_FILE);
    let short_phrase = |name: &str| {
        let p = f.tmp.path().join("in").join(name);
        write_wav(&p, &sine(200.0, 0.1, 0.5));
        p
    };
    let flagged = |passage: &Path, phrase: &Path| {
        let v = f.json(&[
            "setup",
            "voice",
            "--from-file",
            passage.to_str().unwrap(),
            "--confirm-file",
            phrase.to_str().unwrap(),
            "--report-unconfirmed",
            "--session",
            "s-1",
            "--keep-as-enrollment",
            "--name",
            "Robin Example",
        ]);
        assert!(passage.exists() && phrase.exists(), "preserved: {v}");
        v
    };
    let assert_report = |v: &Value, issue: &str| {
        assert_eq!(v["committed"], false, "{v}");
        assert_eq!(v["reason"], "confirmation_unconfirmed", "{v}");
        assert_eq!(v["confirmation_issue"], issue, "{v}");
        assert_eq!(v["voiceprints"], 0, "{v}");
        assert!(v["enrollment_file"].is_null(), "{v}");
        assert!(v["model"].is_string(), "{v}");
        assert!(v.get("person_id").is_none(), "{v}");
        assert!(v["checks"]["duration_s"].is_number(), "{v}");
        let phrase_checks = &v["confirmation_checks"];
        assert!(phrase_checks["duration_s"].is_number(), "{v}");
        let warnings = phrase_checks["warnings"].as_array().unwrap();
        assert!(!warnings.iter().any(|w| w == "short_speech"), "{v}");
        assert_eq!(
            v["confirmation_min_speech_s"].as_f64(),
            Some(CONFIRM_MIN_SPEECH_S),
            "{v}"
        );
        let speech_s = phrase_checks["speech_s"].as_f64().unwrap();
        assert_eq!(issue == "too_short", speech_s < CONFIRM_MIN_SPEECH_S, "{v}");
    };

    // Without the flag, a phrase too short to compare stays an error.
    let config_before = std::fs::read(&config_file).unwrap();
    let wav = f.passage_wav("p0.wav", 200.0);
    let phrase = short_phrase("c0.wav");
    let (code, out, _) = f.run(&[
        "--json",
        "setup",
        "voice",
        "--from-file",
        wav.to_str().unwrap(),
        "--confirm-file",
        phrase.to_str().unwrap(),
        "--session",
        "s-1",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(code, 1, "{out}");
    let e: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(e["error"]["code"], "invalid_input", "{e}");

    // A different voice: a report with the real comparison, nothing saved.
    let v = flagged(
        &f.passage_wav("p1.wav", 200.0),
        &f.phrase_wav("c1.wav", 440.0),
    );
    assert_report(&v, "below_threshold");
    assert_eq!(v["confirmation"]["passed"], false, "{v}");
    assert_eq!(v["confirmation"]["method"], "relative", "{v}");
    let score = v["confirmation"]["score"].as_f64().unwrap();
    assert!(
        score < v["confirmation"]["threshold"].as_f64().unwrap(),
        "{v}"
    );

    // Too short to compare: no comparison and no score to report.
    let v = flagged(&f.passage_wav("p2.wav", 200.0), &short_phrase("c2.wav"));
    assert_report(&v, "too_short");
    assert!(v["confirmation"].is_null(), "{v}");

    // The text output says the check was inconclusive, not that the session
    // was cancelled or that the voice was enrolled.
    let wav = f.passage_wav("p3.wav", 200.0);
    let phrase = f.phrase_wav("c3.wav", 440.0);
    let (code, out, err) = f.run(&[
        "setup",
        "voice",
        "--from-file",
        wav.to_str().unwrap(),
        "--confirm-file",
        phrase.to_str().unwrap(),
        "--report-unconfirmed",
        "--session",
        "s-1",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("inconclusive"), "{out}");
    assert!(out.contains("without --confirm-file"), "{out}");
    assert!(
        !out.contains("cancelled") && !out.contains("Enrolled"),
        "{out}"
    );

    assert!(f.library().self_person().unwrap().is_none());
    assert_eq!(std::fs::read(&config_file).unwrap(), config_before);

    // The user's explicit save of the first recording without the phrase.
    let wav = f.passage_wav("p4.wav", 200.0);
    let v = f.json(&[
        "setup",
        "voice",
        "--from-file",
        wav.to_str().unwrap(),
        "--session",
        "s-1",
        "--keep-as-enrollment",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(v["committed"], true, "{v}");
    assert!(v["confirmation"].is_null(), "{v}");
    let kept = PathBuf::from(v["enrollment_file"].as_str().unwrap());
    let sha = kept.file_stem().unwrap().to_str().unwrap().to_owned();
    let lib = f.library();
    let me = lib.self_person().unwrap().unwrap();
    let vps = lib.load_voiceprints(&me.id).unwrap();
    assert_eq!(
        vps.embeddings.len() as u64,
        v["voiceprints"].as_u64().unwrap()
    );
    assert!(
        vps.embeddings
            .iter()
            .all(|e| e.source.sha256.as_deref() == Some(sha.as_str())),
        "only the first recording is enrolled"
    );
    let (_, out, _) = f.run(&["setup", "voice", "--delete-enrollment"]);
    assert!(out.contains("Deleted 1"), "{out}");

    // A later unconfirmed redo (same voice, unreachable calibrated
    // threshold) leaves the saved profile and config alone.
    set_suggest(&f, Some(1.5));
    let config_before = std::fs::read(&config_file).unwrap();
    let v = flagged(
        &f.passage_wav("p5.wav", 200.0),
        &f.phrase_wav("c5.wav", 200.0),
    );
    assert_report(&v, "below_threshold");
    assert_eq!(v["confirmation"]["method"], "calibrated", "{v}");
    assert_eq!(lib.load_voiceprints(&me.id).unwrap(), vps);
    assert_eq!(std::fs::read(&config_file).unwrap(), config_before);
    assert!(
        !lib.person_dir(&me.id).join("enrollment").exists(),
        "no passage kept"
    );

    // A matching phrase with the flag commits as usual.
    set_suggest(&f, None);
    let v = flagged(
        &f.passage_wav("p6.wav", 200.0),
        &f.phrase_wav("c6.wav", 200.0),
    );
    assert_eq!(v["committed"], true, "{v}");
    assert!(v.get("reason").is_none(), "{v}");
    assert_eq!(v["confirmation"]["passed"], true, "{v}");
    assert!(v["voiceprints"].as_u64().unwrap() >= 4, "{v}");
    let config = Config::load(&f.data_dir()).unwrap();
    assert!(config.setup.completed_steps.iter().any(|s| s == "voice"));
}

#[test]
fn setup_voice_report_unconfirmed_keeps_cancellation_and_reading_errors() {
    let mut f = Fixture::new();
    f.set_session(Some("s-new"));
    let run_flagged = |f: &Fixture, passage: &Path, phrase: &Path, session: &str| {
        f.run(&[
            "--json",
            "setup",
            "voice",
            "--from-file",
            passage.to_str().unwrap(),
            "--confirm-file",
            phrase.to_str().unwrap(),
            "--report-unconfirmed",
            "--session",
            session,
            "--name",
            "Robin Example",
        ])
    };

    // A superseded session reports its cancellation, not an offer to save.
    let (code, out, _) = run_flagged(
        &f,
        &f.passage_wav("old.wav", 200.0),
        &f.phrase_wav("old-c.wav", 440.0),
        "s-old",
    );
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["committed"], false, "{v}");
    assert_eq!(v["reason"], "session_superseded", "{v}");
    assert!(v.get("confirmation_issue").is_none(), "{v}");

    // A first recording with too few usable parts stays an error.
    let short = f.tmp.path().join("in").join("short.wav");
    write_wav(&short, &spoken(18.0, 0.5));
    let (code, out, _) = run_flagged(&f, &short, &f.phrase_wav("c1.wav", 440.0), "s-new");
    assert_eq!(code, 1, "{out}");
    let e: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(e["error"]["code"], "invalid_input", "{e}");

    // So does a reading whose parts the speaker model cannot embed.
    f.deps.models = Box::new(UnembeddableProvider);
    let (code, out, _) = run_flagged(
        &f,
        &f.passage_wav("p2.wav", 200.0),
        &f.phrase_wav("c2.wav", 200.0),
        "s-new",
    );
    assert_eq!(code, 1, "{out}");
    let e: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(e["error"]["code"], "invalid_input", "{e}");
    assert!(f.library().self_person().unwrap().is_none());

    // The flag needs both recordings and no other source or mode.
    let wav = f.passage_wav("p3.wav", 200.0);
    let phrase = f.phrase_wav("c3.wav", 200.0);
    let (wav, phrase) = (wav.to_str().unwrap(), phrase.to_str().unwrap());
    let refused: [&[&str]; 4] = [
        &["--from-file", wav, "--report-unconfirmed"],
        &["--mic", "--report-unconfirmed"],
        &["--confirm-file", phrase, "--report-unconfirmed"],
        &["--show-passage", "--report-unconfirmed"],
    ];
    for extra in refused {
        let mut args = vec!["setup", "voice"];
        args.extend_from_slice(extra);
        let (code, out, err) = f.run(&args);
        assert_eq!(code, 2, "{extra:?}: {out}{err}");
    }
    assert!(Path::new(wav).is_file() && Path::new(phrase).is_file());
}

/// Shares a fake microphone with the test so it can count captures.
struct SharedMic(std::sync::Arc<FakeMicCapture>);
impl broscribe_platform::MicCapture for SharedMic {
    fn devices(&self) -> broscribe_core::Result<Vec<broscribe_platform::AudioDevice>> {
        broscribe_platform::MicCapture::devices(self.0.as_ref())
    }
    fn start(
        &self,
        device: Option<&str>,
        sink: broscribe_platform::MicSink,
    ) -> broscribe_core::Result<Box<dyn broscribe_platform::CaptureSession>> {
        broscribe_platform::MicCapture::start(self.0.as_ref(), device, sink)
    }
}

#[test]
fn setup_voice_mic_stops_after_a_first_recording_without_enough_parts() {
    let mut f = Fixture::new();
    let mut mic = fake_mic();
    mic.chunks = 180; // 18 s: three usable parts
    let mic = std::sync::Arc::new(mic);
    f.deps.mic = Box::new(SharedMic(mic.clone()));
    let phrase = f.json(&["setup", "voice", "--show-passage"])["confirmation_phrase"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, out, err) = f.run(&[
        "--json",
        "setup",
        "voice",
        "--mic",
        "--seconds",
        "18",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(code, 1, "{out}{err}");
    let e: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(e["error"]["code"], "invalid_input", "{e}");
    assert_eq!(mic.started.lock().unwrap().len(), 1, "no second capture");
    assert!(
        !err.contains(&phrase),
        "the phrase was never asked for: {err}"
    );
    assert!(f.library().self_person().unwrap().is_none());
}

type VadInputs = std::sync::Arc<std::sync::Mutex<Vec<usize>>>;

/// The fake VAD, recording the length of every input it analyses.
struct RecordingVad(VadInputs, broscribe_models::fakes::EnergyVad);
impl broscribe_models::Vad for RecordingVad {
    fn segments(
        &mut self,
        samples: &[f32],
    ) -> broscribe_core::Result<Vec<broscribe_models::SpeechSegment>> {
        self.0.lock().unwrap().push(samples.len());
        broscribe_models::Vad::segments(&mut self.1, samples)
    }
}
struct RecordingVadProvider(VadInputs);
impl broscribe_models::ModelProvider for RecordingVadProvider {
    fn load(
        &self,
        profile: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        let mut set =
            broscribe_models::ModelProvider::load(&FakeModelProvider::default(), profile)?;
        set.vad = Box::new(RecordingVad(self.0.clone(), Default::default()));
        Ok(set)
    }
    fn load_vad(&self, _: &str) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        Ok(Box::new(RecordingVad(self.0.clone(), Default::default())))
    }
    fn load_embedder(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        broscribe_models::ModelProvider::load_embedder(&FakeModelProvider::default(), model)
    }
}

#[test]
fn setup_voice_mic_analyses_the_first_recording_once() {
    let mut f = Fixture::new();
    let inputs = VadInputs::default();
    f.deps.models = Box::new(RecordingVadProvider(inputs.clone()));
    let mic = std::sync::Arc::new(fake_mic());
    f.deps.mic = Box::new(SharedMic(mic.clone()));
    let v = f.json(&[
        "setup",
        "voice",
        "--mic",
        "--seconds",
        "22",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(v["committed"], true, "{v}");
    assert_eq!(mic.started.lock().unwrap().len(), 2);
    // The 22 s reading once (the early check, reused by enrollment), then
    // the fixed-length confirmation capture.
    assert_eq!(
        *inputs.lock().unwrap(),
        [22 * 16_000, CONFIRM_MAX_CAPTURE_S as usize * 16_000]
    );
}

/// The confirmation voice of the probe tests: quieter than the 0.5 reading,
/// so the speaker model can tell which recording an input came from.
const PHRASE_AMPLITUDE: f32 = 0.3;
/// What the probing speaker model does with confirmation audio.
const PHRASE_EMBEDS: u8 = 0;
const PHRASE_NO_VECTOR: u8 = 1;
const PHRASE_FAILS: u8 = 2;

/// The fake model set with a speaker model that counts the confirmation
/// inputs it is given and treats them as `mode` says; the reading's inputs
/// embed as usual.
#[derive(Clone, Default)]
struct PhraseProbe {
    phrase_inputs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
}
impl PhraseProbe {
    fn set_mode(&self, mode: u8) {
        self.mode.store(mode, std::sync::atomic::Ordering::SeqCst);
    }
    /// Confirmation inputs embedded since the last call.
    fn take_phrase_inputs(&self) -> usize {
        self.phrase_inputs
            .swap(0, std::sync::atomic::Ordering::SeqCst)
    }
    fn embedder(
        &self,
        inner: Box<dyn broscribe_models::SpeakerEmbedder + Send>,
    ) -> Box<dyn broscribe_models::SpeakerEmbedder + Send> {
        Box::new(ProbeEmbedder {
            inner,
            probe: self.clone(),
        })
    }
}
impl broscribe_models::ModelProvider for PhraseProbe {
    fn load(
        &self,
        profile: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        let set = broscribe_models::ModelProvider::load(&FakeModelProvider::default(), profile)?;
        Ok(broscribe_models::ModelSet {
            embedder: self.embedder(set.embedder),
            vad: set.vad,
            asr: set.asr,
            diarizer: set.diarizer,
        })
    }
    fn load_vad(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        broscribe_models::ModelProvider::load_vad(&FakeModelProvider::default(), model)
    }
    fn load_embedder(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        let inner =
            broscribe_models::ModelProvider::load_embedder(&FakeModelProvider::default(), model)?;
        Ok(self.embedder(inner))
    }
}
struct ProbeEmbedder {
    inner: Box<dyn broscribe_models::SpeakerEmbedder + Send>,
    probe: PhraseProbe,
}
impl broscribe_models::SpeakerEmbedder for ProbeEmbedder {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed(&self, samples: &[f32]) -> broscribe_core::Result<Option<Vec<f32>>> {
        let peak = samples.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        if (PHRASE_AMPLITUDE - peak).abs() < 0.05 {
            self.probe
                .phrase_inputs
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.probe.mode.load(std::sync::atomic::Ordering::SeqCst) {
                PHRASE_NO_VECTOR => return Ok(None),
                PHRASE_FAILS => {
                    return Err(broscribe_core::Error::Inference(
                        "synthetic speaker model failure".into(),
                    ));
                }
                _ => {}
            }
        }
        self.inner.embed(samples)
    }
}

/// `seconds` of the probe tests' confirmation voice.
fn phrase_voice(freq: f32, seconds: f64) -> Vec<f32> {
    sine(freq, seconds, PHRASE_AMPLITUDE)
}

#[test]
fn setup_voice_confirmation_needs_detected_speech_before_comparing() {
    let mut f = Fixture::new();
    let probe = PhraseProbe::default();
    f.deps.models = Box::new(probe.clone());
    f.set_session(Some("s-1"));
    let config_file = f.data_dir().join(broscribe_core::config::CONFIG_FILE);
    let config_before = std::fs::read(&config_file).unwrap();
    // Runs with both recordings (the reading at 200 Hz); returns the exit
    // code and the JSON output, after checking both inputs were preserved.
    let run = |name: &str, phrase: &[f32], flagged: bool| {
        let passage = f.passage_wav(&format!("{name}-p.wav"), 200.0);
        let phrase_file = f.tmp.path().join("in").join(format!("{name}-c.wav"));
        write_wav(&phrase_file, phrase);
        let mut args = vec![
            "--json",
            "setup",
            "voice",
            "--from-file",
            passage.to_str().unwrap(),
            "--confirm-file",
            phrase_file.to_str().unwrap(),
            "--session",
            "s-1",
            "--keep-as-enrollment",
            "--name",
            "Robin Example",
        ];
        if flagged {
            args.push("--report-unconfirmed");
        }
        let (code, out, err) = f.run(&args);
        assert!(
            passage.exists() && phrase_file.exists(),
            "preserved: {out}{err}"
        );
        let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}{err}"));
        (code, v)
    };
    let report = |name: &str, phrase: &[f32], issue: &str| {
        let (code, v) = run(name, phrase, true);
        assert_eq!(code, 0, "{v}");
        assert_eq!(v["committed"], false, "{v}");
        assert_eq!(v["reason"], "confirmation_unconfirmed", "{v}");
        assert_eq!(v["confirmation_issue"], issue, "{v}");
        assert_eq!(v["voiceprints"], 0, "{v}");
        assert!(v["enrollment_file"].is_null(), "{v}");
        assert_eq!(
            v["confirmation_min_speech_s"].as_f64(),
            Some(CONFIRM_MIN_SPEECH_S),
            "{v}"
        );
        if issue == "below_threshold" {
            assert!(v["confirmation"]["score"].is_number(), "{v}");
        } else {
            assert!(
                v["confirmation"].is_null(),
                "no score without a comparison: {v}"
            );
        }
        v
    };
    let speech_s = |v: &Value| v["confirmation_checks"]["speech_s"].as_f64().unwrap();
    let rejected = |code: i32, v: &Value| {
        assert_eq!(code, 1, "{v}");
        assert_eq!(v["error"]["code"], "invalid_input", "{v}");
        v["error"]["message"].as_str().unwrap().to_owned()
    };

    // Four short bursts spread over a 25 s capture: the speech span and the
    // capture are far longer than the minimum, the detected speech is not.
    let mut scattered = silence(3.0);
    for _ in 0..4 {
        scattered.extend(phrase_voice(440.0, 1.8));
        scattered.extend(silence(3.0));
    }
    scattered.extend(silence(2.8));
    let v = report("scattered", &scattered, "too_short");
    let scattered_s = speech_s(&v);
    assert!((7.0..CONFIRM_MIN_SPEECH_S).contains(&scattered_s), "{v}");
    assert!((v["confirmation_checks"]["duration_s"].as_f64().unwrap() - 25.0).abs() < 0.01);
    assert_eq!(
        probe.take_phrase_inputs(),
        0,
        "never given to the speaker model"
    );
    // Without the flag: an error, still without a comparison or model call.
    let (code, v) = run("scattered-plain", &scattered, false);
    let message = rejected(code, &v);
    assert!(!message.contains("threshold"), "{message}");
    assert_eq!(probe.take_phrase_inputs(), 0);

    // The exact boundary (whole 32 ms VAD frames): one frame short is too
    // short; exactly the minimum is compared (a different voice here).
    let v = report("below", &phrase_voice(440.0, 7.968), "too_short");
    assert!((speech_s(&v) - 7.968).abs() < 1e-9, "{v}");
    assert_eq!(probe.take_phrase_inputs(), 0);
    let v = report("exact", &phrase_voice(440.0, 8.0), "below_threshold");
    assert_eq!(speech_s(&v), CONFIRM_MIN_SPEECH_S, "{v}");
    assert_eq!(probe.take_phrase_inputs(), 1);

    // Enough speech, but the speaker model gives no vector: unusable, not
    // too short; with or without the flag.
    probe.set_mode(PHRASE_NO_VECTOR);
    let v = report("unusable", &phrase_voice(200.0, 9.0), "unusable");
    assert!(speech_s(&v) >= CONFIRM_MIN_SPEECH_S, "{v}");
    assert_eq!(probe.take_phrase_inputs(), 1);
    let (code, v) = run("unusable-plain", &phrase_voice(200.0, 9.0), false);
    let message = rejected(code, &v);
    assert!(message.contains("9.0 s"), "{message}");
    assert!(!message.contains("threshold"), "{message}");
    assert_eq!(probe.take_phrase_inputs(), 1);

    // A real speaker-model failure stays an error, even with the flag.
    probe.set_mode(PHRASE_FAILS);
    let (code, v) = run("failed", &phrase_voice(200.0, 9.0), true);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "inference", "{v}");
    assert_eq!(probe.take_phrase_inputs(), 1);
    probe.set_mode(PHRASE_EMBEDS);

    // None of that saved anything or marked the step.
    assert!(f.library().self_person().unwrap().is_none());
    assert_eq!(std::fs::read(&config_file).unwrap(), config_before);

    // Enough speech in the same voice between long silences confirms.
    let mut padded = silence(8.0);
    padded.extend(phrase_voice(200.0, 9.0));
    padded.extend(silence(8.0));
    let (code, v) = run("padded", &padded, true);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["committed"], true, "{v}");
    assert_eq!(v["confirmation"]["passed"], true, "{v}");
    assert_eq!(probe.take_phrase_inputs(), 1);
    let me = f.library().self_person().unwrap().unwrap();
    let vps = f.library().load_voiceprints(&me.id).unwrap();

    // A later too-short redo leaves that enrollment alone; the explicit
    // save without the phrase still works.
    let config_before = std::fs::read(&config_file).unwrap();
    report("redo", &phrase_voice(200.0, 5.0), "too_short");
    assert_eq!(f.library().load_voiceprints(&me.id).unwrap(), vps);
    assert_eq!(std::fs::read(&config_file).unwrap(), config_before);
    let wav = f.passage_wav("skip.wav", 200.0);
    let v = f.json(&[
        "setup",
        "voice",
        "--from-file",
        wav.to_str().unwrap(),
        "--session",
        "s-1",
        "--name",
        "Robin Example",
    ]);
    assert_eq!(v["committed"], true, "{v}");
    assert!(v["confirmation"].is_null(), "{v}");
    let config = Config::load(&f.data_dir()).unwrap();
    assert!(config.setup.completed_steps.iter().any(|s| s == "voice"));
}

/// A microphone that plays one scripted recording per capture.
struct ScriptedMic(std::sync::Mutex<std::collections::VecDeque<Vec<f32>>>);
impl broscribe_platform::MicCapture for ScriptedMic {
    fn devices(&self) -> broscribe_core::Result<Vec<broscribe_platform::AudioDevice>> {
        broscribe_platform::MicCapture::devices(&fake_mic())
    }
    fn start(
        &self,
        _: Option<&str>,
        mut sink: broscribe_platform::MicSink,
    ) -> broscribe_core::Result<Box<dyn broscribe_platform::CaptureSession>> {
        let samples = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted capture");
        for chunk in samples.chunks(1_600) {
            sink(broscribe_platform::MicChunk::new(chunk.to_vec(), 16_000));
        }
        Ok(Box::new(ScriptedSession(Some(sink))))
    }
}
struct ScriptedSession(Option<broscribe_platform::MicSink>);
impl broscribe_platform::CaptureSession for ScriptedSession {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn failure(&self) -> Option<String> {
        None
    }
    fn stop(mut self: Box<Self>) -> broscribe_core::Result<()> {
        drop(self.0.take());
        Ok(())
    }
}

#[test]
fn setup_voice_mic_confirmation_counts_detected_speech_not_capture_time() {
    let mut f = Fixture::new();
    let capture = CONFIRM_MAX_CAPTURE_S as f64;
    // A full-length confirmation capture holding `speech` seconds of voice.
    let confirmation = |speech: f64| {
        let mut a = silence(3.0);
        a.extend(sine(200.0, speech, 0.5));
        a.extend(silence(capture - 3.0 - speech));
        a
    };
    f.deps.mic = Box::new(ScriptedMic(std::sync::Mutex::new(
        [
            sine(200.0, 22.0, 0.5),
            confirmation(6.0),
            sine(200.0, 22.0, 0.5),
            confirmation(10.0),
        ]
        .into(),
    )));
    let text = f.json(&["setup", "voice", "--show-passage"])["confirmation_phrase"]
        .as_str()
        .unwrap()
        .to_owned();
    let args = [
        "--json",
        "setup",
        "voice",
        "--mic",
        "--seconds",
        "22",
        "--name",
        "Robin Example",
    ];

    // 6 s of voice in a 25 s capture: refused, nothing saved.
    let (code, out, err) = f.run(&args);
    assert_eq!(code, 1, "{out}{err}");
    let e: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(e["error"]["code"], "invalid_input", "{e}");
    assert!(
        err.contains(&text),
        "the confirmation text was shown: {err}"
    );
    assert!(f.library().self_person().unwrap().is_none());
    let config = Config::load(&f.data_dir()).unwrap();
    assert!(!config.setup.completed_steps.iter().any(|s| s == "voice"));

    // 10 s of voice in the same capture length is compared and confirms.
    let v = f.json(&args[1..]);
    assert_eq!(v["committed"], true, "{v}");
    assert_eq!(v["confirmation"]["passed"], true, "{v}");
}

#[test]
fn setup_try_preserves_source_with_or_without_saving_a_meeting() {
    let f = Fixture::new();
    let wav = f.wav("try.wav");
    let original = std::fs::read(&wav).unwrap();
    let v = f.json(&["setup", "try", "--from-file", wav.to_str().unwrap()]);
    assert!(v["transcript"].as_str().unwrap().contains("pricing"), "{v}");
    assert!(v["meeting_id"].is_null());
    assert_eq!(v["me"], "none");
    assert_eq!(v["recognised_me"], false);
    assert_eq!(std::fs::read(&wav).unwrap(), original);

    let wav = f.wav("Synthetic sample.WAV");
    let v = f.json(&[
        "setup",
        "try",
        "--from-file",
        wav.to_str().unwrap(),
        "--save-as-meeting",
    ]);
    assert_eq!(v["me"], "none");
    let id = v["meeting_id"].as_str().unwrap().to_owned();
    assert!(wav.exists());
    let (code, out, _) = f.run(&["show", &id]);
    assert_eq!(code, 0, "{out}");
    let library = f.library();
    let meeting = library
        .load_meeting(&broscribe_core::MeetingId::parse(&id).unwrap())
        .unwrap();
    assert_eq!(meeting.recording.unwrap().path, Path::new("audio.wav"));
    assert_eq!(meeting.title.as_deref(), Some("Synthetic sample"));

    let v = f.json(&["setup", "try", "--seconds", "5"]);
    assert!(v["duration_s"].as_f64().unwrap() > 4.0, "{v}");
}

#[test]
fn import_respects_automatic_and_explicit_language_for_files_and_folders() {
    let f = Fixture::new();
    Config::update(&f.data_dir(), |config| {
        config.default_language = Some("en".into());
        Ok(())
    })
    .unwrap();

    let file = f.wav("auto.wav");
    let imported = f.json(&["import", file.to_str().unwrap(), "--auto-language"]);
    let read = |id: &Value| f.json(&["show", id.as_str().unwrap()]);
    assert_eq!(
        read(&imported["meeting_id"])["languages"],
        serde_json::json!([])
    );

    for (i, (flags, expected)) in [
        (&["--auto-language"][..], None),
        (&["--lang", "pt-BR"][..], Some("pt-BR")),
        (&[][..], Some("en")),
    ]
    .into_iter()
    .enumerate()
    {
        let folder = f.tmp.path().join(format!("folder-{i}"));
        std::fs::create_dir(&folder).unwrap();
        for n in 0..2 {
            let samples = sine(230.0 + (i * 2 + n) as f32, 4.0, 0.5);
            write_wav(&folder.join(format!("clip-{n}.wav")), &samples);
        }
        let args = ["import", folder.to_str().unwrap()]
            .into_iter()
            .chain(flags.iter().copied())
            .collect::<Vec<_>>();
        let result = f.json(&args);
        let imported = result["imported"].as_array().unwrap();
        assert_eq!(imported.len(), 2, "{result}");
        for outcome in imported {
            let shown = read(&outcome["meeting_id"]);
            assert_eq!(shown["languages"][0]["lang"].as_str(), expected, "{shown}");
        }
    }
}

#[test]
fn forget_previews_and_refuses_without_explicit_confirmation() {
    let f = Fixture::new();
    let source = f.passage_wav("synthetic-reading.wav", 200.0);
    let enrollment = f.json(&[
        "setup",
        "voice",
        "--from-file",
        source.to_str().unwrap(),
        "--name",
        "Robin Synthetic",
        "--keep-as-enrollment",
    ]);
    let reading = PathBuf::from(enrollment["enrollment_file"].as_str().unwrap());
    let id = import_one(&f, "synthetic-meeting.wav");
    f.json(&["speakers", "name", &id, "S1", "Robin Synthetic"]);
    let before = f.json(&["people", "show", "Robin Synthetic"]);
    let shown = f.json(&["show", &id]);
    for command in ["forget-voice", "forget"] {
        assert_eq!(f.run(&["people", command, "Robin Synthetic"]).0, 2);
        let preview = f.json(&["people", command, "Robin Synthetic", "--dry-run"]);
        assert_eq!(preview["plan"]["kept_readings"], 1, "{preview}");
        assert!(preview["plan"]["voice_samples"].as_u64().unwrap() > 0);
        assert_eq!(preview["plan"]["named_meetings"], 1);
        assert_eq!(preview["voice_only"], command == "forget-voice");
        for section in ["goes", "changes", "stays"] {
            assert!(!preview[section].as_array().unwrap().is_empty());
        }
        assert!(reading.is_file());
        assert_eq!(f.json(&["people", "show", "Robin Synthetic"]), before);
        assert_eq!(f.json(&["show", &id]), shown);
    }
    let forgotten = f.json(&["people", "forget", "Robin Synthetic", "--yes"]);
    assert_eq!(forgotten["person_deleted"], true);
    assert!(!reading.exists());
    assert!(f.json(&["show", &id])["speakers"][0]["person_id"].is_null());
}

#[test]
fn import_participants_and_people_edit() {
    let f = Fixture::new();
    let wav = f.wav("p.wav");
    let v = f.json(&[
        "import",
        wav.to_str().unwrap(),
        "--participant",
        "Ana Example <ana@example.com>",
        "--participant",
        "Bo Test",
    ]);
    let id = v["meeting_id"].as_str().unwrap().to_owned();
    let m = f
        .library()
        .load_meeting(&MeetingId::parse(&id).unwrap())
        .unwrap();
    assert_eq!(m.participants.len(), 2);
    assert_eq!(m.participants[0].email.as_deref(), Some("ana@example.com"));
    assert_eq!(m.participants[1].name.as_deref(), Some("Bo Test"));

    f.json(&["speakers", "name", &id, "S1", "Ana Example"]);
    let notes = f.tmp.path().join("notes.txt");
    std::fs::write(&notes, "Leads pricing.\n").unwrap();
    let v = f.json(&[
        "people",
        "edit",
        "Ana Example",
        "--name",
        "Ana B. Example",
        "--alias",
        "+Ani",
        "--email",
        "ana@example.com",
        "--notes-file",
        notes.to_str().unwrap(),
    ]);
    assert_eq!(v["changed"], true, "{v}");
    assert!(v["meetings_rerendered"].as_u64().unwrap() >= 1);
    let p = f.json(&["people", "show", "Ani"]);
    assert_eq!(p["name"], "Ana B. Example");
    assert_eq!(p["emails"], serde_json::json!(["ana@example.com"]));
    assert!(p["notes"].as_str().unwrap().contains("Leads pricing."));
    let v = f.json(&["people", "edit", "Ana B. Example", "--alias", "-Ani"]);
    assert_eq!(v["person"]["aliases"], serde_json::json!([]));
    let (code, out, _) = f.run(&["--json", "people", "edit", "Nobody Here", "--name", "X"]);
    assert_eq!(code, 1);
    assert!(out.contains("not_found"), "{out}");
}

#[test]
fn read_commands_do_not_create_a_library() {
    let f = Fixture::new();
    for args in [
        &["--json", "search", "pricing"][..],
        &["--json", "identity", "re-resolve", "--dry-run"],
        &["--json", "speakers", "review", "--dismissed"],
    ] {
        let (code, out, _) = f.run(args);
        assert_eq!(code, 1, "{args:?}");
        assert!(out.contains("not_found"), "{args:?}: {out}");
        assert!(!f.lib().exists(), "{args:?}");
    }
}

#[test]
fn retention_commands_and_filter_only_search() {
    let f = Fixture::new();
    let id = import_one(&f, "r.wav");
    f.json(&["tag", &id, "+kept"]);
    let by_tag = f.json(&["search", "--tag", "kept", "--mode", "keyword"]);
    assert!(
        !by_tag["results"].as_array().unwrap().is_empty(),
        "{by_tag}"
    );
    let (code, out, _) = f.run(&["--json", "search"]);
    assert_eq!(code, 1, "{out}");

    let dry = f.json(&["retention", "apply", "--dry-run"]);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["checked"], 1);

    let kept = f.json(&["retention", "keep", &id]);
    assert_eq!(kept["retention_override"], "keep", "{kept}");
    assert_eq!(kept["recording_state"], "available");

    // delete-now asks for a dry run or an explicit --yes.
    let (code, _, _) = f.run(&["--json", "retention", "delete-now", &id]);
    assert_eq!(code, 2);
    let plan = f.json(&["retention", "delete-now", &id, "--dry-run"]);
    assert_eq!(plan["dry_run"], true, "{plan}");
    assert_eq!(plan["verdict"], "delete", "{plan}");
    // The dry run saved nothing: the keep choice still holds.
    let explained = f.json(&["retention", "explain", &id]);
    assert_eq!(explained["meetings"][0]["verdict"], "keep", "{explained}");
    assert_eq!(
        explained["meetings"][0]["reason"], "override",
        "{explained}"
    );

    let gone = f.json(&["retention", "delete-now", &id, "--yes"]);
    assert_eq!(gone["deleted"], true, "{gone}");
    assert_eq!(gone["recording_state"], "deleted");
    // The transcript stays.
    let (code, text, _) = f.run(&["transcript", &id]);
    assert_eq!(code, 0);
    assert!(text.contains("pricing"));
}

#[test]
fn keyword_search_and_directory_commands_work_without_configured_integrations() {
    let f = Fixture::new();
    let id = import_one(&f, "directory.wav");
    let result = f.json(&["search", "pricing", "--mode", "keyword"]);
    assert_eq!(result["mode"], "keyword");
    assert!(result["note"].is_null());
    assert!(!result["results"].as_array().unwrap().is_empty());
    let lookup = f.json(&["people", "lookup", "Synthetic", "--integration", "google"]);
    assert!(lookup["results"].as_array().unwrap().is_empty());
    // Without --integration, signed-out accounts are skipped, not errors.
    let lookup = f.json(&["people", "lookup", "Synthetic"]);
    assert!(lookup["results"].as_array().unwrap().is_empty(), "{lookup}");
    assert!(lookup["errors"].as_array().unwrap().is_empty(), "{lookup}");
    let (code, _, _) = f.run(&["people", "lookup", "   "]);
    assert_ne!(code, 0);
    let (code, _, _) = f.run(&["people", "photo", "Nobody Synthetic"]);
    assert_ne!(code, 0);
    f.json(&["speakers", "name", &id, "S1", "Casey Synthetic"]);
    f.json(&[
        "people",
        "edit",
        "Casey Synthetic",
        "--email",
        "+casey@example.invalid",
    ]);
    let photo = f.json(&["people", "photo", "Casey Synthetic"]);
    assert!(photo["photos"].as_array().unwrap().is_empty(), "{photo}");
    assert!(photo["errors"].as_array().unwrap().is_empty(), "{photo}");
}

#[test]
fn people_edit_with_expected_generation_commits_only_for_that_sign_in() {
    use broscribe_integrations::{AccountState, AccountStatus, IntegrationId, account_dir};
    let f = Fixture::new();
    let id = import_one(&f, "link.wav");
    f.json(&["speakers", "name", &id, "S1", "Casey Synthetic"]);
    let account = |generation, status| {
        let dir = account_dir(&f.data_dir(), IntegrationId::Google);
        std::fs::create_dir_all(&dir).unwrap();
        let state = AccountState {
            generation,
            status,
            ..Default::default()
        };
        broscribe_core::fsutil::write_json_atomic(&dir.join("state.json"), &state).unwrap();
    };
    let link = |expect: &str| {
        f.run(&[
            "--json",
            "people",
            "edit",
            "Casey Synthetic",
            "--email=+casey@example.invalid",
            &format!("--expect-generation={expect}"),
        ])
    };
    let emails = || f.json(&["people", "show", "Casey Synthetic"])["emails"].clone();
    // The search ran at generation 2; the account was replaced (now 3).
    account(3, AccountStatus::Active);
    let (code, out, _) = link("google:2");
    assert_ne!(code, 0, "{out}");
    let error: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(error["error"]["code"], "superseded");
    assert_eq!(emails(), serde_json::json!([]));
    // Signed out since (the generation is kept, the account is gone).
    account(3, AccountStatus::SignedOut);
    assert_ne!(link("google:3").0, 0);
    assert_eq!(emails(), serde_json::json!([]));
    for bad in ["google", "google:x", "nowhere:3"] {
        assert_ne!(link(bad).0, 0, "{bad}");
    }
    assert_eq!(emails(), serde_json::json!([]));
    // Still signed in at the search's generation: the address is added.
    account(3, AccountStatus::Active);
    let (code, out, _) = link("google:3");
    assert_eq!(code, 0, "{out}");
    assert_eq!(emails(), serde_json::json!(["casey@example.invalid"]));
}

#[test]
fn immediate_retention_never_keeps_enrollment_passage() {
    let f = Fixture::new();
    let mut config = Config::load(&f.data_dir()).unwrap();
    config.retention = broscribe_core::config::Retention::DeleteImmediately {
        wait_until_named: true,
    };
    config.save(&f.data_dir()).unwrap();
    let wav = f.passage_wav("immediate.wav", 200.0);
    let result = f.json(&[
        "setup",
        "voice",
        "--from-file",
        wav.to_str().unwrap(),
        "--name",
        "Robin Synthetic",
        "--keep-as-enrollment",
    ]);
    assert_eq!(result["committed"], true);
    assert!(result["enrollment_file"].is_null());
    let lib = f.library();
    let me = lib.self_person().unwrap().unwrap();
    assert!(!lib.person_dir(&me.id).join("enrollment").exists());
    assert!(!lib.load_voiceprints(&me.id).unwrap().embeddings.is_empty());
}

#[test]
fn try_it_resolves_people_read_only_with_configured_thresholds() {
    let f = Fixture::new();
    let id = import_one(&f, "known.wav");
    let named = f.json(&["speakers", "name", &id, "S1", "Robin Synthetic"]);
    let person = broscribe_core::PersonId::parse(named["person_id"].as_str().unwrap()).unwrap();
    let library = f.library();
    let mut me = library.load_person(&person).unwrap();
    me.is_self = true;
    library.write(|tx| tx.save_person(&me)).unwrap();
    let before = library.list_people().unwrap();
    let wav = f.wav("try-known.wav");
    let result = f.json(&["setup", "try", "--from-file", wav.to_str().unwrap()]);
    assert!(
        result["transcript"]
            .as_str()
            .unwrap()
            .contains("Robin Synthetic"),
        "{result}"
    );
    assert_eq!(result["me"], "suggested");
    assert_eq!(result["recognised_me"], false);
    assert_eq!(f.library().list_people().unwrap(), before);
    assert_eq!(f.library().list_meeting_dirs().unwrap().len(), 1);
    let mut config = Config::load(&f.data_dir()).unwrap();
    let model = config.profile.speaker_embedding.clone();
    config.identity.insert(
        model.clone(),
        broscribe_core::config::Thresholds {
            auto_name: Some(0.5),
            ..Default::default()
        },
    );
    config.save(&f.data_dir()).unwrap();
    let wav = f.wav("try-named.wav");
    let result = f.json(&["setup", "try", "--from-file", wav.to_str().unwrap()]);
    assert_eq!(result["me"], "named");
    assert_eq!(result["recognised_me"], true);
    config.identity.insert(
        model,
        broscribe_core::config::Thresholds {
            auto_name: Some(1.1),
            suggest: Some(1.1),
            ..Default::default()
        },
    );
    config.save(&f.data_dir()).unwrap();
    let wav = f.wav("try-threshold.wav");
    let result = f.json(&["setup", "try", "--from-file", wav.to_str().unwrap()]);
    assert!(
        !result["transcript"]
            .as_str()
            .unwrap()
            .contains("Robin Synthetic"),
        "{result}"
    );
    assert_eq!(result["me"], "none");
    assert_eq!(result["recognised_me"], false);
}

struct AbortingEmbedder(broscribe_models::fakes::ToneEmbedder);
impl broscribe_models::SpeakerEmbedder for AbortingEmbedder {
    fn model_id(&self) -> &str {
        &self.0.model_id
    }
    fn dim(&self) -> usize {
        self.0.dim
    }
    fn embed(&self, _: &[f32]) -> broscribe_core::Result<Option<Vec<f32>>> {
        std::process::abort()
    }
}
struct AbortingProvider;
impl broscribe_models::ModelProvider for AbortingProvider {
    fn load(
        &self,
        profile: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        broscribe_models::ModelProvider::load(&FakeModelProvider::default(), profile)
    }
    fn load_vad(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        broscribe_models::ModelProvider::load_vad(&FakeModelProvider::default(), model)
    }
    fn load_embedder(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        Ok(Box::new(AbortingEmbedder(
            broscribe_models::fakes::ToneEmbedder::new(model, 16),
        )))
    }
}

#[test]
fn confirm_aborting_embedding_child() {
    let Some(root) = std::env::var_os("BROSCRIBE_CONFIRM_ABORT_TEST") else {
        return;
    };
    let mut f = Fixture::new();
    f.deps.paths = Box::new(FakePaths::new(PathBuf::from(&root).join("root")));
    f.deps.models = Box::new(AbortingProvider);
    let args = vec![
        OsString::from("broscribe"),
        OsString::from("--library"),
        PathBuf::from(root).join("lib").into_os_string(),
        OsString::from("queue"),
        OsString::from("work"),
    ];
    let code = run(args, &f.deps, &mut Vec::new(), &mut Vec::new());
    panic!("embedding child should abort, returned {code}");
}

#[test]
fn confirm_embedding_child_abort_never_partially_confirms() {
    let f = Fixture::new();
    let id = import_one(&f, "confirm-crash.wav");
    let named = f.json(&["speakers", "name", &id, "S1", "Robin Synthetic"]);
    let person = broscribe_core::PersonId::parse(named["person_id"].as_str().unwrap()).unwrap();
    let lib = f.library();
    let queue = Queue::open(lib.root()).unwrap();
    // The naming's speaker recheck runs to the end first, so the snapshot
    // below changes only through the aborted job.
    f.json(&["queue", "work"]);
    let recheck =
        broscribe_core::queue::JobId::parse(named["learning"]["job_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        queue.get(&recheck).unwrap().state,
        broscribe_core::queue::JobState::Done
    );
    let meeting = lib.load_meeting(&MeetingId::parse(&id).unwrap()).unwrap();
    let before = lib.load_voiceprints(&person).unwrap();
    let model = Config::load(&f.data_dir())
        .unwrap()
        .profile
        .speaker_embedding;
    let job = lib
        .write(|tx| {
            queue.enqueue(
                tx,
                "confirm-samples",
                "aborting",
                serde_json::json!({
                    "meeting_id": id, "speaker_id": "S1", "person_id": person, "model": model,
                }),
            )
        })
        .unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "confirm_aborting_embedding_child"])
        .env("BROSCRIBE_CONFIRM_ABORT_TEST", f.tmp.path())
        .status()
        .unwrap();
    assert!(!status.success());
    // The aborted child still holds a (dead) lease; plain claims count no
    // attempt, so the crash counts when that lease is taken over.
    let aborted = queue.get(&job).unwrap();
    assert_eq!(aborted.attempts, 0);
    assert!(aborted.lease.is_some());
    let takeover = queue
        .claim(&lib, &job, &FakeProcessProbe::new(std::process::id()))
        .unwrap()
        .unwrap();
    takeover.release(&lib).unwrap();
    assert_eq!(queue.get(&job).unwrap().attempts, 1);
    assert_eq!(lib.load_meeting(&meeting.id).unwrap(), meeting);
    assert_eq!(lib.load_voiceprints(&person).unwrap(), before);
}

struct PausedProvider {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl broscribe_models::ModelProvider for PausedProvider {
    fn load(
        &self,
        profile: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        broscribe_models::ModelProvider::load(&FakeModelProvider::default(), profile)
    }
    fn load_vad(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        broscribe_models::ModelProvider::load_vad(&FakeModelProvider::default(), model)
    }
    fn load_embedder(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        broscribe_models::ModelProvider::load_embedder(&FakeModelProvider::default(), model)
    }
}

#[test]
fn naming_and_search_continue_while_a_fake_import_is_paused() {
    let f = Fixture::new();
    let id = import_one(&f, "available.wav");
    let wav = f.passage_wav("slow.wav", 440.0);
    let library = f.lib();
    let root = f.tmp.path().join("root");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let child = std::thread::spawn(move || {
        let mut child = Fixture::new();
        child.deps.paths = Box::new(FakePaths::new(root));
        child.deps.models = Box::new(PausedProvider {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
        });
        let args = vec![
            "broscribe".into(),
            "--library".into(),
            library.into_os_string(),
            "import".into(),
            wav.into_os_string(),
        ];
        run(args, &child.deps, &mut Vec::new(), &mut Vec::new())
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let named = f.json(&["speakers", "name", &id, "S1", "Robin Synthetic"]);
    assert!(named["person_id"].is_string());
    let results = f.json(&["search", "pricing", "--mode", "keyword"]);
    assert!(!results["results"].as_array().unwrap().is_empty());
    release_tx.send(()).unwrap();
    assert_eq!(child.join().unwrap(), 0);
}

#[test]
fn try_it_superseded_session_saves_no_meeting() {
    let f = Fixture::new();
    f.set_session(Some("current"));
    let wav = f.wav("superseded-try.wav");
    let result = f.json(&[
        "setup",
        "try",
        "--from-file",
        wav.to_str().unwrap(),
        "--save-as-meeting",
        "--session",
        "old",
    ]);
    assert_eq!(result["committed"], false);
    assert!(f.library().list_meeting_dirs().unwrap().is_empty());
    assert!(wav.exists());
}

#[test]
fn queue_confirmation_commit_runs_after_naming_retention() {
    let f = Fixture::new();
    let id = import_one(&f, "retained-until-confirm.wav");
    let named = f.json(&["speakers", "name", &id, "S1", "Robin Synthetic"]);
    f.json(&["speakers", "name", &id, "S2", "Morgan Synthetic"]);
    let mut config = Config::load(&f.data_dir()).unwrap();
    config.retention = broscribe_core::config::Retention::DeleteImmediately {
        wait_until_named: true,
    };
    config.save(&f.data_dir()).unwrap();
    let lib = f.library();
    let queue = Queue::open(lib.root()).unwrap();
    lib.write(|tx| {
        queue.enqueue(
            tx,
            "confirm-samples",
            "retention-confirm",
            serde_json::json!({
                "meeting_id": id, "speaker_id": "S1", "person_id": named["person_id"],
                "model": config.profile.speaker_embedding,
            }),
        )
    })
    .unwrap();
    let mid = MeetingId::parse(&id).unwrap();
    assert!(
        lib.meeting_audio_path(&lib.load_meeting(&mid).unwrap())
            .unwrap()
            .is_some()
    );
    f.json(&["queue", "work"]);
    assert_eq!(
        lib.load_meeting(&mid).unwrap().recording.unwrap().state,
        broscribe_core::schema::RecordingState::Deleted
    );
}

#[test]
fn integrations_sync_all_continues_after_failing_account() {
    use broscribe_integrations::{AccountState, AccountStatus, IntegrationId, account_dir};
    let f = Fixture::new();
    // Both accounts are signed in with their configured client; neither has a stored
    // credential. Google reads only explicitly selected calendars, so it has one
    // selected: each sync then fails on its own without reaching the network.
    let mut config = Config::load(&f.data_dir()).unwrap();
    for id in [IntegrationId::MicrosoftGraph, IntegrationId::Google] {
        config.integration.insert(
            id.as_str().into(),
            toml::Value::Table(toml::Table::from_iter([(
                "client_id".to_owned(),
                toml::Value::String("synthetic-client".into()),
            )])),
        );
    }
    config.save(&f.data_dir()).unwrap();
    let active = |selected: &[&str]| AccountState {
        status: AccountStatus::Active,
        client_id: Some("synthetic-client".into()),
        native_calendar_ids: selected.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    };
    for id in [IntegrationId::MicrosoftGraph, IntegrationId::Google] {
        let dir = account_dir(&f.data_dir(), id);
        std::fs::create_dir_all(&dir).unwrap();
        let state = match id {
            IntegrationId::Google => active(&["synthetic-calendar"]),
            _ => active(&[]),
        };
        broscribe_core::fsutil::write_json_atomic(&dir.join("state.json"), &state).unwrap();
    }
    let result = f.json(&["integrations", "sync"]);
    let results = result["synced"].as_array().unwrap();
    // Legacy Microsoft credentials are quarantined, never guessed into an account.
    assert_eq!(results.len(), 1, "{result}");
    assert_eq!(results[0]["integration"], "google");
    assert!(results[0]["error"]["code"].is_string(), "{result}");
    let status = f.json(&["integrations", "status", "microsoft-graph"]);
    assert_eq!(status["legacy_microsoft"], "quarantined");
    assert_eq!(
        f.json(&["integrations", "accounts", "microsoft-graph"]),
        serde_json::json!([])
    );
    let (code, _, _) = f.run(&["integrations", "sync", "google"]);
    assert_ne!(code, 0);
}

#[test]
fn discard_legacy_requires_confirmation_and_preserves_verified_accounts() {
    use broscribe_integrations::{AccountState, AccountStatus, IntegrationId, account_dir};
    use std::sync::Arc;
    let mut f = Fixture::new();
    let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
    f.deps.browser = browser.clone();
    let provider = IntegrationId::MicrosoftGraph;
    let dir = account_dir(&f.data_dir(), provider);
    let handle = "11111111111111111111111111111111";
    let account_key = format!(
        "integration/microsoft-graph/accounts/{handle}/tokens/22222222222222222222222222222222"
    );
    let legacy_key = "integration/microsoft-graph/tokens";
    let target = serde_json::json!({"kind":"own","calendar_id":"synthetic-calendar"});
    let mut config = Config::load(&f.data_dir()).unwrap();
    config.integration.insert(
        "microsoft-graph".into(),
        toml::Value::Table(toml::Table::from_iter([(
            "client_id".into(),
            toml::Value::String("synthetic-client".into()),
        )])),
    );
    config.save(&f.data_dir()).unwrap();
    std::fs::create_dir_all(dir.join("accounts").join(handle)).unwrap();
    broscribe_core::fsutil::write_json_atomic(&dir.join("accounts.json"), &serde_json::json!({
        "version":1,"accounts":[{"view":{
            "account":{"integration":"microsoft-graph","account_id":handle},
            "identity":{"authority_host":"login.microsoftonline.com","resource_tenant_id":"synthetic-tenant",
                "object_id":"synthetic-object","client_id":"synthetic-client","verified_addresses":[]},
            "generation":3,"selection_generation":2,"status":"active",
            "granted_scopes":["User.Read","Calendars.Read"],"selected_calendars":[target],
            "calendar":"available","directory":"not_requested","shared_calendars":"not_requested",
            "microsoft365_groups":"not_requested","target_results":[]
        },"secret_key":account_key,"readable_targets":[target],"authorized_scopes":["User.Read","Calendars.Read"]}],
        "pending_secrets":[],"attempts":[],"legacy_quarantined":false,"legacy_checked":false
    })).unwrap();
    broscribe_core::fsutil::write_json_atomic(
        &dir.join("state.json"),
        &AccountState {
            status: AccountStatus::Active,
            generation: 9,
            client_id: Some("synthetic-client".into()),
            ..Default::default()
        },
    )
    .unwrap();
    f.deps
        .secrets
        .set(legacy_key, "synthetic-legacy-secret")
        .unwrap();
    f.deps
        .secrets
        .set(&account_key, "synthetic-account-secret")
        .unwrap();
    std::fs::write(dir.join("calendar.json"), "{\"events\":[]}").unwrap();
    std::fs::create_dir_all(dir.join("photos")).unwrap();
    std::fs::write(dir.join("photos").join("synthetic.jpg"), b"synthetic").unwrap();
    let accounts = f.json(&["integrations", "accounts", "microsoft-graph"]);
    assert_eq!(accounts.as_array().unwrap().len(), 1);
    let status = f.json(&["integrations", "status", "microsoft-graph"]);
    assert_eq!(status["legacy_microsoft"], "quarantined");
    let generation = status["state"]["generation"].as_u64().unwrap().to_string();
    for (args, expected) in [
        (
            vec![
                "discard-legacy",
                "microsoft-graph",
                "--generation",
                &generation,
            ],
            "invalid_input",
        ),
        (
            vec![
                "discard-legacy",
                "microsoft-graph",
                "--generation",
                "8",
                "--yes",
            ],
            "superseded",
        ),
        (
            vec!["discard-legacy", "google", "--generation", "9", "--yes"],
            "account_required",
        ),
    ] {
        let mut command = vec!["--json", "integrations"];
        command.extend(args);
        let (code, out, _) = f.run(&command);
        assert_ne!(code, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["error"]["code"],
            expected
        );
        assert_eq!(
            f.json(&["integrations", "status", "microsoft-graph"])["legacy_microsoft"],
            "quarantined"
        );
        assert_eq!(
            f.deps.secrets.get(legacy_key).unwrap().as_deref(),
            Some("synthetic-legacy-secret")
        );
    }
    let result = f.json(&[
        "integrations",
        "discard-legacy",
        "microsoft-graph",
        "--generation",
        &generation,
        "--yes",
    ]);
    assert_eq!(result["integration"], "microsoft-graph");
    assert_eq!(result["legacy_microsoft"], "none");
    assert!(result.get("deleted").is_none());
    assert!(f.deps.secrets.get(legacy_key).unwrap().is_none());
    assert_eq!(
        f.deps.secrets.get(&account_key).unwrap().as_deref(),
        Some("synthetic-account-secret")
    );
    let retained = f.json(&["integrations", "accounts", "microsoft-graph"]);
    assert_eq!(retained.as_array().unwrap().len(), 1);
    assert_eq!(retained[0]["account"]["account_id"], handle);
    assert_eq!(retained[0]["generation"], 3);
    assert_eq!(retained[0]["selection_generation"], 2);
    assert_eq!(retained[0]["status"], "active");
    assert_eq!(
        retained[0]["selected_calendars"][0]["calendar_id"],
        "synthetic-calendar"
    );
    assert_eq!(
        f.json(&[
            "integrations",
            "discard-legacy",
            "microsoft-graph",
            "--generation",
            &generation,
            "--yes"
        ])["legacy_microsoft"],
        "none"
    );
    assert!(browser.opened.lock().unwrap().is_empty());
}

#[test]
fn microsoft_provider_sync_attempts_accounts_after_a_blocked_account() {
    use broscribe_integrations::{IntegrationId, account_dir};
    use std::sync::Arc;
    let mut f = Fixture::new();
    let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
    f.deps.browser = browser.clone();
    let mut config = Config::load(&f.data_dir()).unwrap();
    config.integration.insert(
        "microsoft-graph".into(),
        toml::Value::Table(toml::Table::from_iter([(
            "client_id".into(),
            toml::Value::String("synthetic-client".into()),
        )])),
    );
    config.save(&f.data_dir()).unwrap();
    let blocked = "11111111111111111111111111111111";
    let available = "22222222222222222222222222222222";
    let accounts = [blocked, available].iter().map(|handle| {
        let key = format!("integration/microsoft-graph/accounts/{handle}/tokens/33333333333333333333333333333333");
        f.deps.secrets.set(&key, r#"{"access_token":"synthetic-access","expires_at":4102444800,"scope":"User.Read Calendars.Read"}"#).unwrap();
        serde_json::json!({"view":{
            "account":{"integration":"microsoft-graph","account_id":handle},
            "identity":{"authority_host":"login.microsoftonline.com","resource_tenant_id":"synthetic-tenant",
                "object_id":handle,"client_id":"synthetic-client","verified_addresses":[]},
            "generation":4,"selection_generation":1,"status":"active",
            "access_error":if *handle == blocked {Some("conditional_access_blocked")} else {None},
            "granted_scopes":["User.Read","Calendars.Read"],"selected_calendars":[],
            "calendar":"available","directory":"not_requested","shared_calendars":"not_requested",
            "microsoft365_groups":"not_requested","target_results":[]
        },"secret_key":key,"authorized_scopes":["User.Read","Calendars.Read"]})
    }).collect::<Vec<_>>();
    let dir = account_dir(&f.data_dir(), IntegrationId::MicrosoftGraph);
    std::fs::create_dir_all(&dir).unwrap();
    broscribe_core::fsutil::write_json_atomic(
        &dir.join("accounts.json"),
        &serde_json::json!({
            "version":1,"accounts":accounts,"pending_secrets":[],"attempts":[],
            "legacy_quarantined":false,"legacy_checked":true
        }),
    )
    .unwrap();
    for args in [
        vec!["integrations", "sync", "microsoft-graph"],
        vec!["integrations", "sync"],
    ] {
        let output = f.json(&args);
        let outcomes = output["synced"].as_array().unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0]["account"]["account_id"], blocked);
        assert_eq!(outcomes[0]["error"]["code"], "conditional_access_blocked");
        assert_eq!(outcomes[1]["account"]["account_id"], available);
        assert_eq!(outcomes[1]["outcome"], "no_selection");
        assert_eq!(outcomes[1]["targets"].as_array().unwrap().len(), 0);
    }
    config
        .integration
        .get_mut("microsoft-graph")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .insert("directory".into(), toml::Value::String("none".into()));
    config.save(&f.data_dir()).unwrap();
    let (code, out, _) = f.run(&[
        "--json",
        "integrations",
        "grant-account",
        "microsoft-graph",
        "--account",
        available,
        "--generation",
        "4",
        "--permission",
        "directory",
    ]);
    assert_ne!(code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["error"]["code"],
        "not_configured"
    );
    assert!(browser.opened.lock().unwrap().is_empty());
}

#[test]
fn corporate_account_commands_return_typed_json_and_refuse_ambiguous_mutations() {
    let f = Fixture::new();
    assert_eq!(f.json(&["integrations", "accounts"]), serde_json::json!([]));
    for command in ["login", "logout"] {
        let (code, out, _) = f.run(&["--json", "integrations", command, "microsoft-graph"]);
        assert_ne!(code, 0);
        let error: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(error["error"]["code"], "account_required");
    }
    let handle = "0123456789abcdef0123456789abcdef";
    for command in ["calendars", "sync-account"] {
        let (code, out, _) = f.run(&[
            "--json",
            "integrations",
            command,
            "microsoft-graph",
            "--account",
            handle,
        ]);
        assert_ne!(code, 0);
        let error: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(error["error"]["code"], "account_not_found");
    }
    let refused = |args: &[&str]| {
        let mut all = vec!["--json", "integrations"];
        all.extend_from_slice(args);
        let (code, out, _) = f.run(&all);
        assert_ne!(code, 0, "{args:?}: {out}");
        serde_json::from_str::<Value>(&out).unwrap()["error"]["code"].clone()
    };
    // Google has one provider-wide account; Microsoft always names one.
    assert_eq!(
        refused(&["calendars", "google", "--account", handle]),
        "invalid_input"
    );
    assert_eq!(
        refused(&["calendars", "microsoft-graph"]),
        "account_required"
    );
    // Refused from argv alone, before any selection is read from stdin.
    for (args, code) in [
        (
            vec![
                "select-calendars",
                "google",
                "--account",
                handle,
                "--generation",
                "1",
                "--selection-generation",
                "1",
                "--spec-stdin",
            ],
            "invalid_input",
        ),
        (
            vec![
                "select-calendars",
                "google",
                "--generation",
                "1",
                "--spec-stdin",
            ],
            "invalid_input",
        ),
        (
            vec![
                "select-calendars",
                "microsoft-graph",
                "--generation",
                "1",
                "--selection-generation",
                "1",
                "--spec-stdin",
            ],
            "account_required",
        ),
        (
            vec![
                "select-calendars",
                "microsoft-graph",
                "--account",
                handle,
                "--generation",
                "1",
                "--spec-stdin",
            ],
            "invalid_input",
        ),
    ] {
        assert_eq!(refused(&args), code, "{args:?}");
    }
    // The old argv form of a selection no longer exists.
    let (code, _, _) = f.run(&[
        "integrations",
        "select-calendars",
        "microsoft-graph",
        "--account",
        handle,
        "--generation",
        "1",
        "--selection-generation",
        "1",
        "--target",
        r#"{"kind":"own","calendar_id":"synthetic"}"#,
    ]);
    assert_eq!(code, 2);
}

/// `broscribe --json integrations <args>` as its own process against `tmp`'s
/// data folder, with `stdin` piped: calendar selections never travel in argv.
fn integrations_stdin(tmp: &Path, args: &[&str], stdin: &str) -> (bool, Value) {
    let mut all = vec!["integrations"];
    all.extend_from_slice(args);
    binary_stdin(tmp, tmp, &all, stdin)
}

/// `broscribe --json <args>` as its own process against `tmp`'s library and
/// data folder, working in `cwd`, with `stdin` piped (what `--spec-stdin`
/// and the other stdin transports read: the process's own stdin).
fn binary_stdin(tmp: &Path, cwd: &Path, args: &[&str], stdin: &str) -> (bool, Value) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_broscribe"))
        .arg("--json")
        .arg("--library")
        .arg(tmp.join("lib"))
        .args(args)
        .current_dir(cwd)
        .env("BROSCRIBE_DATA_DIR", tmp.join("data"))
        .env_remove("BROSCRIBE_LIBRARY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // A refusal can end the child before it reads stdin (broken pipe).
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    (
        out.status.success(),
        serde_json::from_str(&text).unwrap_or(Value::Null),
    )
}

#[test]
fn google_selection_reads_explicit_ids_from_stdin_under_the_catalog_generation() {
    use broscribe_integrations::{AccountState, AccountStatus, IntegrationId, account_dir};
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let dir = account_dir(&data, IntegrationId::Google);
    std::fs::create_dir_all(&dir).unwrap();
    broscribe_core::fsutil::write_json_atomic(
        &dir.join("state.json"),
        &AccountState {
            generation: 3,
            selection_generation: 5,
            status: AccountStatus::Active,
            client_id: Some("synthetic-client".into()),
            native_calendar_ids: vec!["synthetic-old".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let select = |generation: &str, selection: &str, spec: &str| {
        integrations_stdin(
            tmp.path(),
            &[
                "select-calendars",
                "google",
                "--generation",
                generation,
                "--selection-generation",
                selection,
                "--spec-stdin",
            ],
            spec,
        )
    };
    let state = || -> AccountState {
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap()
    };
    let pair = |state: &AccountState| (state.generation, state.selection_generation);
    // Malformed specs and routing targets change nothing.
    for spec in [
        r#"{"calendar_ids":[""]}"#,
        r#"{"targets":[{"kind":"own","calendar_id":"x"}]}"#,
        r#"{"calendar_ids":[],"unknown":true}"#,
        "not json",
    ] {
        let (ok, value) = select("3", "5", spec);
        assert!(!ok, "{spec}: {value}");
        assert_eq!(value["error"]["code"], "invalid_input", "{spec}: {value}");
        assert_eq!(pair(&state()), (3, 5));
    }
    // A catalog of another account generation or of an older selection is
    // refused before anything changes.
    for (generation, selection) in [("2", "5"), ("3", "4")] {
        let (ok, value) = select(generation, selection, r#"{"calendar_ids":[]}"#);
        assert!(!ok, "{value}");
        assert_eq!(value["error"]["code"], "superseded");
        assert_eq!(state().native_calendar_ids, ["synthetic-old"]);
    }
    // An explicit empty selection reads nothing and fences the old one by its
    // selection generation only: the account (directory/photo) one stays.
    let (ok, value) = select("3", "5", r#"{"calendar_ids":[]}"#);
    assert!(ok, "{value}");
    assert_eq!(value["state"]["generation"], 3);
    assert_eq!(value["state"]["selection_generation"], 6);
    assert_eq!(value["agent_notified"], false);
    let saved = state();
    assert_eq!(pair(&saved), (3, 6));
    assert!(saved.native_calendar_ids.is_empty());
    // A selection made under the catalog's old selection is refused.
    let (ok, value) = select("3", "5", r#"{"calendar_ids":[]}"#);
    assert!(!ok, "{value}");
    assert_eq!(value["error"]["code"], "superseded");
    assert_eq!(pair(&state()), (3, 6));
}

/// CL02: `setup --spec-stdin` seeds people from the contacts file it names (a
/// relative path resolving against the working directory, as in argv) and
/// sets Me, with nothing of either in argv.
#[test]
fn setup_spec_seeds_people_from_its_contacts_file_and_names_me() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("in");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("contacts.csv"),
        "Name,Email\nBo Example,bo@example.invalid\nCy Example,cy@example.invalid\n",
    )
    .unwrap();
    let setup = |spec: &str| {
        binary_stdin(
            tmp.path(),
            &cwd,
            &["setup", "--non-interactive", "--spec-stdin"],
            spec,
        )
    };
    // Unknown fields and a missing contacts file are refused; nobody is added.
    for spec in [
        r#"{"people_file":"contacts.csv","words_file":"words.txt"}"#,
        r#"{"people_file":"absent.csv"}"#,
    ] {
        let (ok, value) = setup(spec);
        assert!(!ok, "{spec}: {value}");
    }
    let (ok, value) = setup(
        r#"{"name":"Robin Example","emails":["robin@example.invalid"],"people_file":"contacts.csv"}"#,
    );
    assert!(ok, "{value}");
    assert_eq!(value["people"]["contacts"], 2, "{value}");
    assert_eq!(value["people"]["created"], 2, "{value}");
    assert_eq!(value["me"]["name"], "Robin Example", "{value}");
    assert_eq!(
        value["me"]["emails"],
        serde_json::json!(["robin@example.invalid"])
    );
    let (ok, people) = binary_stdin(tmp.path(), &cwd, &["people"], "");
    assert!(ok, "{people}");
    let mut names: Vec<&str> = people["people"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["Bo Example", "Cy Example", "Robin Example"]);
    // Seeding again updates the same people instead of adding others.
    let (ok, value) = setup(r#"{"people_file":"contacts.csv"}"#);
    assert!(ok, "{value}");
    assert_eq!(value["people"]["created"], 0, "{value}");
    assert!(value.get("me").is_none(), "{value}");
}

/// CL02: `import --spec-stdin` with `{"manifest"}` reads the manifest file
/// where it is (relative to the working directory, as in argv) and refuses
/// what argv refuses beside `--manifest`.
#[test]
fn import_spec_reads_the_manifest_it_names_relative_to_the_working_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("in");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("batch.json"),
        r#"{"format":"synthetic-other-format","version":1,"meetings":[]}"#,
    )
    .unwrap();
    let import = |extra: &[&str], spec: &str| {
        let mut args = vec!["import", "--spec-stdin"];
        args.extend_from_slice(extra);
        binary_stdin(tmp.path(), &cwd, &args, spec)
    };
    // The file at the relative path was read and parsed: its own format is
    // what is refused.
    let (ok, value) = import(&[], r#"{"manifest":"batch.json"}"#);
    assert!(!ok, "{value}");
    assert_eq!(value["error"]["code"], "invalid_manifest", "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("synthetic-other-format"),
        "{value}"
    );
    // Another working directory has no such manifest.
    let (ok, value) = binary_stdin(
        tmp.path(),
        tmp.path(),
        &["import", "--spec-stdin"],
        r#"{"manifest":"batch.json"}"#,
    );
    assert!(!ok, "{value}");
    assert_ne!(value["error"]["code"], "invalid_manifest", "{value}");
    // A manifest carries its own title, participants, language and date.
    for (extra, spec) in [
        (&["--lang", "en"][..], r#"{"manifest":"batch.json"}"#),
        (
            &["--date", "2026-11-10"][..],
            r#"{"manifest":"batch.json"}"#,
        ),
        (&[][..], r#"{"manifest":"batch.json","title":"Synthetic"}"#),
        (&[][..], r#"{"manifest":"batch.json","path":"a.wav"}"#),
    ] {
        let (ok, value) = import(extra, spec);
        assert!(!ok, "{extra:?} {spec}: {value}");
        assert_eq!(
            value["error"]["code"], "invalid_input",
            "{extra:?} {spec}: {value}"
        );
    }
}

/// CL02: `integrations setup system-calendar --spec-stdin` hands the agent
/// exactly the selection on stdin (address-like IDs included, repeats once,
/// empty as an explicit clear); it never needs argv for a calendar.
#[cfg(unix)]
#[test]
fn native_calendar_selection_from_stdin_reaches_the_agent_exactly() {
    use broscribe_ipc::{Call, Request, Response, SystemCalendarAction};
    use std::io::{BufRead, Write};
    // A short folder: the agent's socket lives under the data folder.
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let data = tmp.path().join("data");
    let endpoint = broscribe_platform_native::ipc::Endpoint::new(&data);
    let location = endpoint.socket_location().unwrap();
    broscribe_platform_native::ipc::ensure_private_dir(&location.dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&location.path).unwrap();
    let address = "synthetic.owner@example.invalid";
    let expected = [vec![address.to_owned(), "cal-b".to_owned()], Vec::new()];
    let replies = expected.clone();
    let agent = std::thread::spawn(move || {
        for ids in replies {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                if std::io::BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap()
                    == 0
                {
                    continue;
                }
                let request = Request::parse(&line).unwrap();
                assert_eq!(
                    request.call,
                    Call::SystemCalendar {
                        action: SystemCalendarAction::Configure {
                            calendar_ids: ids.clone()
                        }
                    }
                );
                let reply = serde_json::json!({
                    "integration": "system-calendar",
                    "configured": true,
                    "state": {"generation": 4, "status": "active", "native_calendar_ids": ids},
                });
                let response = Response {
                    id: request.id,
                    outcome: Ok(reply),
                };
                writeln!(stream, "{}", response.to_line().unwrap()).unwrap();
                break;
            }
        }
    });
    let configure = |spec: &str| {
        integrations_stdin(
            tmp.path(),
            &["setup", "system-calendar", "--spec-stdin"],
            spec,
        )
    };
    // Malformed selections are refused before the agent is asked.
    for spec in [
        r#"{}"#,
        r#"{"calendar_ids":[" "]}"#,
        r#"{"calendar_ids":["cal-b"],"targets":[]}"#,
    ] {
        let (ok, value) = configure(spec);
        assert!(!ok, "{spec}: {value}");
        assert_eq!(value["error"]["code"], "invalid_input", "{spec}: {value}");
    }
    let selection = serde_json::json!({"calendar_ids": [address, "cal-b", address]}).to_string();
    let (ok, value) = configure(&selection);
    assert!(ok, "{value}");
    assert_eq!(
        value["state"]["native_calendar_ids"],
        serde_json::json!(expected[0])
    );
    let (ok, value) = configure(r#"{"calendar_ids":[]}"#);
    assert!(ok, "{value}");
    assert_eq!(value["state"]["native_calendar_ids"], serde_json::json!([]));
    agent.join().unwrap();
    // Other integrations refuse the native spec before reading it.
    let (ok, value) = integrations_stdin(
        tmp.path(),
        &["setup", "google", "--spec-stdin"],
        r#"{"calendar_ids":[]}"#,
    );
    assert!(!ok, "{value}");
    assert_eq!(value["error"]["code"], "invalid_input", "{value}");
}

#[test]
fn microsoft_selection_and_removal_read_typed_targets_from_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = "0123456789abcdef0123456789abcdef";
    let account = [
        "--account",
        handle,
        "--generation",
        "1",
        "--selection-generation",
        "1",
        "--spec-stdin",
    ];
    let run = |command: &str, spec: &str| {
        let mut args = vec![command, "microsoft-graph"];
        args.extend(account);
        let (ok, value) = integrations_stdin(tmp.path(), &args, spec);
        assert!(!ok, "{command} {spec}: {value}");
        value["error"]["code"].clone()
    };
    assert_eq!(
        run(
            "select-calendars",
            r#"{"targets":[{"kind":"delegated","mailbox":"","calendar_id":null}]}"#
        ),
        "invalid_calendar_target"
    );
    assert_eq!(
        run("select-calendars", r#"{"calendar_ids":["x"]}"#),
        "invalid_input"
    );
    assert_eq!(
        run(
            "select-calendars",
            r#"{"targets":[{"kind":"own","calendar_id":"synthetic"}]}"#
        ),
        "account_not_found"
    );
    assert_eq!(
        run(
            "remove-calendar",
            r#"{"target":{"kind":"delegated","mailbox":"","calendar_id":null}}"#
        ),
        "invalid_calendar_target"
    );
    assert_eq!(
        run(
            "remove-calendar",
            r#"{"target":{"kind":"own","calendar_id":"synthetic"}}"#
        ),
        "account_not_found"
    );
}

#[test]
fn sign_in_attempts_are_cancelled_durably_and_report_terminal_results() {
    use std::sync::Arc;
    let mut f = Fixture::new();
    let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
    f.deps.browser = browser.clone();
    let refused = |args: &[&str]| {
        let mut all = vec!["--json", "integrations"];
        all.extend_from_slice(args);
        let (code, out, _) = f.run(&all);
        assert_ne!(code, 0, "{args:?}: {out}");
        serde_json::from_str::<Value>(&out).unwrap()["error"].clone()
    };
    // Cancelled before it started: that ID can never sign in.
    let cancelled = f.json(&[
        "integrations",
        "cancel-login",
        "google",
        "--attempt",
        "synthetic-cancelled",
    ]);
    assert_eq!(cancelled["integration"], "google");
    assert_eq!(cancelled["attempt"], "synthetic-cancelled");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        refused(&["login", "google", "--attempt", "synthetic-cancelled"])["code"],
        "login_cancelled"
    );
    assert_eq!(
        f.json(&[
            "integrations",
            "login-attempt",
            "google",
            "--attempt",
            "synthetic-cancelled"
        ])["status"],
        "cancelled"
    );
    // A failed sign-in keeps its sanitized terminal result; its ID is spent.
    assert_eq!(
        refused(&["login", "google", "--attempt", "synthetic-failed"])["code"],
        "not_configured"
    );
    let failed = f.json(&[
        "integrations",
        "login-attempt",
        "google",
        "--attempt",
        "synthetic-failed",
    ]);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["code"], "not_configured");
    // Cancelling after the end returns that end, unchanged.
    assert_eq!(
        f.json(&[
            "integrations",
            "cancel-login",
            "google",
            "--attempt",
            "synthetic-failed"
        ])["status"],
        "failed"
    );
    assert_eq!(
        refused(&["login", "google", "--attempt", "synthetic-failed"])["code"],
        "login_attempt_used"
    );
    assert_eq!(
        f.json(&[
            "integrations",
            "login-attempt",
            "microsoft-graph",
            "--attempt",
            "synthetic-unknown"
        ])["status"],
        "not_found"
    );
    // Native calendars connect through macOS permission: no sign-in attempt.
    assert_eq!(
        refused(&["login", "system-calendar", "--attempt", "synthetic-native"])["code"],
        "invalid_input"
    );
    assert_eq!(
        refused(&[
            "cancel-login",
            "system-calendar",
            "--attempt",
            "synthetic-native"
        ])["code"],
        "login_unsupported"
    );
    assert_eq!(
        refused(&["cancel-login", "google", "--attempt", "not valid!"])["code"],
        "invalid_input"
    );
    assert!(browser.opened.lock().unwrap().is_empty());
    assert_eq!(
        f.json(&["integrations", "status", "google"])["state"]["status"],
        "signed_out"
    );
}

#[test]
fn a_malformed_integration_table_disables_only_that_integration() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.data_dir()).unwrap();
    std::fs::write(
        Config::path(&f.data_dir()),
        "[integration.google]\nclient_id = 123\n\n[integration.microsoft-graph]\nclient_id = \"synthetic-client\"\n",
    )
    .unwrap();
    let all = f.json(&["integrations", "status"]);
    let statuses = all["integrations"].as_array().unwrap();
    let of = |id: &str| {
        statuses
            .iter()
            .find(|s| s["integration"] == id)
            .unwrap()
            .clone()
    };
    let google = of("google");
    assert_eq!(google["configured"], false);
    assert!(
        google["config_error"]
            .as_str()
            .is_some_and(|m| m.contains("integration.google")),
        "{google}"
    );
    let graph = of("microsoft-graph");
    assert_eq!(graph["configured"], true);
    assert!(graph.get("config_error").is_none(), "{graph}");
    assert!(
        of("system-calendar")["helper_error"]["code"].is_string(),
        "{all}"
    );
    assert_eq!(
        f.json(&["integrations", "accounts", "microsoft-graph"]),
        serde_json::json!([])
    );
    let (code, out, _) = f.run(&["--json", "integrations", "login", "google"]);
    assert_ne!(code, 0);
    let error = &serde_json::from_str::<Value>(&out).unwrap()["error"];
    assert_eq!(error["code"], "not_configured");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("integration.google")),
        "{error}"
    );
}

#[test]
fn explicit_import_title_is_user_authored() {
    let f = Fixture::new();
    let wav = f.wav("title.wav");
    let result = f.json(&[
        "import",
        wav.to_str().unwrap(),
        "--title",
        "Synthetic user title",
    ]);
    let meeting = f
        .library()
        .load_meeting(&MeetingId::parse(result["meeting_id"].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(
        meeting.title_source,
        Some(broscribe_core::schema::TitleSource::User)
    );
}

#[test]
fn setup_voice_session_changed_during_inference_commits_nothing() {
    let f = Fixture::new();
    f.set_session(Some("capturing"));
    let lib = Library::open(f.lib()).unwrap();
    let library = f.lib();
    let root = f.tmp.path().join("root");
    let wav = f.passage_wav("racing-voice.wav", 200.0);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let child = std::thread::spawn(move || {
        let mut child = Fixture::new();
        child.deps.paths = Box::new(FakePaths::new(root));
        child.deps.models = Box::new(PausedProvider {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
        });
        let args = vec![
            "broscribe".into(),
            "--json".into(),
            "--library".into(),
            library.into_os_string(),
            "setup".into(),
            "voice".into(),
            "--from-file".into(),
            wav.into_os_string(),
            "--name".into(),
            "Robin Synthetic".into(),
            "--session".into(),
            "capturing".into(),
        ];
        let mut out = Vec::new();
        let code = run(args, &child.deps, &mut out, &mut Vec::new());
        (code, serde_json::from_slice::<Value>(&out).unwrap())
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    lib.write(|_| {
        let mut config = Config::load(&f.data_dir())?;
        config.setup.voice_session = None;
        config.save(&f.data_dir())
    })
    .unwrap();
    release_tx.send(()).unwrap();
    let (code, result) = child.join().unwrap();
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["committed"], false);
    assert!(lib.self_person().unwrap().is_none());
}

#[test]
fn enrollment_applies_expired_retention_immediately_after_commit() {
    let f = Fixture::new();
    let mut config = Config::load(&f.data_dir()).unwrap();
    config.retention = broscribe_core::config::Retention::DeleteAfterDays { days: 0 };
    config.save(&f.data_dir()).unwrap();
    let wav = f.passage_wav("expired-enrollment.wav", 200.0);
    let result = f.json(&[
        "setup",
        "voice",
        "--from-file",
        wav.to_str().unwrap(),
        "--name",
        "Robin Synthetic",
        "--keep-as-enrollment",
    ]);
    assert_eq!(result["committed"], true);
    assert!(result["enrollment_file"].is_null(), "{result}");
    let lib = f.library();
    let me = lib.self_person().unwrap().unwrap();
    assert!(
        std::fs::read_dir(lib.person_dir(&me.id).join("enrollment"))
            .map(|entries| entries.count() == 0)
            .unwrap_or(true)
    );
    assert!(!lib.load_voiceprints(&me.id).unwrap().embeddings.is_empty());
}

#[test]
fn reimport_adds_new_participants_once_to_the_existing_meeting() {
    let f = Fixture::new();
    let wav = f.wav("again.wav");
    let first = f.json(&[
        "import",
        wav.to_str().unwrap(),
        "--participant",
        "Ana Example",
    ]);
    let id = first["meeting_id"].as_str().unwrap().to_owned();
    let again = f.json(&[
        "import",
        wav.to_str().unwrap(),
        "--participant",
        "ana example",
        "--participant",
        "Bo Test <bo@example.com>",
        "--participant",
        "<BO@example.com>",
    ]);
    assert_eq!(again["already_exists"], true, "{again}");
    assert_eq!(again["meeting_id"], id.as_str());
    let m = f
        .library()
        .load_meeting(&MeetingId::parse(&id).unwrap())
        .unwrap();
    let names: Vec<_> = m
        .participants
        .iter()
        .map(|p| (p.name.as_deref(), p.email.as_deref()))
        .collect();
    assert_eq!(
        names,
        [
            (Some("Ana Example"), None),
            (Some("Bo Test"), Some("bo@example.com")),
        ]
    );
}

#[test]
fn keyword_search_ignores_an_invalid_text_embedding_model() {
    let f = Fixture::new();
    import_one(&f, "invalid-embedding.wav");
    let mut config = Config::load(&f.data_dir()).unwrap();
    // A registry model of another role: building the text model fails.
    config.profile.text_embedding = config.profile.asr.clone();
    config.save(&f.data_dir()).unwrap();
    let result = f.json(&["search", "pricing", "--mode", "keyword"]);
    assert_eq!(result["mode"], "keyword");
    assert!(
        !result["results"].as_array().unwrap().is_empty(),
        "{result}"
    );
    // Ranking by meaning does need the model: the error names it.
    let (code, out, _) = f.run(&["--json", "search", "pricing", "--mode", "semantic"]);
    assert_ne!(code, 0, "{out}");
}

#[test]
fn queue_discard_setup_removes_only_setup_imports_of_ended_or_named_sessions() {
    let f = Fixture::new();
    // A library with a queue: one ordinary import first.
    import_one(&f, "ordinary-first.wav");
    let lib = f.library();
    let queue = Queue::open(lib.root()).unwrap();
    let data = f.data_dir();
    f.set_session(Some("killed"));
    let enqueue = |name: &str, session: Option<&str>, handling: &str| {
        let wav = f.wav(name);
        let input = serde_json::json!({
            "path": wav,
            "sha256": name,
            "handling": handling,
            "setup_session": session.map(|s| (data.clone(), s)),
        });
        let id = lib
            .write(|tx| queue.enqueue(tx, "import", name, input))
            .unwrap();
        // A killed child left its staged copy behind.
        std::fs::copy(&wav, queue.job_dir(&id).join("audio.wav")).unwrap();
        id
    };
    let killed = enqueue("killed.wav", Some("killed"), "copy");
    let ended = enqueue("ended.wav", Some("ended"), "copy");
    let moved = enqueue("moved.wav", Some("killed"), "move");
    let ordinary = enqueue("ordinary.wav", None, "copy");

    // The killed child's session (still current in the config).
    let out = f.json(&["queue", "discard-setup", "--session", "killed"]);
    assert_eq!(out["discarded"], serde_json::json!([killed.to_string()]));
    assert!(!queue.job_dir(&killed).exists());
    assert!(queue.job_dir(&ended).join("audio.wav").is_file());

    // Startup recovery: every import whose session ended.
    let out = f.json(&["queue", "discard-setup"]);
    assert_eq!(out["discarded"], serde_json::json!([ended.to_string()]));
    assert!(!queue.job_dir(&ended).exists());
    // Never a move (its staged file is the user's original) or an ordinary import.
    assert!(queue.job_dir(&moved).join("audio.wav").is_file());
    assert!(queue.job_dir(&ordinary).join("audio.wav").is_file());
}

/// The JSON row of provider `id` in `providers list`.
fn provider_row(f: &Fixture, id: &str) -> Value {
    let list = f.json(&["providers", "list"]);
    list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("{id} missing: {list}"))
        .clone()
}

#[test]
fn providers_list_shows_consent_text_and_key_presence_only() {
    let f = Fixture::new();
    let reg = broscribe_models::llm::ProviderRegistry::builtin().unwrap();
    for id in ["claude-cli", "openai-http", "openai-embeddings"] {
        let row = provider_row(&f, id);
        assert_eq!(
            row["consent_text"],
            reg.get(id).unwrap().consent_text(),
            "{row}"
        );
    }
    assert_eq!(provider_row(&f, "claude-cli")["has_key"], Value::Null);
    assert_eq!(provider_row(&f, "openai-http")["has_key"], false);

    // An invented key: the list says that it exists, never what it is.
    f.deps
        .secrets
        .set("provider/openai-http/api_key", "sk-synthetic-list")
        .unwrap();
    assert_eq!(provider_row(&f, "openai-http")["has_key"], true);
    let (code, out, err) = f.run(&["providers", "list"]);
    assert_eq!(code, 0, "{err}");
    let (_, json_out, _) = f.run(&["--json", "providers", "list"]);
    assert!(!out.contains("sk-synthetic") && !json_out.contains("sk-synthetic"));

    // Enabling goes through the locked config edit and shows in the list.
    f.json(&["providers", "enable", "claude-cli", "--accept"]);
    assert_eq!(provider_row(&f, "claude-cli")["enabled"], true);
    f.json(&["providers", "disable", "claude-cli"]);
    assert_eq!(provider_row(&f, "claude-cli")["enabled"], false);
}

#[test]
fn providers_delete_key_is_idempotent_and_set_key_refuses_agent_clis() {
    let f = Fixture::new();
    let account = "provider/openai-embeddings/api_key";
    f.deps.secrets.set(account, "sk-synthetic-delete").unwrap();
    let v = f.json(&["providers", "delete-key", "openai-embeddings"]);
    assert_eq!(
        v,
        serde_json::json!({ "provider": "openai-embeddings", "deleted": true })
    );
    assert_eq!(f.deps.secrets.get(account).unwrap(), None);
    let v = f.json(&["providers", "delete-key", "openai-embeddings"]);
    assert_eq!(v["deleted"], false);

    // Agent CLIs sign in on their own: no key is read or stored for them.
    for cmd in ["set-key", "delete-key"] {
        let (code, out, _) = f.run(&["--json", "providers", cmd, "claude-cli"]);
        assert_eq!(code, 1, "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "invalid_input", "{out}");
    }
    let (code, out, _) = f.run(&["--json", "providers", "set-key", "no-such-provider"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        f.deps
            .secrets
            .get("provider/claude-cli/api_key")
            .unwrap()
            .is_none()
    );
}

#[test]
fn models_use_text_embedding_accepts_models_and_embedding_providers() {
    let f = Fixture::new();
    let v = f.json(&["models", "use", "text_embedding", "openai-embeddings"]);
    assert_eq!(v["role"], "text_embedding");
    assert_eq!(v["changed"], true);
    assert_eq!(v["kind"], "provider");
    assert_eq!(v["enabled"], false);
    let config = Config::load(&f.data_dir()).unwrap();
    assert_eq!(config.profile.text_embedding, "openai-embeddings");

    let v = f.json(&[
        "models",
        "use",
        "text-embedding",
        "potion-multilingual-128m",
    ]);
    assert_eq!(v["kind"], "model");
    assert_eq!(v["previous"], "openai-embeddings");
    let config = Config::load(&f.data_dir()).unwrap();
    assert_eq!(config.profile.text_embedding, "potion-multilingual-128m");

    // An LLM provider, a model of another role and an unknown ID are refused.
    for id in ["openai-http", "nemo-titanet-large", "no-such-model"] {
        let (code, out, _) = f.run(&["--json", "models", "use", "text_embedding", id]);
        assert_eq!(code, 1, "{id}: {out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "invalid_input", "{id}: {out}");
    }
    let config = Config::load(&f.data_dir()).unwrap();
    assert_eq!(config.profile.text_embedding, "potion-multilingual-128m");
}

#[test]
fn import_enrich_is_refused_before_importing_without_a_consented_provider() {
    let f = Fixture::new();
    let file = f.wav("enrich-refused.wav");
    let folder = file.parent().unwrap().to_path_buf();
    for path in [&file, &folder] {
        let (code, out, _) = f.run(&["--json", "import", path.to_str().unwrap(), "--enrich"]);
        assert_eq!(code, 1, "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], "invalid_input", "{out}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no LLM provider is enabled with consent"),
            "{out}"
        );
    }
    // Nothing was imported: the library was not even created.
    assert!(!f.lib().exists());
}

/// PL10: the enrichment asked for while importing (`--enrich`) is queued as
/// requested, never `auto` (the worker's `auto_enrich` check cannot drop
/// it); the one `auto_enrich = new` gives a plain import is automatic.
#[test]
fn import_enrich_queues_requested_passes_and_auto_enrich_automatic_ones() {
    use broscribe_core::config::{AutoEnrich, ProviderConfig};
    use broscribe_core::queue::JobId;
    use broscribe_pipeline::enrich::ENRICH_KIND;
    let f = Fixture::new();
    let dir = f.data_dir();
    let set_auto = |auto: AutoEnrich| {
        let mut c = Config::load(&dir).unwrap();
        c.auto_enrich = auto;
        c.providers.insert(
            "openai-http".into(),
            ProviderConfig {
                enabled: true,
                consented_at: Some(broscribe_core::schema::now()),
                model: Some("synthetic-model".into()),
                base_url: Some("http://127.0.0.1:9/v1".into()),
            },
        );
        c.save(&dir).unwrap();
    };
    // The `auto` flag of the one enrichment job an import queued.
    let auto_of = |v: &Value| {
        let queued = v["enrichment"]["queued"]
            .as_array()
            .unwrap_or_else(|| panic!("{v}"));
        assert_eq!(queued.len(), 1, "{v}");
        let job = Queue::open(f.lib())
            .unwrap()
            .get(&JobId::parse(queued[0].as_str().unwrap()).unwrap())
            .unwrap();
        assert_eq!(job.kind, ENRICH_KIND);
        job.input.get("auto").cloned()
    };

    set_auto(AutoEnrich::Off);
    let requested = f.wav("requested.wav");
    let v = f.json(&["import", requested.to_str().unwrap(), "--enrich"]);
    assert_eq!(auto_of(&v), None, "{v}");

    set_auto(AutoEnrich::New);
    let automatic = f.tmp.path().join("in").join("automatic.wav");
    let mut audio = conversation();
    audio.extend(silence(1.0));
    write_wav(&automatic, &audio);
    let v = f.json(&["import", automatic.to_str().unwrap()]);
    assert_eq!(auto_of(&v), Some(Value::Bool(true)), "{v}");
}

#[test]
fn import_enrich_reports_busy_meetings_without_hiding_queued_work() {
    use broscribe_core::config::{AutoEnrich, ProviderConfig};
    use broscribe_core::queue::{ENRICH_KIND, JobId, JobState};
    let f = Fixture::new();
    let mut config = Config::load(&f.data_dir()).unwrap();
    config.auto_enrich = AutoEnrich::New;
    config.providers.insert(
        "openai-http".into(),
        ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            model: Some("synthetic-model".into()),
            base_url: Some("http://127.0.0.1:9/v1".into()),
        },
    );
    config.save(&f.data_dir()).unwrap();
    let first = f.wav("first.wav");
    let first_import = f.json(&["import", first.to_str().unwrap()]);
    let second = f.tmp.path().join("in").join("second.wav");
    let mut audio = conversation();
    audio.extend(silence(1.0));
    write_wav(&second, &audio);
    let second_import = f.json(&["import", second.to_str().unwrap()]);
    let first_job =
        JobId::parse(first_import["enrichment"]["queued"][0].as_str().unwrap()).unwrap();
    let second_job =
        JobId::parse(second_import["enrichment"]["queued"][0].as_str().unwrap()).unwrap();
    let lib = Library::open(f.lib()).unwrap();
    let queue = Queue::open(f.lib()).unwrap();
    let lease = queue
        .claim(&lib, &second_job, f.deps.process.as_ref())
        .unwrap()
        .expect("the automatic enrichment is claimable");
    let claimed = lease.claimed_job().input.clone();
    assert_eq!(claimed["auto"], true);
    config.auto_enrich = AutoEnrich::Off;
    config.save(&f.data_dir()).unwrap();
    let manifest = f.tmp.path().join("enrich.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "format": "broscribe-import-manifest", "version": 1,
            "meetings": [{"audio_path": first}, {"audio_path": second}],
        }))
        .unwrap(),
    )
    .unwrap();
    let result = f.json(&[
        "import",
        "--manifest",
        manifest.to_str().unwrap(),
        "--enrich",
    ]);
    assert_eq!(
        result["enrichment"]["queued"],
        serde_json::json!([first_job])
    );
    assert_eq!(
        result["enrichment"]["failed"][0]["meeting_id"],
        second_import["meeting_id"]
    );
    assert_eq!(result["enrichment"]["failed"][0]["code"], "library_busy");
    assert_eq!(result["enrichment"]["failed"].as_array().unwrap().len(), 1);
    let promoted = queue.get(&first_job).unwrap();
    assert_eq!(promoted.kind, ENRICH_KIND);
    assert_ne!(promoted.input["auto"], true);
    let running = queue.get(&second_job).unwrap();
    assert_eq!(running.state, JobState::Running);
    assert_eq!(running.input, claimed);
    lease.release(&lib).unwrap();
}

#[test]
fn global_library_flag_after_tag_values_is_not_a_tag() {
    let f = Fixture::new();
    let id = import_one(&f, "talk.wav");
    let lib = f.lib();
    for flag in [
        vec![OsString::from("--library"), lib.clone().into()],
        vec![format!("--library={}", lib.display()).into()],
    ] {
        let mut argv: Vec<OsString> = ["broscribe", "--json", "tag", &id, "+kept"]
            .iter()
            .map(OsString::from)
            .collect();
        argv.extend(flag);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &f.deps, &mut out, &mut err);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{out}{}", String::from_utf8_lossy(&err));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["tags"], serde_json::json!(["kept"]), "{v}");
    }
}

#[test]
fn setup_keeps_config_written_by_another_process_meanwhile() {
    let f = Fixture::new();
    Library::open(f.lib()).unwrap();
    let library = f.lib();
    let root = f.tmp.path().join("root");
    // The lock holds `setup` between loading its config and naming Me.
    let guard = LibraryLock::acquire(&f.lib(), Duration::ZERO).unwrap();
    let child = std::thread::spawn(move || {
        let mut child = Fixture::new();
        child.deps.paths = Box::new(FakePaths::new(root));
        let args = [
            "broscribe",
            "--json",
            "--library",
            library.to_str().unwrap(),
            "setup",
            "--non-interactive",
            "--name",
            "Robin Synthetic",
        ];
        let mut out = Vec::new();
        let code = run(
            args.iter().map(OsString::from).collect(),
            &child.deps,
            &mut out,
            &mut Vec::new(),
        );
        (code, String::from_utf8(out).unwrap())
    });
    std::thread::sleep(Duration::from_millis(500));
    // Another process (the app) stores a setting meanwhile.
    let data = f.data_dir();
    let mut c = Config::load(&data).unwrap();
    c.integration
        .insert("synthetic".into(), toml::Value::Boolean(true));
    c.save(&data).unwrap();
    drop(guard);
    let (code, out) = child.join().unwrap();
    assert_eq!(code, 0, "{out}");
    let c = Config::load(&data).unwrap();
    assert_eq!(c.setup.name.as_deref(), Some("Robin Synthetic"));
    assert!(c.integration.contains_key("synthetic"), "{c:?}");
}

/// Names every other name writer refuses: a right-to-left override, a
/// control character, and one character over the length limit.
fn refused_me_names() -> [String; 3] {
    [
        "Ana\u{202e}elpmaxE".to_owned(),
        "Ana\u{7}Example".to_owned(),
        "a".repeat(201),
    ]
}

#[test]
fn setup_refuses_a_me_name_from_stdin_before_saving_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let words = tmp.path().join("words.txt");
    std::fs::write(&words, "ExampleCorp\n").unwrap();
    let setup = |spec: Value| {
        binary_stdin(
            tmp.path(),
            tmp.path(),
            &[
                "setup",
                "--non-interactive",
                "--lang",
                "en",
                "--words-file",
                words.to_str().unwrap(),
                "--spec-stdin",
            ],
            &spec.to_string(),
        )
    };
    for name in refused_me_names() {
        let spec = serde_json::json!({ "name": name, "emails": ["robin@example.com"] });
        let (ok, v) = setup(spec);
        assert!(!ok, "{v}");
        assert_eq!(v["error"]["code"], "invalid_input", "{v}");
    }
    // Neither Me nor the steps saved before it in a run were saved.
    let config = Config::load(&data).unwrap();
    assert_eq!(config.setup.name, None);
    assert!(config.setup.emails.is_empty(), "{config:?}");
    assert!(config.setup.completed_steps.is_empty(), "{config:?}");
    assert!(config.default_language.is_none() && config.hotwords.is_empty());
    let lib = Library::open(tmp.path().join("lib")).unwrap();
    assert!(lib.self_person().unwrap().is_none());
    assert!(
        lib.find_person_by_email("robin@example.com")
            .unwrap()
            .is_none()
    );

    // A valid invented name is saved trimmed, and a later run that gives no
    // name keeps it.
    let spec = serde_json::json!({ "name": "  Robin Example ", "emails": ["robin@example.com"] });
    let (ok, v) = setup(spec);
    assert!(ok, "{v}");
    assert_eq!(v["me"]["name"], "Robin Example", "{v}");
    let (ok, v) = setup(serde_json::json!({ "emails": ["robin@work.example"] }));
    assert!(ok, "{v}");
    let me = lib.self_person().unwrap().unwrap();
    assert_eq!(me.name, "Robin Example");
    assert_eq!(me.emails, ["robin@example.com", "robin@work.example"]);
    let config = Config::load(&data).unwrap();
    assert_eq!(config.setup.name.as_deref(), Some("Robin Example"));
    assert_eq!(config.default_language.as_deref(), Some("en"));
}

#[test]
fn setup_refuses_a_people_file_with_a_refused_name_before_saving_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let contacts = tmp.path().join("people.csv");
    let setup = || {
        let spec = serde_json::json!({
            "name": "Robin Example",
            "people_file": contacts.to_str().unwrap(),
        });
        binary_stdin(
            tmp.path(),
            tmp.path(),
            &["setup", "--non-interactive", "--lang", "en", "--spec-stdin"],
            &spec.to_string(),
        )
    };
    // The second contact's name is refused: not even the first is seeded.
    for bad in refused_me_names() {
        std::fs::write(
            &contacts,
            format!("Name,Email\nBo Test,bo@example.com\n{bad},cy@example.com\n"),
        )
        .unwrap();
        let (ok, v) = setup();
        assert!(!ok, "{v}");
        assert_eq!(v["error"]["code"], "invalid_input", "{v}");
    }
    let config = Config::load(&data).unwrap();
    assert_eq!(config.setup.name, None);
    assert!(config.setup.completed_steps.is_empty(), "{config:?}");
    assert!(config.default_language.is_none());
    let lib = Library::open(tmp.path().join("lib")).unwrap();
    assert!(lib.self_person().unwrap().is_none());
    for email in ["bo@example.com", "cy@example.com"] {
        assert!(
            lib.find_person_by_email(email).unwrap().is_none(),
            "{email}"
        );
    }

    // Valid invented contacts are seeded; one without a name goes by its
    // e-mail.
    std::fs::write(
        &contacts,
        "Name,Email\nBo Test,bo@example.com\n ,dee@example.com\n",
    )
    .unwrap();
    let (ok, v) = setup();
    assert!(ok, "{v}");
    assert_eq!(v["people"]["created"], 2, "{v}");
    let bo = lib.find_person_by_email("bo@example.com").unwrap().unwrap();
    assert_eq!(bo.name, "Bo Test");
    let dee = lib
        .find_person_by_email("dee@example.com")
        .unwrap()
        .unwrap();
    assert_eq!(dee.name, "dee@example.com");
    assert_eq!(lib.self_person().unwrap().unwrap().name, "Robin Example");
    let config = Config::load(&data).unwrap();
    assert!(config.setup.completed_steps.iter().any(|s| s == "people"));
}

#[test]
fn setup_voice_refuses_a_me_name_from_stdin_before_saving_anything() {
    let f = Fixture::new();
    let data = f.tmp.path().join("data");
    let voice = |args: &[&str], spec: Value| {
        let mut all = vec!["setup", "voice"];
        all.extend_from_slice(args);
        all.push("--spec-stdin");
        binary_stdin(f.tmp.path(), f.tmp.path(), &all, &spec.to_string())
    };
    // A name `setup` saved before names were checked is not given to a new
    // Me either.
    let legacy = "Ana\u{202e}elpmaxE";
    let mut config = Config::default();
    config.setup.name = Some(legacy.to_owned());
    config.save(&data).unwrap();
    let specs = refused_me_names()
        .map(|name| serde_json::json!({ "name": name }))
        .into_iter()
        .chain([serde_json::json!({})]);
    for spec in specs {
        let wav = f.passage_wav("p.wav", 200.0);
        let (ok, v) = voice(&["--from-file", wav.to_str().unwrap()], spec.clone());
        assert!(!ok, "{spec}: {v}");
        assert_eq!(v["error"]["code"], "invalid_input", "{spec}: {v}");
    }
    assert!(f.library().self_person().unwrap().is_none());
    let config = Config::load(&data).unwrap();
    assert!(config.setup.completed_steps.is_empty(), "{config:?}");
    assert_eq!(config.setup.name.as_deref(), Some(legacy));

    // A valid invented name names Me (here from a meeting's speaker, by its
    // stored centroid: the process has no installed models).
    let id = import_one(&f, "m.wav");
    let (ok, v) = voice(
        &["--from", &id, "--speaker", "S2"],
        serde_json::json!({ "name": " Robin Example " }),
    );
    assert!(ok, "{v}");
    let me = f.library().self_person().unwrap().unwrap();
    assert_eq!(v["person_id"], me.id.as_str(), "{v}");
    assert_eq!(me.name, "Robin Example");
    let config = Config::load(&data).unwrap();
    assert_eq!(config.setup.name.as_deref(), Some("Robin Example"));
    assert!(config.setup.completed_steps.iter().any(|s| s == "voice"));
}

#[test]
fn setup_profile_switching_the_speaker_model_reembeds_the_library() {
    let f = Fixture::new();
    let mut c = Config::default();
    c.profile.speaker_embedding = "nemo-titanet-small".into();
    c.save(&f.data_dir()).unwrap();
    let id = MeetingId::parse(&import_one(&f, "talk.wav")).unwrap();
    let models = |f: &Fixture| -> Vec<String> {
        let m = f.library().load_meeting(&id).unwrap();
        m.speakers
            .iter()
            .filter_map(|s| s.centroid.as_ref().map(|c| c.model.clone()))
            .collect()
    };
    let before = models(&f);
    assert!(!before.is_empty());
    assert!(before.iter().all(|m| m == "nemo-titanet-small"));

    let v = f.json(&["setup", "--non-interactive", "--profile", "default"]);
    assert_eq!(v["reembed"]["model"], "nemo-titanet-large", "{v}");
    assert!(models(&f).iter().all(|m| m == "nemo-titanet-large"));
    assert_eq!(
        Config::load(&f.data_dir())
            .unwrap()
            .profile
            .speaker_embedding,
        "nemo-titanet-large"
    );
}

/// Loads a speaker-embedding model only once it is installed in `data_dir`,
/// as the real provider does.
struct InstalledOnlyProvider {
    data_dir: PathBuf,
}
impl broscribe_models::ModelProvider for InstalledOnlyProvider {
    fn load(
        &self,
        profile: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        broscribe_models::ModelProvider::load(&FakeModelProvider::default(), profile)
    }
    fn load_vad(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        broscribe_models::ModelProvider::load_vad(&FakeModelProvider::default(), model)
    }
    fn load_embedder(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        if !broscribe_models::ModelStore::new(&self.data_dir).is_installed(model) {
            return Err(broscribe_core::Error::ModelMissing(model.into()));
        }
        broscribe_models::ModelProvider::load_embedder(&FakeModelProvider::default(), model)
    }
}

/// Serves `body` over plain HTTP on a local port for every request.
fn serve_forever(body: &'static [u8]) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body);
        }
    });
    port
}

#[test]
fn setup_pull_models_downloads_before_loading_the_new_embedder() {
    const BODY: &[u8] = b"synthetic model bytes";
    let mut f = Fixture::new();
    let data = f.data_dir();
    f.deps.models = Box::new(InstalledOnlyProvider {
        data_dir: data.clone(),
    });
    let mut c = Config::default();
    c.profile.speaker_embedding = "nemo-titanet-small".into();
    c.save(&data).unwrap();
    // The default profile's models, plus the multilingual profile's Whisper
    // (no live role; it declares Parakeet as its live model), served as
    // tiny synthetic files.
    let port = serve_forever(BODY);
    let profile = broscribe_core::config::Profile::default();
    let models = [
        (profile.vad.as_str(), "\"vad\"", "onnx", ""),
        (profile.asr.as_str(), "\"asr\", \"live\"", "onnx", ""),
        (profile.diarization.as_str(), "\"diarization\"", "onnx", ""),
        (
            profile.speaker_embedding.as_str(),
            "\"speaker_embedding\"",
            "onnx",
            "",
        ),
        (
            "whisper-large-v3-turbo",
            "\"asr\"",
            "whisper",
            "live_model = \"parakeet-tdt-0.6b-v3\"\n",
        ),
    ];
    let mut registry = String::new();
    for (id, roles, runtime, extra) in models {
        registry.push_str(&format!(
            "[[model]]\nid = \"{id}\"\nroles = [{roles}]\nruntime = \"{runtime}\"\n\
             languages = [\"*\"]\nlicense = \"MIT\"\nlicense_verified = true\n\
             accepts_hotwords = false\n{extra}\n[[model.files]]\n\
             url = \"http://127.0.0.1:{port}/{id}.onnx\"\nsha256 = \"{}\"\nbytes = {}\n\
             archive = false\n\n",
            broscribe_core::schema::sha256_hex(BODY),
            BODY.len()
        ));
    }
    std::fs::write(
        data.join(broscribe_models::registry::USER_REGISTRY_FILE),
        registry,
    )
    .unwrap();
    let speaker = |f: &Fixture| {
        Config::load(&f.data_dir())
            .unwrap()
            .profile
            .speaker_embedding
    };

    // Without the download the new embedder cannot load: nothing is saved.
    let (code, out, err) = f.run(&[
        "--json",
        "setup",
        "--non-interactive",
        "--profile",
        "default",
    ]);
    assert_ne!(code, 0, "{out}{err}");
    assert!(out.contains("model_missing"), "{out}{err}");
    assert_eq!(speaker(&f), "nemo-titanet-small");

    let v = f.json(&[
        "setup",
        "--non-interactive",
        "--profile",
        "default",
        "--pull-models",
    ]);
    let pulled = v["models"].as_array().unwrap();
    assert_eq!(pulled.len(), 4, "{v}");
    assert!(pulled.iter().all(|m| m["outcome"] == "installed"), "{v}");
    // The ASR model is the live model too: listed once, with both roles.
    assert_eq!(pulled[1]["id"], profile.asr.as_str(), "{v}");
    assert_eq!(
        pulled[1]["roles"],
        serde_json::json!(["asr", "live"]),
        "{v}"
    );
    assert_eq!(speaker(&f), profile.speaker_embedding);

    // PL02: a profile whose ASR model has no live role installs the live
    // model it declares, never another one already installed.
    let v = f.json(&[
        "setup",
        "--non-interactive",
        "--profile",
        "multilingual",
        "--pull-models",
    ]);
    let pulled: Vec<(&str, &str)> = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["outcome"].as_str().unwrap()))
        .collect();
    assert_eq!(
        pulled,
        [
            (profile.vad.as_str(), "already_installed"),
            ("whisper-large-v3-turbo", "installed"),
            (profile.asr.as_str(), "already_installed"),
            (profile.diarization.as_str(), "already_installed"),
            (profile.speaker_embedding.as_str(), "already_installed"),
        ],
        "{v}"
    );
    assert_eq!(v["models"][2]["roles"], serde_json::json!(["live"]), "{v}");
}

#[test]
fn model_removal_protects_configured_roles_and_in_progress_downloads() {
    use broscribe_core::config::Config;
    use broscribe_models::{ModelStore, Registry};

    let f = Fixture::new();
    let data = f.data_dir();
    let store = ModelStore::new(&data);
    let whisper = "whisper-small";
    let unused = "whisper-base";
    Config::update(&data, |config| {
        config.profile.asr = whisper.into();
        Ok(())
    })
    .unwrap();

    let registry = Registry::builtin().unwrap();
    let protect = [
        "parakeet-tdt-0.6b-v3",
        whisper,
        "silero-vad",
        "nemo-titanet-large",
        "potion-multilingual-128m",
        "pyannote-segmentation-3.0",
    ];
    for id in protect.into_iter().chain([unused]) {
        let dir = store.model_dir(id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let entry = registry.require(id).unwrap();
        let name = entry.files[0].file_name();
        if id == unused {
            // An unfinished download: its file sits in staging with its record.
            let partial = dir.join(".partial");
            std::fs::create_dir_all(&partial).unwrap();
            std::fs::write(partial.join(name), b"cached").unwrap();
            broscribe_platform::profile_cleanup::ModelPartialEntries {
                id: id.into(),
                files: vec![name.into()],
                ..Default::default()
            }
            .save(&partial)
            .unwrap();
            continue;
        }
        std::fs::write(dir.join(name), b"cached").unwrap();
        store
            .mark_verified(&broscribe_models::VerifiedMarker {
                id: id.into(),
                sources: Vec::new(),
                files: vec![broscribe_models::store::InstalledFile {
                    path: name.into(),
                    bytes: 6,
                    sha256: String::new(),
                }],
            })
            .unwrap();
    }
    let fail = |args: &[&str], code: &str| {
        let (status, out, err) = f.run(args);
        assert_ne!(status, 0, "{args:?}: {out}{err}");
        assert!(format!("{out}{err}").contains(code), "{args:?}: {out}{err}");
    };
    for id in protect {
        fail(&["--json", "models", "remove", id, "--yes"], "model_in_use");
        assert!(store.has_model_files(registry.require(id).unwrap()));
    }
    fail(&["models", "remove", unused], "--yes");
    let entry = registry.require(unused).unwrap();
    let lock = store.lock_for_removal(entry).unwrap().unwrap();
    fail(
        &["--json", "models", "remove", unused, "--yes"],
        "model_busy",
    );
    drop(lock);
    assert!(store.has_model_files(registry.require(unused).unwrap()));
    let models = f.json(&["models", "list"]);
    let row = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == unused)
        .unwrap();
    assert_eq!(row["has_model_files"], true);
    assert_eq!(row["state"], "missing");
    let revision = Config::load(&data).unwrap().revision();
    let unrelated = store.model_dir(unused).unwrap().join("my-notes.txt");
    std::fs::write(&unrelated, b"keep").unwrap();
    assert_eq!(
        f.json(&["models", "remove", "--yes", "--", unused])["removed"],
        true
    );
    assert_eq!(
        f.json(&["models", "remove", unused, "--yes"])["removed"],
        false
    );
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");
    assert!(!store.has_model_files(registry.require(unused).unwrap()));
    fail(
        &[
            "--json",
            "models",
            "use",
            "asr",
            unused,
            "--require-installed",
        ],
        "model_missing",
    );
    fail(
        &[
            "--json",
            "models",
            "use",
            "text_embedding",
            "potion-base-8m",
            "--require-installed",
        ],
        "model_missing",
    );
    let config = Config::load(&data).unwrap();
    assert_eq!(config.revision(), revision);
    assert_eq!(config.profile.asr, whisper);
    assert_eq!(config.profile.text_embedding, "potion-multilingual-128m");

    // A current registry marker authorises a switch through Settings' strict
    // path, but a damaged payload must leave the active model alone.
    let file = &entry.files[0];
    let target = store.model_dir(unused).unwrap().join(file.file_name());
    std::fs::write(&target, b"ready").unwrap();
    store
        .mark_verified(&broscribe_models::VerifiedMarker {
            id: unused.into(),
            sources: entry
                .files
                .iter()
                .map(|file| broscribe_models::store::SourceHash {
                    file: file.file_name().into(),
                    sha256: file.sha256.clone(),
                })
                .collect(),
            files: vec![broscribe_models::store::InstalledFile {
                path: file.file_name().into(),
                bytes: 5,
                sha256: String::new(),
            }],
        })
        .unwrap();
    assert_eq!(
        f.json(&["models", "use", "asr", unused, "--require-installed"])["installed"],
        true
    );
    std::fs::write(&target, b"damaged payload").unwrap();
    fail(
        &[
            "--json",
            "models",
            "use",
            "asr",
            unused,
            "--require-installed",
        ],
        "model_missing",
    );

    let fresh = Fixture::new();
    let fresh_store = ModelStore::new(&fresh.data_dir());
    assert_eq!(
        fresh.json(&["models", "remove", unused, "--yes"])["removed"],
        false
    );
    assert!(!fresh_store.root().exists());
}

/// Pauses in `load_embedder` until released, to interleave another
/// process's config write while `models use` loads the new model.
struct PausedEmbedderProvider {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl broscribe_models::ModelProvider for PausedEmbedderProvider {
    fn load(
        &self,
        profile: &broscribe_core::config::Profile,
    ) -> broscribe_core::Result<broscribe_models::ModelSet> {
        broscribe_models::ModelProvider::load(&FakeModelProvider::default(), profile)
    }
    fn load_vad(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::Vad + Send>> {
        broscribe_models::ModelProvider::load_vad(&FakeModelProvider::default(), model)
    }
    fn load_embedder(
        &self,
        model: &str,
    ) -> broscribe_core::Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        broscribe_models::ModelProvider::load_embedder(&FakeModelProvider::default(), model)
    }
}

#[test]
fn models_use_keeps_config_written_by_another_process_meanwhile() {
    let f = Fixture::new();
    let data = f.data_dir();
    Config::default().save(&data).unwrap();
    let library = f.lib();
    let root = f.tmp.path().join("root");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let child = std::thread::spawn(move || {
        let mut child = Fixture::new();
        child.deps.paths = Box::new(FakePaths::new(root));
        child.deps.models = Box::new(PausedEmbedderProvider {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
        });
        let args = [
            "broscribe",
            "--json",
            "--library",
            library.to_str().unwrap(),
            "models",
            "use",
            "speaker_embedding",
            "nemo-titanet-small",
            "--allow-unverified-license",
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            args.iter().map(OsString::from).collect(),
            &child.deps,
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap() + &String::from_utf8_lossy(&err),
        )
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // Another process (the app) stores settings while the model loads.
    let mut c = Config::load(&data).unwrap();
    c.integration
        .insert("synthetic".into(), toml::Value::Boolean(true));
    c.profile.asr = "synthetic-asr".into();
    c.save(&data).unwrap();
    release_tx.send(()).unwrap();
    let (code, out) = child.join().unwrap();
    assert_eq!(code, 0, "{out}");
    let c = Config::load(&data).unwrap();
    assert_eq!(c.profile.speaker_embedding, "nemo-titanet-small");
    assert_eq!(c.profile.asr, "synthetic-asr");
    assert!(c.integration.contains_key("synthetic"), "{c:?}");
}

#[test]
fn direct_import_keeps_its_recording_when_the_policy_switches_meanwhile() {
    let f = Fixture::new();
    let data = f.data_dir();
    let mut config = Config::default();
    config.retention = broscribe_core::config::Retention::DeleteImmediately {
        wait_until_named: false,
    };
    config.save(&data).unwrap();
    let state = |f: &Fixture, id: &Value| {
        let id = MeetingId::parse(id.as_str().unwrap()).unwrap();
        f.library()
            .load_meeting(&id)
            .unwrap()
            .recording
            .unwrap()
            .state
    };
    // Control: under delete_immediately a direct import deletes its copy.
    let wav = f.passage_wav("deleted.wav", 300.0);
    let v = f.json(&["import", wav.to_str().unwrap()]);
    assert_eq!(
        state(&f, &v["meeting_id"]),
        broscribe_core::schema::RecordingState::Deleted
    );

    let wav = f.passage_wav("kept.wav", 440.0);
    let library = f.lib();
    let root = f.tmp.path().join("root");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let child = std::thread::spawn(move || {
        let mut child = Fixture::new();
        child.deps.paths = Box::new(FakePaths::new(root));
        child.deps.models = Box::new(PausedProvider {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
        });
        let args = [
            "broscribe",
            "--json",
            "--library",
            library.to_str().unwrap(),
            "import",
            wav.to_str().unwrap(),
        ];
        let mut out = Vec::new();
        let code = run(
            args.iter().map(OsString::from).collect(),
            &child.deps,
            &mut out,
            &mut Vec::new(),
        );
        (code, String::from_utf8(out).unwrap())
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // The user switches to keep forever while the import runs.
    let mut c = Config::load(&data).unwrap();
    c.retention = broscribe_core::config::Retention::KeepForever;
    c.save(&data).unwrap();
    release_tx.send(()).unwrap();
    let (code, out) = child.join().unwrap();
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_ne!(
        state(&f, &v["meeting_id"]),
        broscribe_core::schema::RecordingState::Deleted
    );
}

/// Under `auto_enrich = new` with a consented synthetic provider, a `move`
/// import that published its meeting, its job and journal put back as a
/// crash right after the source was removed leaves them. That import's
/// result.
fn interrupted_move_import(f: &Fixture) -> Value {
    use broscribe_core::config::{AutoEnrich, ProviderConfig};
    use broscribe_core::queue::IMPORT_KIND;
    let dir = f.data_dir();
    let mut c = Config::load(&dir).unwrap();
    c.auto_enrich = AutoEnrich::New;
    c.providers.insert(
        "openai-http".into(),
        ProviderConfig {
            enabled: true,
            consented_at: Some(broscribe_core::schema::now()),
            model: Some("synthetic-model".into()),
            base_url: Some("http://127.0.0.1:9/v1".into()),
        },
    );
    c.save(&dir).unwrap();
    let moved = f.tmp.path().join("in").join("moved.wav");
    let mut audio = conversation();
    audio.extend(silence(1.0));
    write_wav(&moved, &audio);
    let v = f.json(&["import", moved.to_str().unwrap(), "--handling", "move"]);
    let lib = f.library();
    let recording = lib
        .load_meeting(&MeetingId::parse(v["meeting_id"].as_str().unwrap()).unwrap())
        .unwrap()
        .recording
        .unwrap();
    let source = recording.original_path.clone().unwrap();
    let queue = Queue::open(lib.root()).unwrap();
    let job = lib
        .write(|tx| {
            queue.enqueue(
                tx,
                IMPORT_KIND,
                &recording.sha256,
                serde_json::json!({
                    "path": source,
                    "sha256": recording.sha256,
                    "handling": "move",
                    "folder": false,
                }),
            )
        })
        .unwrap();
    std::fs::write(
        queue.job_dir(&job).join("move.json"),
        serde_json::json!({
            "source": source,
            "staged": "staged.wav",
            "sha256": recording.sha256,
            "bytes": 0,
            "renamed": true,
            "state": "source_removed",
        })
        .to_string(),
    )
    .unwrap();
    v
}

#[test]
fn an_unrelated_import_gives_a_completed_interrupted_move_its_follow_ups() {
    use broscribe_core::queue::IMPORT_KIND;
    use broscribe_pipeline::enrich::ENRICH_KIND;
    let f = Fixture::new();
    let moved_id = interrupted_move_import(&f)["meeting_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let other = f.wav("other.wav");
    let v = f.json(&["import", other.to_str().unwrap()]);
    assert_eq!(v["already_exists"], false, "{v}");
    assert_ne!(v["meeting_id"], moved_id.as_str(), "{v}");
    assert_eq!(
        v["completed_moves"][0]["meeting_id"],
        moved_id.as_str(),
        "{v}"
    );
    // Both meetings get enrichment under `auto_enrich = new`.
    assert_eq!(
        v["enrichment"]["queued"].as_array().unwrap().len(),
        2,
        "{v}"
    );
    assert_eq!(v["enrichment"]["failed"], serde_json::json!([]), "{v}");
    assert_eq!(v["enrichment"]["errors"], serde_json::json!([]), "{v}");
    let queue = Queue::open(f.lib()).unwrap();
    assert!(queue.list(IMPORT_KIND).unwrap().is_empty());
    assert_eq!(queue.list(ENRICH_KIND).unwrap().len(), 2);
}

/// The completed move's enrichment is refused (its job record is
/// unreadable) while the unrelated import's is queued: the result reports
/// both, and neither hides the other.
#[test]
fn a_completed_interrupted_move_reports_its_refused_enrichment_beside_queued_work() {
    use broscribe_core::queue::{ENRICH_KIND, IMPORT_KIND, JOB_JSON, JobId};
    let f = Fixture::new();
    let first = interrupted_move_import(&f);
    let moved_id = first["meeting_id"].clone();
    let moved_job = JobId::parse(first["enrichment"]["queued"][0].as_str().unwrap()).unwrap();
    let record = Queue::open(f.lib())
        .unwrap()
        .job_dir(&moved_job)
        .join(JOB_JSON);
    std::fs::write(&record, "{ not a job record").unwrap();

    let other = f.wav("other.wav");
    let v = f.json(&["import", other.to_str().unwrap()]);
    assert_eq!(v["completed_moves"][0]["meeting_id"], moved_id, "{v}");
    let failed = v["enrichment"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{v}");
    assert_eq!(failed[0]["meeting_id"], moved_id, "{v}");
    assert_eq!(failed[0]["code"], "schema", "{v}");
    assert_eq!(v["enrichment"]["errors"], serde_json::json!([]), "{v}");
    let queued = v["enrichment"]["queued"].as_array().unwrap();
    assert_eq!(queued.len(), 1, "{v}");
    // The unrelated meeting's job is durable; the refused record is untouched.
    let queue = Queue::open(f.lib()).unwrap();
    let job = queue
        .get(&JobId::parse(queued[0].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(job.kind, ENRICH_KIND);
    assert_eq!(job.key, v["meeting_id"].as_str().unwrap(), "{v}");
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        "{ not a job record"
    );
    assert!(queue.list(IMPORT_KIND).unwrap().is_empty());
}

/// Runs `--json <args…>` expecting a command error: the exit code and the
/// parsed error document (`Null` for usage errors, which print to stderr).
fn json_fail(f: &Fixture, args: &[&str]) -> (i32, Value) {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let (code, out, _) = f.run(&all);
    (code, serde_json::from_str(&out).unwrap_or(Value::Null))
}

/// One imported (done) synthetic meeting; its ID.
fn imported_meeting(f: &Fixture, name: &str) -> String {
    let wav = f.wav(name);
    f.json(&["import", wav.to_str().unwrap()])["meeting_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn enrich_queue_returns_its_job_and_one_request_at_a_time() {
    use broscribe_core::queue::JobId;
    use broscribe_pipeline::enrich::ENRICH_KIND;

    let f = Fixture::new();
    let id = imported_meeting(&f, "queue-me.wav");
    let queue = Queue::open(f.lib()).unwrap();

    // Without consent nothing is queued.
    let (code, v) = json_fail(&f, &["enrich", &id, "--queue", "--provider", "claude-cli"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "invalid_input", "{v}");
    assert!(queue.list(ENRICH_KIND).unwrap().is_empty());

    f.json(&["providers", "enable", "claude-cli", "--accept"]);
    // Model names are checked before anything is queued.
    for model in [&["--model", "two words"][..], &["--model=-x"][..]] {
        let mut args = vec!["enrich", id.as_str(), "--queue", "--provider", "claude-cli"];
        args.extend_from_slice(model);
        let (code, v) = json_fail(&f, &args);
        assert_eq!(code, 1, "{model:?}: {v}");
        assert_eq!(v["error"]["code"], "invalid_input", "{model:?}: {v}");
    }
    assert!(queue.list(ENRICH_KIND).unwrap().is_empty());
    // --queue takes one meeting.
    let (code, _) = json_fail(&f, &["enrich", "--all", "--queue"]);
    assert_eq!(code, 2);
    // A model is chosen only together with its provider.
    let (code, _) = json_fail(&f, &["enrich", &id, "--queue", "--model", "chosen-1"]);
    assert_eq!(code, 2);

    let v = f.json(&[
        "enrich",
        &id,
        "--queue",
        "--provider",
        "claude-cli",
        "--model",
        "chosen-1",
    ]);
    assert_eq!(v["queued"], true, "{v}");
    let job_id = v["job_id"].as_str().unwrap();
    // The baseline is the enrichment saved when the job was queued (none
    // yet): the app waits for a newer one than that.
    let saved = f
        .library()
        .load_meeting(&MeetingId::parse(&id).unwrap())
        .unwrap()
        .enrichment
        .map(|e| e.created_at);
    assert_eq!(saved, None);
    assert_eq!(v.get("baseline"), Some(&serde_json::json!(saved)), "{v}");
    let job = queue.get(&JobId::parse(job_id).unwrap()).unwrap();
    assert_eq!(job.kind, ENRICH_KIND);
    assert_eq!(job.input["meeting_id"], id.as_str());
    assert_eq!(job.input["provider"], "claude-cli");
    assert_eq!(job.input["model"], "chosen-1");
    assert!(
        job.input
            .get("interactive")
            .is_none_or(|v| v == &Value::Bool(false)),
        "{}",
        job.input
    );

    // A second request while it waits is refused, and so is a line
    // correction (the enrichment reads the transcript).
    let (code, v) = json_fail(&f, &["enrich", &id, "--queue"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "library_busy", "{v}");
    let (code, v) = json_fail(
        &f,
        &[
            "transcript",
            "attribute",
            &id,
            "--segment",
            "0",
            "--person",
            "Ida Invented",
        ],
    );
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "library_busy", "{v}");
    assert_eq!(queue.list(ENRICH_KIND).unwrap().len(), 1);
}

/// CL03: `reprocess` runs through the meeting's leased `reprocess` job (a
/// finished run leaves no job behind), and a reprocessing already queued
/// for the meeting refuses the synchronous one (`library_busy`) instead of
/// running both or replacing the queued request.
#[test]
fn reprocess_runs_leased_and_excludes_a_queued_reprocessing() {
    use broscribe_core::queue::JobState;
    use broscribe_core::queue::REPROCESS_KIND;

    let f = Fixture::new();
    let id = imported_meeting(&f, "reprocess.wav");
    let v = f.json(&["reprocess", &id]);
    assert!(v.get("queued").is_none(), "{v}");
    assert_eq!(v["meeting_id"], id.as_str(), "{v}");
    let queue = Queue::open(f.lib()).unwrap();
    assert!(queue.list(REPROCESS_KIND).unwrap().is_empty());

    // A queued reprocessing request for the worker.
    let lib = f.library();
    lib.write(|tx| {
        queue.enqueue(
            tx,
            REPROCESS_KIND,
            &id,
            serde_json::json!({ "meeting_id": id, "asr_only": true }),
        )
    })
    .unwrap();
    drop(lib);
    let (code, v) = json_fail(&f, &["reprocess", &id]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "library_busy", "{v}");
    let jobs = queue.list(REPROCESS_KIND).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Queued, "{:?}", jobs[0]);
    // The queued request is left as it was, never replaced by this one.
    assert_eq!(jobs[0].input["asr_only"], true, "{}", jobs[0].input);
}

#[test]
fn templates_run_queue_holds_one_job_per_meeting_and_template() {
    use broscribe_core::queue::{JobId, TEMPLATE_KIND};

    let f = Fixture::new();
    let id = imported_meeting(&f, "templates.wav");
    let queue = Queue::open(f.lib()).unwrap();
    let run = |template: &str| {
        json_fail(
            &f,
            &[
                "templates",
                "run",
                &id,
                template,
                "--queue",
                "--provider",
                "claude-cli",
            ],
        )
    };

    // Without consent nothing is queued.
    let (code, v) = run("bant");
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "invalid_input", "{v}");
    assert!(queue.list(TEMPLATE_KIND).unwrap().is_empty());

    f.json(&["providers", "enable", "claude-cli", "--accept"]);
    let (code, v) = run("no-such-template");
    assert_eq!(
        (code, &v["error"]["code"]),
        (1, &Value::from("not_found")),
        "{v}"
    );
    let (code, _) = json_fail(&f, &["templates", "run", &id, "bant", "--model", "m-1"]);
    assert_eq!(code, 2, "a model needs its provider");

    let v = f.json(&[
        "templates",
        "run",
        &id,
        "bant",
        "--queue",
        "--provider",
        "claude-cli",
        "--model",
        "chosen-1",
    ]);
    assert_eq!(v["queued"], true, "{v}");
    let job = queue
        .get(&JobId::parse(v["job_id"].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(job.kind, TEMPLATE_KIND);
    assert_eq!(job.input["meeting_id"], id.as_str());
    assert_eq!(job.input["template_id"], "bant");
    assert_eq!(job.input["provider"], "claude-cli");
    assert_eq!(job.input["model"], "chosen-1");

    // One reservation per (meeting, template): the same template is busy,
    // another one queues beside it.
    let (code, v) = run("bant");
    assert_eq!(
        (code, &v["error"]["code"]),
        (1, &Value::from("library_busy")),
        "{v}"
    );
    let other = f.json(&[
        "templates",
        "run",
        &id,
        "meeting-notes",
        "--queue",
        "--provider",
        "claude-cli",
    ]);
    assert_eq!(other["queued"], true, "{other}");
    assert_eq!(queue.list(TEMPLATE_KIND).unwrap().len(), 2);

    // `queue status` names jobs by what they do.
    let status = f.json(&["queue", "status"]);
    let labels: Vec<&str> = status["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["kind"] == TEMPLATE_KIND)
        .map(|j| j["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels.len(), 2, "{status}");
    assert!(
        labels.iter().all(|l| l.starts_with("Running ")),
        "{labels:?}"
    );
    // Their place in line: the later one waits behind the earlier one.
    let ahead: Vec<u64> = status["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["kind"] == TEMPLATE_KIND)
        .map(|j| j["jobs_ahead"].as_u64().unwrap())
        .collect();
    assert_eq!(ahead[1], ahead[0] + 1, "{status}");
    let (code, human, _) = f.run(&["queue", "status"]);
    assert_eq!(code, 0);
    assert!(human.contains("Running "), "{human}");
    assert!(!human.contains(&format!("  {TEMPLATE_KIND} ")), "{human}");

    // No result yet.
    let (code, v) = json_fail(&f, &["templates", "result", &id, "bant"]);
    assert_eq!(
        (code, &v["error"]["code"]),
        (1, &Value::from("not_found")),
        "{v}"
    );
}

#[test]
fn transcript_attribute_names_lines_and_refuses_stale_or_bad_input() {
    let f = Fixture::new();
    let id = imported_meeting(&f, "lines.wav");
    let t = f.json(&["transcript", &id]);
    let segs = t["segments"].as_array().unwrap();
    assert!(!segs.is_empty(), "{t}");
    for s in segs {
        assert_eq!(s["attribution"], "speaker", "{s}");
        assert!(s.get("person_id").is_some() && s.get("person_name").is_some());
    }
    let first = segs[0]["id"].as_u64().unwrap().to_string();
    let speaker = segs[0]["speaker"].clone();

    let v = f.json(&[
        "transcript",
        "attribute",
        &id,
        "--segment",
        &first,
        "--segment",
        &first,
        "--person",
        "Ida Invented",
    ]);
    let person_id = v["person_id"].as_str().unwrap().to_owned();
    assert_eq!(
        v,
        serde_json::json!({
            "meeting_id": id,
            "segments": [first.parse::<u32>().unwrap()],
            "person_id": person_id,
        })
    );
    let t = f.json(&["transcript", &id]);
    let line = &t["segments"][0];
    assert_eq!(line["attribution"], "line");
    assert_eq!(line["person_id"], person_id.as_str());
    assert_eq!(line["person_name"], "Ida Invented");
    assert_eq!(line["speaker"], speaker, "the speaker is unchanged");
    let (code, text, err) = f.run(&["transcript", &id]);
    assert_eq!(code, 0, "{err}");
    assert!(
        text.lines().next().unwrap().contains("] Ida Invented: "),
        "{text}"
    );
    // The person filter finds the corrected line.
    let only = f.json(&["transcript", &id, "--speaker", "Ida Invented"]);
    assert_eq!(only["segments"].as_array().unwrap().len(), 1);

    // Refusals: unknown segment, unknown e-mail, missing or conflicting
    // choices (usage errors), nothing changed by any of them.
    let (code, v) = json_fail(
        &f,
        &[
            "transcript",
            "attribute",
            &id,
            "--segment",
            "9999",
            "--clear",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "invalid_input", "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("reload"));
    let (code, v) = json_fail(
        &f,
        &[
            "transcript",
            "attribute",
            &id,
            "--segment",
            &first,
            "--person",
            "nobody@example.invalid",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "not_found", "{v}");
    for args in [
        &[
            "transcript",
            "attribute",
            id.as_str(),
            "--segment",
            first.as_str(),
        ][..],
        &[
            "transcript",
            "attribute",
            id.as_str(),
            "--segment",
            first.as_str(),
            "--person",
            "X",
            "--clear",
        ][..],
        &["transcript", "attribute", id.as_str(), "--clear"][..],
    ] {
        let (code, _) = json_fail(&f, args);
        assert_eq!(code, 2, "{args:?}");
    }
    assert_eq!(
        f.json(&["transcript", &id])["segments"][0]["person_id"],
        person_id.as_str()
    );

    let v = f.json(&[
        "transcript",
        "attribute",
        &id,
        "--segment",
        &first,
        "--clear",
    ]);
    assert_eq!(v["person_id"], Value::Null, "{v}");
    assert_eq!(
        f.json(&["transcript", &id])["segments"][0]["attribution"],
        "speaker"
    );
}

#[cfg(unix)]
#[test]
fn providers_models_lists_only_with_consent() {
    use std::os::unix::fs::PermissionsExt;

    let mut f = Fixture::new();
    let bin = f.tmp.path().join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = f.tmp.path().join("calls");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nprintf '* g-2 (default)\\n- g-1\\n'\n",
        calls.display()
    );
    let grok = bin.join("grok");
    std::fs::write(&grok, script).unwrap();
    std::fs::set_permissions(&grok, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.deps.agents.search = broscribe_models::llm::SearchPaths::new(vec![bin]);

    let (code, v) = json_fail(&f, &["providers", "models", "grok-cli"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"]["code"], "consent_required", "{v}");
    assert!(!calls.exists(), "nothing ran without consent");

    f.json(&["providers", "enable", "grok-cli", "--accept"]);
    let v = f.json(&["providers", "models", "grok-cli"]);
    assert_eq!(
        v,
        serde_json::json!({
            "provider": "grok-cli",
            "source": "cli",
            "configured_model": null,
            "models": [
                {"id": "g-2", "name": null, "default": true},
                {"id": "g-1", "name": null, "default": false},
            ],
            "default_resolved": null,
        })
    );
    assert_eq!(std::fs::read_to_string(&calls).unwrap(), "models\n");
    let (code, text, err) = f.run(&["providers", "models", "grok-cli"]);
    assert_eq!(code, 0, "{err}");
    assert!(text.contains("g-2  (default)"), "{text}");

    // No listing for this provider: says so and runs nothing.
    f.json(&["providers", "enable", "gemini-cli", "--accept"]);
    let v = f.json(&["providers", "models", "gemini-cli"]);
    assert_eq!(v["source"], "none");
    assert_eq!(v["models"], serde_json::json!([]));

    let (code, v) = json_fail(&f, &["providers", "models", "openai-embeddings"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "invalid_input", "{v}");
    let (code, v) = json_fail(&f, &["providers", "models", "no-such-provider"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "not_found", "{v}");
}

/// A meeting whose enrichment came from `provider`, written through core
/// and indexed.
#[cfg(unix)]
fn seed_enrichment(
    lib: &Library,
    at: &str,
    provider: &str,
    requested: Option<&str>,
    model: &str,
    reported: bool,
) {
    use broscribe_core::index::Index;
    use broscribe_core::schema::{Meeting, Source};

    let started = chrono::DateTime::parse_from_rfc3339(at).unwrap();
    let mut m = Meeting::new(lib.new_meeting_id().unwrap(), Source::Import, started);
    m.enrichment = Some(
        serde_json::from_value(serde_json::json!({
            "provider": provider,
            "model": model,
            "requested_model": requested,
            "model_reported": reported,
            "created_at": at,
            "summary": "Synthetic summary.",
        }))
        .unwrap(),
    );
    lib.write(|tx| {
        tx.create_meeting_dir(&m)?;
        tx.save_meeting(&m)?;
        Index::open(tx.root())?.upsert_meeting(tx, &m)
    })
    .unwrap();
}

#[cfg(unix)]
#[test]
fn providers_models_shows_what_requests_last_resolved_to() {
    use std::os::unix::fs::PermissionsExt;

    let mut f = Fixture::new();
    let bin = f.tmp.path().join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let grok = bin.join("grok");
    std::fs::write(&grok, "#!/bin/sh\nprintf '* g-2 (default)\\n- g-1\\n'\n").unwrap();
    std::fs::set_permissions(&grok, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.deps.agents.search = broscribe_models::llm::SearchPaths::new(vec![bin]);
    f.json(&["providers", "enable", "grok-cli", "--accept"]);

    // No library yet: no hints, no error.
    let v = f.json(&["providers", "models", "grok-cli"]);
    assert_eq!(v["default_resolved"], Value::Null, "{v}");
    assert!(v["models"][0].get("resolved").is_none(), "{v}");

    let lib = Library::open(f.lib()).unwrap();
    let seed = |at, provider, requested, model, reported| {
        seed_enrichment(&lib, at, provider, requested, model, reported)
    };
    seed(
        "2026-11-10T10:00:00-03:00",
        "grok-cli",
        None,
        "g-2-old",
        true,
    );
    seed(
        "2026-11-12T10:00:00-03:00",
        "grok-cli",
        None,
        "g-2-build",
        true,
    );
    seed(
        "2026-11-11T10:00:00-03:00",
        "grok-cli",
        Some("g-1"),
        "g-1-0915",
        true,
    );
    // Requested only (nothing reported), and another provider: no hints.
    seed(
        "2026-11-13T10:00:00-03:00",
        "grok-cli",
        Some("g-2"),
        "g-2",
        false,
    );
    seed(
        "2026-11-14T10:00:00-03:00",
        "claude-cli",
        None,
        "other-m",
        true,
    );

    let v = f.json(&["providers", "models", "grok-cli"]);
    assert_eq!(v["default_resolved"], "g-2-build", "{v}");
    assert_eq!(
        v["models"],
        serde_json::json!([
            {"id": "g-2", "name": null, "default": true},
            {"id": "g-1", "name": null, "default": false, "resolved": "g-1-0915"},
        ])
    );
    let (code, text, err) = f.run(&["providers", "models", "grok-cli"]);
    assert_eq!(code, 0, "{err}");
    assert!(text.contains("g-1  (last run: g-1-0915)"), "{text}");
    assert!(
        text.contains("Provider default last ran as g-2-build"),
        "{text}"
    );
}

#[test]
fn providers_models_asks_an_http_server_with_the_stored_key() {
    use std::io::{BufRead, BufReader, Write};

    let f = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            head.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let body = r#"{"object":"list","data":[{"id":"local-a"},{"id":"local-b"}]}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).unwrap();
        head
    });
    let address = format!("http://127.0.0.1:{port}/v1");
    f.json(&[
        "providers",
        "configure",
        "openai-http",
        "--base-url",
        &address,
        "--model",
        "local-a",
    ]);
    f.json(&["providers", "enable", "openai-http", "--accept"]);
    f.deps
        .secrets
        .set("provider/openai-http/api_key", "sk-synthetic-models")
        .unwrap();

    let (code, out, err) = f.run(&["--json", "providers", "models", "openai-http"]);
    assert_eq!(code, 0, "{out}{err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["source"], "api");
    assert_eq!(v["configured_model"], "local-a");
    assert_eq!(v["models"][1]["id"], "local-b");
    assert!(!out.contains("sk-synthetic") && !err.contains("sk-synthetic"));
    let request = server.join().unwrap();
    assert!(request.starts_with("GET /v1/models "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-synthetic-models")
    );
}

/// `providers test` names the model that answered only when the provider
/// reported one or was given one: an agent CLI whose help lacks the model
/// flag runs its own default, whatever model is configured, and the reply
/// names none (SET-63).
#[cfg(unix)]
#[test]
fn providers_test_names_only_the_model_that_ran() {
    use std::os::unix::fs::PermissionsExt;

    let mut f = Fixture::new();
    let bin = f.tmp.path().join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let reply = r#"{"modelUsage":{"synthetic-reported-1":{"outputTokens":3}},"structured_output":{"summary":"S1 and S2 move the picnic to Saturday.","topics":[],"keywords":[],"tags":[],"decisions":[],"actions":[]}}"#;
    let flags = "  -p, --print\n  --json-schema <s>\n  --tools <t>";
    for (name, help) in [
        ("flagless", flags.to_owned()),
        ("flagged", format!("{flags}\n  --model <m>")),
    ] {
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in --version) echo '{name} 1.0.0'; exit 0;; esac\n\
             for a in \"$@\"; do case \"$a\" in --help) printf '%s\\n' '{help}'; exit 0;; esac; done\n\
             printf '%s\\n' \"$@\" > '{argv}'\n\
             cat > /dev/null\n\
             printf '%s\\n' '{reply}'\n",
            argv = f.tmp.path().join(format!("{name}.argv")).display(),
        );
        let path = bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.deps.agents.search =
        broscribe_models::llm::SearchPaths::new(vec![bin, "/usr/bin".into(), "/bin".into()]);
    let entry = |id: &str, binary: &str, extra: &str| {
        format!(
            "[[provider]]\nid = \"{id}\"\nname = \"Synthetic {id}\"\nvendor = \"Nobody\"\n\
             runtime = \"agent-cli\"\nbinary = \"{binary}\"\nprompt_transport = \"stdin\"\n\
             output_keys = [\"structured_output\"]\nmodel_args = [\"--model\", \"{{model}}\"]\n\
             {extra}args = [\n\
               {{ flag = \"-p\", need = \"required\" }},\n\
               {{ flag = \"--json-schema\", values = [\"{{schema}}\"], need = \"required\" }},\n\
               {{ flag = \"--tools\", values = [\"\"], need = \"safety\" }},\n\
             ]\n\n"
        )
    };
    let reports = "reported_model = { json_object = \"modelUsage\", rank_by = \"outputTokens\" }\n";
    let data = f.data_dir();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join(broscribe_models::registry::USER_REGISTRY_FILE),
        [
            entry("synthetic-flagless", "flagless", ""),
            entry("synthetic-flagged", "flagged", ""),
            entry("synthetic-reporting", "flagged", reports),
        ]
        .concat(),
    )
    .unwrap();
    let ids = [
        "synthetic-flagless",
        "synthetic-flagged",
        "synthetic-reporting",
    ];
    for id in ids {
        f.json(&["providers", "enable", id, "--accept"]);
    }
    let mut config = Config::load(&data).unwrap();
    for id in ids {
        config.providers.get_mut(id).unwrap().model = Some("synthetic-configured".into());
    }
    config.save(&data).unwrap();
    let argv = |name: &str| std::fs::read_to_string(f.tmp.path().join(format!("{name}.argv")));
    let model_of = |v: &Value| {
        (
            v["model"].clone(),
            v["requested_model"].clone(),
            v["model_reported"].clone(),
        )
    };

    // The help lacks --model and the entry reads no report: the CLI runs
    // its own default and the reply names no model, though one is
    // configured.
    let v = f.json(&["providers", "test", "synthetic-flagless"]);
    assert_eq!(v["ok"], true, "{v}");
    let flagless = argv("flagless").unwrap();
    assert!(
        !flagless.contains("--model") && !flagless.contains("synthetic-configured"),
        "{flagless}"
    );
    assert_eq!(
        model_of(&v),
        (Value::Null, Value::Null, Value::Bool(false)),
        "{v}"
    );
    let (code, text, err) = f.run(&["providers", "test", "synthetic-flagless"]);
    assert_eq!(code, 0, "{err}");
    assert!(text.contains(": ok — a valid reply in "), "{text}");

    // Given --model, with no report read: the requested model.
    let v = f.json(&["providers", "test", "synthetic-flagged"]);
    assert!(
        argv("flagged")
            .unwrap()
            .contains("--model\nsynthetic-configured\n")
    );
    assert_eq!(
        model_of(&v),
        (
            Value::String("synthetic-configured".into()),
            Value::String("synthetic-configured".into()),
            Value::Bool(false)
        ),
        "{v}"
    );

    // Given --model and read the report: the reported model.
    let v = f.json(&["providers", "test", "synthetic-reporting"]);
    assert_eq!(
        model_of(&v),
        (
            Value::String("synthetic-reported-1".into()),
            Value::String("synthetic-configured".into()),
            Value::Bool(true)
        ),
        "{v}"
    );
    let (code, text, err) = f.run(&["providers", "test", "synthetic-reporting"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        text.contains("a valid reply from synthetic-reported-1 in "),
        "{text}"
    );
}

/// A loopback server answering one request with `body`; joining it gives
/// that request (head and body).
fn serve_once(body: String) -> (u16, std::thread::JoinHandle<String>) {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            request.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).unwrap();
        request.push_str(&String::from_utf8(payload).unwrap());
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).unwrap();
        request
    });
    (port, server)
}

/// `providers test` of an HTTP embedding provider names the model the
/// server says answered, as an LLM test does: asked for `embed-latest`, a
/// server answering `embed-v2` gives `model: embed-v2`, `requested_model:
/// embed-latest`, `model_reported: true`; a reply naming no model falls
/// back to the requested one, not reported (SET-63).
#[test]
fn providers_test_of_an_embedding_provider_names_the_model_the_server_reported() {
    let f = Fixture::new();
    let vectors = serde_json::json!([{"index": 0, "embedding": [0.5, 0.25, 0.125]}]);
    let point_at = |port: u16| {
        let dir = f.data_dir();
        let mut c = Config::load(&dir).unwrap();
        let pc = c.providers.entry("openai-embeddings".into()).or_default();
        pc.base_url = Some(format!("http://127.0.0.1:{port}/v1"));
        pc.model = Some("embed-latest".into());
        c.save(&dir).unwrap();
    };
    let model_of = |v: &Value| {
        (
            v["model"].clone(),
            v["requested_model"].clone(),
            v["model_reported"].clone(),
        )
    };

    // The server resolves the alias to a version and says so.
    let (port, server) = serve_once(
        serde_json::json!({"object": "list", "model": "embed-v2", "data": vectors}).to_string(),
    );
    point_at(port);
    f.json(&["providers", "enable", "openai-embeddings", "--accept"]);
    let v = f.json(&["providers", "test", "openai-embeddings"]);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["detail"], "a vector of 3 values", "{v}");
    assert_eq!(
        model_of(&v),
        (
            Value::String("embed-v2".into()),
            Value::String("embed-latest".into()),
            Value::Bool(true)
        ),
        "{v}"
    );
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /v1/embeddings "), "{request}");
    let body: Value = serde_json::from_str(&request[request.find("\r\n\r\n").unwrap()..])
        .unwrap_or_else(|e| panic!("{e}: {request}"));
    assert_eq!(body["model"], "embed-latest", "{request}");

    let (port, server) = serve_once(
        serde_json::json!({"object": "list", "model": "embed-v2", "data": vectors}).to_string(),
    );
    point_at(port);
    let (code, text, err) = f.run(&["providers", "test", "openai-embeddings"]);
    server.join().unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(
        text.contains("a vector of 3 values from embed-v2 in "),
        "{text}"
    );

    // No `model` in the reply: the requested model, not reported.
    let (port, server) = serve_once(serde_json::json!({"data": vectors}).to_string());
    point_at(port);
    let v = f.json(&["providers", "test", "openai-embeddings"]);
    server.join().unwrap();
    assert_eq!(
        model_of(&v),
        (
            Value::String("embed-latest".into()),
            Value::String("embed-latest".into()),
            Value::Bool(false)
        ),
        "{v}"
    );
}

/// "Can't tell" from the terminal: every unresolved speaker dismissed
/// leaves no false "all resolved": the empty state counts the dismissed.
#[test]
fn dismissing_every_speaker_reports_the_dismissed_count() {
    let f = Fixture::new();
    let id = import_one(&f, "a.wav");
    let review = f.json(&["speakers", "review"]);
    let pending: Vec<String> = ["speakers", "without_transcript"]
        .iter()
        .flat_map(|k| review[*k].as_array().unwrap().clone())
        .filter(|r| r["meeting_id"] == id.as_str())
        .map(|r| r["speaker_id"].as_str().unwrap().to_owned())
        .collect();
    assert!(!pending.is_empty(), "{review}");
    for s in &pending {
        let (code, out, err) = f.run(&["speakers", "dismiss", &id, s]);
        assert_eq!(code, 0, "{out}{err}");
        assert!(out.contains("speakers restore"), "{out}");
    }
    let (code, out, _) = f.run(&["speakers", "review"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("Nothing to review · {} dismissed", pending.len())),
        "{out}"
    );
    let review = f.json(&["speakers", "review"]);
    assert_eq!(review["dismissed"], pending.len());
    let (code, out, _) = f.run(&["--json", "speakers", "dismiss", &id, "S9"]);
    assert_eq!(code, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["error"]["code"],
        "not_found"
    );
}

#[test]
fn commands_on_existing_data_never_create_a_missing_library() {
    let fixture = Fixture::new();
    let commands: &[&[&str]] = &[
        &["queue", "work", "--once"],
        &["retention", "keep", "m-000001"],
        &["identity", "re-resolve"],
        &["index", "rebuild"],
        &["speakers", "name", "m-000001", "S1", "p-000001"],
    ];
    for command in commands {
        let mut args = vec!["--json"];
        args.extend_from_slice(command);
        let (code, out, err) = fixture.run(&args);
        assert_eq!(code, 1, "{command:?}: {out}{err}");
        let output: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(output["error"]["code"], "not_found", "{command:?}: {out}");
        assert!(
            !fixture.lib().exists(),
            "{command:?} created an empty library"
        );
    }
}

#[test]
fn parity_recording_checks_restore_only_matching_audio_and_clips_keep_existing_files() {
    let f = Fixture::new();
    let original = f.wav("synthetic-parity-recording.wav");
    let input = original.to_str().unwrap();
    let result = f.json(&["import", input, "--handling", "reference"]);
    let id = result["meeting_id"].as_str().unwrap();
    let backup = std::fs::read(&original).unwrap();
    std::fs::remove_file(&original).unwrap();
    assert_eq!(
        f.json(&["meeting", "check-recording", id])["state"],
        "missing"
    );
    std::fs::write(&original, b"different bytes").unwrap();
    assert_eq!(
        f.json(&["meeting", "check-recording", id])["state"],
        "missing"
    );
    std::fs::write(&original, &backup).unwrap();
    assert_eq!(
        f.json(&["meeting", "check-recording", id])["state"],
        "available"
    );
    let cache = f.json(&["meeting", "prepare-audio", id]);
    assert_eq!(cache["ready"], false);
    let queue = f.json(&["queue", "status"]);
    assert!(
        queue["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|job| job["id"] == cache["job_id"]
                && job["kind"] == "playback-cache"
                && job["meeting_id"] == id)
    );
    let target = f.tmp.path().join("synthetic-cut.wav");
    let args = [
        "meeting",
        "clip",
        id,
        "--from",
        "0",
        "--to",
        "1",
        "--out",
        target.to_str().unwrap(),
    ];
    f.json(&args);
    let saved = std::fs::read(&target).unwrap();
    assert_eq!(&saved[..4], b"RIFF");
    assert_eq!(saved.len(), 44 + 32_000);
    assert_ne!(f.run(&args).0, 0);
    assert_eq!(std::fs::read(&target).unwrap(), saved);
    assert_eq!(std::fs::read(&original).unwrap(), backup);
}

#[test]
fn parity_preview_and_example_use_no_library_writes() {
    let f = Fixture::new();
    let wav = f.wav("synthetic-candidate.wav");
    let unsupported = wav.with_file_name("synthetic-not-audio.txt");
    std::fs::write(&unsupported, b"synthetic").unwrap();
    let folder = wav.parent().unwrap().to_str().unwrap();
    let preview = f.json(&["import", folder, "--dry-run"]);
    assert_eq!(preview["kind"], "folder");
    assert_eq!(preview["count"], 1);
    assert_eq!(preview["skipped"][0], unsupported.to_str().unwrap());
    let manifest = f.tmp.path().join("synthetic-import.json");
    std::fs::write(
        &manifest,
        serde_json::json!({
            "format": "broscribe-import-manifest", "version": 1,
            "meetings": [{"audio_path": wav}]
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        f.json(&[
            "import",
            "--manifest",
            manifest.to_str().unwrap(),
            "--dry-run"
        ])["count"],
        1
    );
    assert!(!f.lib().exists());
    let config = Config::default();
    let model = broscribe_models::ModelStore::new(&f.data_dir())
        .model_dir(&config.profile.asr)
        .unwrap();
    let example = model.join("test_wavs/en.wav");
    std::fs::create_dir_all(example.parent().unwrap()).unwrap();
    std::fs::copy(&wav, &example).unwrap();
    let report = f.json(&["setup", "try", "--model-example"]);
    assert!(report["meeting_id"].is_null());
    assert_eq!(std::fs::read(example).unwrap(), std::fs::read(wav).unwrap());
    assert!(
        !Config::load(&f.data_dir())
            .unwrap()
            .setup
            .completed_steps
            .iter()
            .any(|s| s == "try")
    );
}

#[test]
fn parity_library_search_uses_metadata_then_a_distinct_transcript_match() {
    let f = Fixture::new();
    let one = f.wav("synthetic-pricing-title.wav");
    let two = f.wav("synthetic-other-title.wav");
    let a = f.json(&[
        "import",
        one.to_str().unwrap(),
        "--title",
        "Pricing metadata",
    ]);
    // Distinct bytes make these separate imports.
    let mut samples = conversation();
    samples[17] = 0.4;
    write_wav(&two, &samples);
    let b = f.json(&[
        "import",
        two.to_str().unwrap(),
        "--title",
        "Meeting with a transcript",
    ]);
    let a = a["meeting_id"].as_str().unwrap();
    let b = b["meeting_id"].as_str().unwrap();
    assert_ne!(a, b);
    let lib = Library::open_existing(f.lib()).unwrap();
    let id = MeetingId::parse(a).unwrap();
    broscribe_core::ops::with_index(&lib, |tx, index| {
        let m = tx.update_meeting(&id, |m| {
            m.app = Some("synthetic-app".into());
            m.tags.push("synthetic-tag".into());
            m.bookmarks.push(broscribe_core::schema::Bookmark {
                at_s: 1.0,
                label: Some("Synthetic bookmark".into()),
            });
            Ok(())
        })?;
        index.upsert_meeting(tx, &m)
    })
    .unwrap();
    let matches = f.json(&["meetings", "pricing", "--include-transcripts"]);
    assert_eq!(matches["meetings"][0]["meeting_id"], a);
    assert!(
        matches["meetings"][0]["speakers_need_name"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(matches["transcript_matches"].as_array().unwrap().len(), 1);
    assert_eq!(matches["transcript_matches"][0]["meeting_id"], b);
    let filters = f.json(&["library", "filters"]);
    assert_eq!(filters["tags"][0]["meetings"], 1);
    assert_eq!(filters["apps"][0]["id"], "synthetic-app");
    let shown = f.json(&["show", a]);
    assert_eq!(shown["app"], "synthetic-app");
    assert_eq!(shown["user_tags"][0], "synthetic-tag");
    assert_eq!(shown["bookmarks"][0]["at_s"], 1.0);
    assert_eq!(
        f.json(&["meeting", "timeline", a])["bookmarks"],
        shown["bookmarks"]
    );
    assert_eq!(f.json(&["transcript", a])["bookmarks"], shown["bookmarks"]);
}

#[test]
fn parity_people_search_and_sample_projection_keep_embeddings_private() {
    let f = Fixture::new();
    let path = f.wav("synthetic-speaker.wav");
    let m = f.json(&["import", path.to_str().unwrap(), "--handling", "reference"]);
    let id = m["meeting_id"].as_str().unwrap();
    let named = f.json(&["speakers", "name", id, "S1", "Ágata Synthetic"]);
    let person = named["person_id"].as_str().unwrap();
    let shown = f.json(&["people", "list", "AGATA"]);
    assert_eq!(shown["people"][0]["id"], person);
    assert!(shown["people"][0]["voice_samples"].as_u64().unwrap() > 0);
    // The person's stored hue (1 to 7), the same in the list and in `show`.
    let hue = shown["people"][0]["hue"].as_u64().unwrap();
    assert!((1..=7).contains(&hue));
    assert_eq!(f.json(&["people", "show", person])["hue"], hue);
    let samples = f.json(&["people", "samples", person]);
    let sample = &samples["samples"][0];
    assert_eq!(sample["meeting_id"], id);
    assert_eq!(sample["audio_available"], true);
    assert!(sample.get("vector_b64").is_none());
    let target = f.tmp.path().join("synthetic-voice-cut.wav");
    f.json(&[
        "people",
        "clip",
        person,
        sample["id"].as_str().unwrap(),
        "--out",
        target.to_str().unwrap(),
    ]);
    assert!(target.is_file());
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        f.json(&["people", "samples", person])["samples"][0]["audio_available"],
        false
    );
}

#[test]
fn parity_setup_checks_show_pin_saved_audio_and_voice_readings() {
    let f = Fixture::new();
    assert_eq!(f.json(&["audio", "pin-status"])["pinned_mic"], Value::Null);
    assert!(f.json(&["audio", "check-status"])["last"].is_null());
    let cfg = Config::update(&f.data_dir(), |c| {
        c.audio.pinned_mic = Some("disconnected-synthetic-device".into());
        c.set_audio_check(broscribe_core::config::AudioCheckChoice {
            input_uid: "synthetic-input".into(),
            output_uid: "synthetic-output".into(),
            output_data_source: Some("Synthetic headphones".into()),
            echo_handling: "none".into(),
        });
        Ok(())
    })
    .unwrap();
    assert_eq!(cfg.0.audio_checks.len(), 1);
    assert_eq!(f.json(&["audio", "pin-status"])["connected"], false);
    assert_eq!(
        f.json(&["audio", "check-status"])["output_name"],
        "Synthetic headphones"
    );
    assert_eq!(f.json(&["setup", "mic-test"])["seconds"], 2);
    f.json(&["setup", "--name", "Synthetic Me", "--non-interactive"]);
    let lib = Library::open_existing(f.lib()).unwrap();
    let person = lib.self_person().unwrap().unwrap();
    let folder = lib.person_dir(&person.id).join("enrollment");
    std::fs::create_dir_all(&folder).unwrap();
    write_wav(&folder.join("synthetic-reading.wav"), &[0.0; 16]);
    let voice = f.json(&["setup", "voice-status"]);
    assert_eq!(voice["readings"], 1);
    assert_eq!(voice["bytes"], 76);
    assert!(
        f.json(&["library", "disk-usage"])["library_bytes"]
            .as_u64()
            .unwrap()
            >= 76
    );
    let plan = f.json(&["setup", "model-check"]);
    assert!(plan["download_bytes"].as_u64().unwrap() > 0);
    assert!(plan["low_disk"].is_boolean());
}

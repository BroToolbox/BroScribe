//! Deterministic synthetic benchmark harness for public core and DSP seams.
//!
//! ```text
//! cargo run --release -p broscribe-cli --example remediation_bench -- \
//!     [--scenarios library,queue,identity,dsp,journal] [--meetings 20,80] [--segments 60]
//!     [--speakers 4] [--people 24] [--confirmed 6] [--weak 3] [--dim 192]
//!     [--probes 64] [--backlog 100] [--jobs 16] [--dsp-seconds 30]
//!     [--journal-seconds 15,60] [--journal-close-delay-ms 0,250] [--journal-block-ms 10]
//!     [--journal-chunk-ms 5000] [--journal-mic-rate 48000] [--journal-system-rate 48000]
//!     [--journal-pause-ms 1500] [--reps 3] [--seed 20261117]
//! ```
//!
//! Every input is invented and generated from `--seed`: meetings, transcripts,
//! people, voice embeddings, audio-like noise and journal PCM. Libraries and
//! recording journals live in one temporary directory this process creates
//! and removes; no default app path, user library or journal, audio device,
//! model, credential or network is touched.
//!
//! Each scenario builds its fixture first (not timed), then times only the
//! public operations it names, and checks a correctness invariant of each
//! result before its timing is kept. A failed invariant aborts the run with
//! a non-zero exit and no report.
//!
//! Scenarios:
//! - `library` (one library per `--meetings` size, for scaling): publishing
//!   meetings (`meeting.json` + `meeting.md`), per-meeting index upserts,
//!   listing meetings, loading each by ID alone and all through one
//!   `Library::meeting_lookup`, a locked meeting update, a full index
//!   rebuild, read-only index open and keyword / meeting search.
//! - `queue`: enqueue, listing with a queued backlog, claim, commit as done
//!   (publication), release, and removal of finished jobs.
//! - `identity`: loading stored voiceprints into per-person samples, ranking
//!   and resolving speaker centroids, merging over-split clusters, cosine
//!   arithmetic, sample capping and the stored vector codec.
//! - `dsp`: GCC-PHAT alignment and drift fit, reference re-timing, frame
//!   energies + energy VAD, residue gating, one GCC-PHAT search and FFT
//!   convolution.
//! - `journal` (one workload per `--journal-seconds` length and per
//!   `--journal-close-delay-ms` policy): the capture crate's durable
//!   two-track `JournalWriter` fed unpaced (back to back, not at the capture
//!   rate) with invented PCM blocks, with one rotation, bookmark and pause,
//!   closed chunks taken back as the recorder's live preview does, then
//!   either finished or dropped like a crash and recovered. Writer counters
//!   come from `JournalWriter::stats`, sizes from the files on disk; every
//!   journaled sample and chunk start time is read back and compared exactly
//!   before the workload is reported. Creating the journal (setup) and
//!   removing it (cleanup) are timed separately from capture.
//!
//! Output is one bounded JSON document on stdout: parameters, environment,
//! provenance and per-operation summaries (min, median, mean, max ns), never
//! raw samples, paths or generated text. Timings are observations on the
//! machine that ran them, never assertions; a debug build is reported as such.

use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use broscribe_capture::journal::{
    self, JournalStats, JournalWriter, PauseSpan, SessionManifest, SessionMeta, SessionState,
};
use broscribe_core::config::Thresholds;
use broscribe_core::identity::{self, MAX_CONFIRMED, MergeInput};
use broscribe_core::index::{Index, SearchMode, SearchQuery};
use broscribe_core::ops::load_person_samples;
use broscribe_core::paths;
use broscribe_core::queue::{JobState, Queue};
use broscribe_core::schema::{
    self, Assignment, Meeting, Person, ProcessingStatus, Segment, Source, Speaker, Strength, Track,
    TrackRole, Voiceprint, VoiceprintKind, VoiceprintOrigin, VoiceprintSource, Voiceprints,
};
use broscribe_core::{Library, MeetingId, PersonId, SpeakerId};
use broscribe_dsp::synth::Rng;
use broscribe_dsp::{
    AlignConfig, GateConfig, VadConfig, align_reference, convolve, energy_vad, estimate_alignment,
    frame_energies, frame_len, gcc_phat, residue_gate,
};
use broscribe_platform_native::NativePlatform;
use chrono::{DateTime, TimeDelta, Utc};
use clap::{Parser, ValueEnum};
use serde_json::{Value, json};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const REPORT_SCHEMA: &str = "broscribe.remediation_bench.v1";
/// Invented token placed in some segments and tags, so searches have an
/// exact expected hit count.
const MARKER: &str = "zephyrquartz";
/// Tag toggled by the timed meeting update.
const TOUCH_TAG: &str = "bench-touched";
const BACKLOG_KIND: &str = "bench-backlog";
const CLAIM_KIND: &str = "bench-claim";
const EMBED_MODEL: &str = "synthetic-embedding";
/// Expected norm of the noise added to a person's unit base vector: same-person
/// embeddings then agree at a cosine of about 0.98 whatever the dimension.
const EMBED_NOISE_NORM: f64 = 0.14;
/// Meeting IDs for voiceprint provenance, above any library meeting ID.
const SAMPLE_MEETING_BASE: usize = 0x10_0000;
const DSP_RATE: u32 = 16_000;
/// Acoustic delay of the synthetic echo path (37 ms at 16 kHz).
const DSP_DELAY: usize = 592;
const DSP_ECHO_GAIN: f32 = 0.5;
const IMPULSE_AT: usize = 300;
const IMPULSE_LEN: usize = 4096;

const WORDS: &[&str] = &[
    "agenda",
    "budget",
    "timeline",
    "release",
    "review",
    "launch",
    "metric",
    "roadmap",
    "design",
    "testing",
    "feedback",
    "priority",
    "customer",
    "quarter",
    "estimate",
    "pipeline",
    "backlog",
    "sprint",
    "deadline",
    "prototype",
    "vendor",
    "contract",
    "hiring",
    "onboarding",
    "migration",
    "database",
    "latency",
    "dashboard",
    "forecast",
    "inventory",
    "shipping",
    "pricing",
    "support",
    "incident",
    "retro",
    "planning",
    "checklist",
    "workshop",
    "survey",
    "draft",
    "approval",
    "summary",
    "decision",
    "action",
    "owner",
    "schedule",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Scenario {
    Library,
    Queue,
    Identity,
    Dsp,
    Journal,
}

impl Scenario {
    fn as_str(self) -> &'static str {
        match self {
            Scenario::Library => "library",
            Scenario::Queue => "queue",
            Scenario::Identity => "identity",
            Scenario::Dsp => "dsp",
            Scenario::Journal => "journal",
        }
    }
}

/// Synthetic benchmark of BroScribe's public library, queue, index, identity
/// and DSP operations. Prints one JSON report on stdout.
#[derive(Debug, Parser)]
#[command(name = "remediation_bench")]
struct Args {
    /// Scenarios to run, comma-separated.
    #[arg(long, value_enum, value_delimiter = ',', default_values = ["library", "queue", "identity", "dsp", "journal"])]
    scenarios: Vec<Scenario>,
    /// Library sizes in meetings, comma-separated; one library per size (1-8 sizes, each 1-5000).
    #[arg(long, value_delimiter = ',', default_values = ["20", "80"])]
    meetings: Vec<usize>,
    /// Transcript segments per meeting (1-2000).
    #[arg(long, default_value_t = 60)]
    segments: usize,
    /// Speakers per meeting (1-32).
    #[arg(long, default_value_t = 4)]
    speakers: usize,
    /// People in each library and in the identity scenario (1-2000).
    #[arg(long, default_value_t = 24)]
    people: usize,
    /// Confirmed voice samples per person (1-200).
    #[arg(long, default_value_t = 6)]
    confirmed: usize,
    /// Weak voice samples per person (0-50).
    #[arg(long, default_value_t = 3)]
    weak: usize,
    /// Embedding dimension (64-1024).
    #[arg(long, default_value_t = 192)]
    dim: usize,
    /// Speaker centroids resolved per repetition (1-2048).
    #[arg(long, default_value_t = 64)]
    probes: usize,
    /// Queued jobs of another kind present while the queue is listed (0-5000).
    #[arg(long, default_value_t = 100)]
    backlog: usize,
    /// Jobs enqueued, claimed, committed and released per repetition (1-500).
    #[arg(long, default_value_t = 16)]
    jobs: usize,
    /// Length of the synthetic two-track signal in seconds (10-600).
    #[arg(long, default_value_t = 30)]
    dsp_seconds: usize,
    /// Repetitions of each timed operation group (1-50).
    #[arg(long, default_value_t = 3)]
    reps: usize,
    /// Synthetic recording lengths in seconds for the journal scenario, comma-separated (1-4 lengths, each 10-3600).
    #[arg(long, value_delimiter = ',', default_values = ["15", "60"])]
    journal_seconds: Vec<u64>,
    /// Chunk-close manifest delays to run, comma-separated (1-4, each 0-5000 ms; the recorder uses 250, 0 writes every close at once).
    #[arg(long, value_delimiter = ',', default_values = ["0", "250"])]
    journal_close_delay_ms: Vec<u64>,
    /// Capture block length per append in ms (1-100); each rate must give whole frames.
    #[arg(long, default_value_t = 10)]
    journal_block_ms: u64,
    /// Journal chunk length in ms (100-60000), a multiple of the block length.
    #[arg(long, default_value_t = (journal::CHUNK_SECONDS * 1000.0) as u64)]
    journal_chunk_ms: u64,
    /// Microphone track rate in Hz (8000-192000).
    #[arg(long, default_value_t = 48_000)]
    journal_mic_rate: u32,
    /// System track rate in Hz (8000-192000).
    #[arg(long, default_value_t = 48_000)]
    journal_system_rate: u32,
    /// Length of the one pause in each journal workload, in ms (0-60000).
    #[arg(long, default_value_t = 1500)]
    journal_pause_ms: u64,
    /// Seed of every generator.
    #[arg(long, default_value_t = 20_261_117)]
    seed: u64,
}

impl Args {
    fn validate(&mut self) -> Res<()> {
        fn within(name: &str, v: usize, lo: usize, hi: usize) -> Res<()> {
            if (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(format!("--{name} {v} is outside {lo}..={hi}").into())
            }
        }
        let mut seen = Vec::new();
        self.scenarios.retain(|s| {
            let first = !seen.contains(s);
            seen.push(*s);
            first
        });
        let mut sizes = Vec::new();
        self.meetings.retain(|m| {
            let first = !sizes.contains(m);
            sizes.push(*m);
            first
        });
        if self.scenarios.is_empty() {
            return Err("--scenarios names no scenario".into());
        }
        within("meetings (count of sizes)", self.meetings.len(), 1, 8)?;
        for &m in &self.meetings {
            within("meetings", m, 1, 5000)?;
        }
        within("segments", self.segments, 1, 2000)?;
        let largest = self.meetings.iter().copied().max().unwrap_or(0);
        if largest * self.segments > 2_000_000 {
            return Err("--meetings × --segments exceeds 2,000,000 segments".into());
        }
        within("speakers", self.speakers, 1, 32)?;
        within("people", self.people, 1, 2000)?;
        within("confirmed", self.confirmed, 1, 200)?;
        within("weak", self.weak, 0, 50)?;
        within("dim", self.dim, 64, 1024)?;
        within("probes", self.probes, 1, 2048)?;
        within("backlog", self.backlog, 0, 5000)?;
        within("jobs", self.jobs, 1, 500)?;
        within("dsp-seconds", self.dsp_seconds, 10, 600)?;
        within("reps", self.reps, 1, 50)?;
        let mut lengths = Vec::new();
        self.journal_seconds.retain(|s| {
            let first = !lengths.contains(s);
            lengths.push(*s);
            first
        });
        let mut delays = Vec::new();
        self.journal_close_delay_ms.retain(|d| {
            let first = !delays.contains(d);
            delays.push(*d);
            first
        });
        let wide = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
        within(
            "journal-seconds (count of lengths)",
            self.journal_seconds.len(),
            1,
            4,
        )?;
        for &s in &self.journal_seconds {
            within("journal-seconds", wide(s), 10, 3600)?;
        }
        within(
            "journal-close-delay-ms (count of delays)",
            self.journal_close_delay_ms.len(),
            1,
            4,
        )?;
        for &d in &self.journal_close_delay_ms {
            within("journal-close-delay-ms", wide(d), 0, 5000)?;
        }
        within("journal-block-ms", wide(self.journal_block_ms), 1, 100)?;
        within("journal-chunk-ms", wide(self.journal_chunk_ms), 100, 60_000)?;
        within("journal-pause-ms", wide(self.journal_pause_ms), 0, 60_000)?;
        for (name, rate) in [
            ("journal-mic-rate", self.journal_mic_rate),
            ("journal-system-rate", self.journal_system_rate),
        ] {
            within(name, rate as usize, 8000, 192_000)?;
            if !(u64::from(rate) * self.journal_block_ms).is_multiple_of(1000) {
                return Err(format!(
                    "--{name} {rate} Hz gives no whole frames per --journal-block-ms {} block",
                    self.journal_block_ms
                )
                .into());
            }
        }
        if !self.journal_chunk_ms.is_multiple_of(self.journal_block_ms) {
            return Err("--journal-chunk-ms must be a multiple of --journal-block-ms".into());
        }
        let longest = self.journal_seconds.iter().copied().max().unwrap_or(0);
        let bytes_per_second =
            4 * (u64::from(self.journal_mic_rate) + u64::from(self.journal_system_rate));
        if longest * bytes_per_second > 1 << 31 {
            return Err("--journal-seconds × track rates exceeds 2 GiB of journaled audio".into());
        }
        Ok(())
    }

    fn json(&self) -> Value {
        json!({
            "scenarios": self.scenarios.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            "meetings": self.meetings,
            "segments": self.segments,
            "speakers": self.speakers,
            "people": self.people,
            "confirmed": self.confirmed,
            "weak": self.weak,
            "dim": self.dim,
            "probes": self.probes,
            "backlog": self.backlog,
            "jobs": self.jobs,
            "dsp_seconds": self.dsp_seconds,
            "reps": self.reps,
            "seed": self.seed,
            "journal": {
                "seconds": self.journal_seconds,
                "close_delay_ms": self.journal_close_delay_ms,
                "block_ms": self.journal_block_ms,
                "chunk_ms": self.journal_chunk_ms,
                "mic_rate": self.journal_mic_rate,
                "system_rate": self.journal_system_rate,
                "pause_ms": self.journal_pause_ms,
            },
        })
    }
}

fn main() -> ExitCode {
    let mut args = Args::parse();
    match run(&mut args) {
        Ok(report) => match serde_json::to_string_pretty(&report) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("remediation_bench: report: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("remediation_bench: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &mut Args) -> Res<Value> {
    args.validate()?;
    let started_at = Utc::now().to_rfc3339();
    let temp = tempfile::Builder::new()
        .prefix("broscribe-bench-")
        .tempdir()?;
    let scenarios = run_scenarios(temp.path(), args);
    // Removes only the directory this process created, also after a failure.
    let cleanup = temp.close();
    let scenarios = scenarios?;
    if let Err(e) = cleanup {
        return Err(format!("removing the temporary directory failed ({:?})", e.kind()).into());
    }
    Ok(json!({
        "schema": REPORT_SCHEMA,
        "harness": {
            "name": "remediation_bench",
            "package": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "provenance": provenance(started_at),
        "environment": environment(),
        "parameters": args.json(),
        "data": "synthetic: generated from the seed in an owned temporary directory; no user library, app data, audio device, model, credential or network",
        "scenarios": scenarios,
        "cleanup": { "temp_dir_removed": true },
        "note": "Timings are observations on this machine and build; they are not assertions or claims of improvement.",
    }))
}

fn run_scenarios(root: &Path, args: &Args) -> Res<Vec<Value>> {
    let mut out = Vec::new();
    for &scenario in &args.scenarios {
        match scenario {
            Scenario::Library => {
                for &meetings in &args.meetings {
                    let dir = root.join(format!("library-{meetings}"));
                    out.push(library_scenario(&dir, meetings, args)?);
                }
            }
            Scenario::Queue => out.push(queue_scenario(&root.join("queue"), args)?),
            Scenario::Identity => out.push(identity_scenario(&root.join("identity"), args)?),
            Scenario::Dsp => out.push(dsp_scenario(args)?),
            Scenario::Journal => {
                for &seconds in &args.journal_seconds {
                    let dir = root.join(format!("journal-{seconds}"));
                    out.push(journal_scenario(&dir, seconds, args)?);
                }
            }
        }
    }
    Ok(out)
}

// ---- provenance and environment ----

fn provenance(started_at: String) -> Value {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let revision = git(&["rev-parse", "HEAD"]).filter(|r| r.bytes().all(|b| b.is_ascii_hexdigit()));
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty());
    json!({
        "git_revision": revision,
        "git_tracked_changes": dirty,
        "started_at": started_at,
        "finished_at": Utc::now().to_rfc3339(),
    })
}

fn environment() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "logical_cpus": std::thread::available_parallelism().ok().map(|n| n.get()),
        "debug_assertions": cfg!(debug_assertions),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
    })
}

// ---- timing ----

struct Op {
    name: &'static str,
    items: u64,
    ns: Vec<u64>,
}

/// Timings of one scenario, in first-seen order.
#[derive(Default)]
struct Ops(Vec<Op>);

impl Ops {
    /// One sample of `name` covering `items` items (constant per operation).
    fn add(&mut self, name: &'static str, items: usize, ns: u64) {
        let items = items as u64;
        match self.0.iter_mut().find(|o| o.name == name) {
            Some(op) => op.ns.push(ns),
            None => self.0.push(Op {
                name,
                items,
                ns: vec![ns],
            }),
        }
    }

    fn json(&self) -> Value {
        Value::Array(self.0.iter().map(summary).collect())
    }
}

fn summary(op: &Op) -> Value {
    let mut s = op.ns.clone();
    s.sort_unstable();
    let n = s.len();
    let median = if n % 2 == 1 {
        s[n / 2]
    } else {
        ((u128::from(s[n / 2 - 1]) + u128::from(s[n / 2])) / 2) as u64
    };
    let mean = (s.iter().map(|&x| u128::from(x)).sum::<u128>() / n as u128) as u64;
    json!({
        "op": op.name,
        "samples": n,
        "items_per_sample": op.items,
        "min_ns": s[0],
        "median_ns": median,
        "mean_ns": mean,
        "max_ns": s[n - 1],
        "median_ns_per_item": median / op.items.max(1),
    })
}

/// Runs `f` once and returns its result with the elapsed wall time.
fn timed<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let start = Instant::now();
    let out = f();
    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    (out, ns)
}

fn check(ok: bool, what: impl FnOnce() -> String) -> Res<()> {
    if ok {
        Ok(())
    } else {
        Err(format!("invariant failed: {}", what()).into())
    }
}

// ---- synthetic data ----

fn meeting_id(n: usize) -> Res<MeetingId> {
    Ok(MeetingId::parse(&format!("m-{n:06x}"))?)
}

fn person_id(n: usize) -> Res<PersonId> {
    Ok(PersonId::parse(&format!("p-{:06x}", n + 1))?)
}

fn sentence(rng: &mut Rng, marker: bool) -> String {
    let len = 8 + (rng.next_u64() % 9) as usize;
    let mut words: Vec<&str> = (0..len)
        .map(|_| WORDS[(rng.next_u64() % WORDS.len() as u64) as usize])
        .collect();
    if marker {
        words.insert(len / 2, MARKER);
    }
    words.join(" ")
}

/// Whether meeting `i` carries the marker tag, and segment `j` of it the
/// marker word.
fn marked(i: usize, j: usize) -> (bool, bool) {
    let meeting = i.is_multiple_of(3);
    (meeting, meeting && j.is_multiple_of(7))
}

fn synthetic_meeting(i: usize, args: &Args, rng: &mut Rng) -> Res<Meeting> {
    let base = DateTime::parse_from_rfc3339("2026-01-05T09:00:00+00:00")?;
    let started = base + TimeDelta::hours(i as i64 * 7);
    let mut m = Meeting::new(meeting_id(i + 1)?, Source::Import, started);
    m.title = Some(format!("Synthetic review {i:04}"));
    m.tracks = vec![Track {
        id: "mixed".into(),
        role: TrackRole::Mixed,
    }];
    let mut speakers: Vec<Speaker> = (1..=args.speakers)
        .map(|k| -> Res<Speaker> { Ok(Speaker::new(SpeakerId::new(k as u32)?, "mixed")) })
        .collect::<Res<_>>()?;
    for j in 0..args.segments {
        let k = j % args.speakers;
        let start = j as f64 * 4.0;
        let end = start + 3.5;
        speakers[k].talk_s += end - start;
        m.segments.push(Segment {
            speech_overlap: Vec::new(),
            id: j as u32,
            track: "mixed".into(),
            speaker: Some(speakers[k].id),
            start,
            end,
            text: sentence(rng, marked(i, j).1),
            words: Vec::new(),
        });
    }
    m.speakers = speakers;
    m.duration_s = args.segments as f64 * 4.0;
    m.processing.status = ProcessingStatus::Done;
    if marked(i, 0).0 {
        m.add_tag(MARKER);
    }
    m.add_tag(&format!("cohort-{}", i % 4));
    Ok(m)
}

fn unit_vector(rng: &mut Rng, dim: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dim).map(|_| rng.gaussian() as f32).collect();
    identity::l2_normalize(&mut v);
    v
}

fn jitter(base: &[f32], rng: &mut Rng) -> Vec<f32> {
    let sigma = EMBED_NOISE_NORM / (base.len().max(1) as f64).sqrt();
    let mut v: Vec<f32> = base
        .iter()
        .map(|&x| x + (sigma * rng.gaussian()) as f32)
        .collect();
    identity::l2_normalize(&mut v);
    v
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

// ---- library and index ----

fn library_scenario(root: &Path, meetings: usize, args: &Args) -> Res<Value> {
    let wall = Instant::now();
    let lib = Library::open(root)?;
    let mut rng = Rng::new(args.seed ^ 0x4c49_4252 ^ meetings as u64);

    // Fixture: people, index bootstrap and the meetings in memory.
    let people: Vec<Person> = (0..args.people)
        .map(|p| -> Res<Person> {
            Ok(Person::new(
                person_id(p)?,
                format!("Synthetic person {p:03}"),
            ))
        })
        .collect::<Res<_>>()?;
    lib.write(|tx| {
        for p in &people {
            tx.save_person(p)?;
        }
        Ok(())
    })?;
    let generated: Vec<Meeting> = (0..meetings)
        .map(|i| synthetic_meeting(i, args, &mut rng))
        .collect::<Res<_>>()?;
    let segments_total = meetings * args.segments;
    let marker_meetings = (0..meetings).filter(|&i| marked(i, 0).0).count();
    let marker_segments = (0..meetings)
        .flat_map(|i| (0..args.segments).map(move |j| marked(i, j).1))
        .filter(|&m| m)
        .count();

    let mut ops = Ops::default();
    for m in &generated {
        let (r, ns) = timed(|| {
            lib.write(|tx| {
                tx.create_meeting_dir(m)?;
                tx.save_meeting(m)
            })
        });
        r?;
        ops.add("library.save_meeting", 1, ns);
    }
    let dirs = lib.list_meeting_dirs()?;
    check(dirs.len() == meetings, || {
        format!("{} meeting folders, expected {meetings}", dirs.len())
    })?;
    let json_bytes: u64 = dirs
        .iter()
        .map(|d| file_len(&d.join(paths::MEETING_JSON)))
        .sum();
    let md_bytes: u64 = dirs
        .iter()
        .map(|d| file_len(&d.join(paths::MEETING_MD)))
        .sum();

    {
        let mut index = Index::open(lib.root())?;
        for m in &generated {
            let (r, ns) = timed(|| lib.write(|tx| index.upsert_meeting(tx, m)));
            r?;
            ops.add("index.upsert_meeting", 1, ns);
        }
        let indexed = index.meeting_ids()?.len();
        check(indexed == meetings, || {
            format!("{indexed} meetings indexed by upsert, expected {meetings}")
        })?;
    }

    for rep in 0..args.reps {
        let (ids, ns) = timed(|| lib.list_meeting_ids());
        let ids = ids?;
        check(ids.len() == meetings, || {
            format!("{} meeting IDs listed, expected {meetings}", ids.len())
        })?;
        ops.add("library.list_meeting_ids", 1, ns);

        let (loaded, ns) = timed(|| {
            ids.iter()
                .map(|id| lib.load_meeting(black_box(id)).map(|m| m.segments.len()))
                .sum::<broscribe_core::Result<usize>>()
        });
        let loaded = loaded?;
        check(loaded == segments_total, || {
            format!("{loaded} segments loaded, expected {segments_total}")
        })?;
        ops.add("library.load_meeting", meetings, ns);

        // The same loads through one read-operation lookup, its folder
        // listing included; `library.load_meeting` above stays the per-ID
        // lookup, one folder scan per meeting.
        let (loaded, ns) = timed(|| {
            lib.meeting_lookup().and_then(|lookup| {
                ids.iter()
                    .map(|id| lookup.load(black_box(id)).map(|m| m.segments.len()))
                    .sum::<broscribe_core::Result<usize>>()
            })
        });
        let loaded = loaded?;
        check(loaded == segments_total, || {
            format!("{loaded} segments loaded through one lookup, expected {segments_total}")
        })?;
        ops.add("library.load_meetings_scoped", meetings, ns);

        let (listed, ns) = timed(|| lib.list_people());
        let listed = listed?.len();
        check(listed == args.people, || {
            format!("{listed} people listed, expected {}", args.people)
        })?;
        ops.add("library.list_people", 1, ns);

        let target = &ids[rep % ids.len()];
        let add = rep.is_multiple_of(2);
        let (updated, ns) = timed(|| {
            lib.write(|tx| {
                tx.update_meeting(target, |m| {
                    if add {
                        m.add_tag(TOUCH_TAG);
                    } else {
                        m.remove_tag(TOUCH_TAG);
                    }
                    Ok(())
                })
            })
        });
        let tagged = updated?.effective_tags().iter().any(|t| t == TOUCH_TAG);
        check(tagged == add, || {
            format!("update_meeting left the touch tag {tagged}, expected {add}")
        })?;
        ops.add("library.update_meeting", 1, ns);

        let (report, ns) = timed(|| lib.write(|tx| Index::rebuild(tx)));
        let report = report?;
        check(
            report.meetings == meetings
                && report.people == args.people
                && report.utterances == segments_total
                && report.skipped.is_empty(),
            || {
                format!(
                    "rebuild indexed {} meetings, {} people, {} utterances, {} skipped",
                    report.meetings,
                    report.people,
                    report.utterances,
                    report.skipped.len()
                )
            },
        )?;
        ops.add("index.rebuild", meetings, ns);

        let (index, ns) = timed(|| Index::open_readonly(lib.root()));
        let index = index?;
        ops.add("index.open_readonly", 1, ns);

        let query = SearchQuery {
            text: Some(MARKER.into()),
            limit: None,
            mode: SearchMode::Keyword,
            ..SearchQuery::default()
        };
        let (hits, ns) = timed(|| index.search(black_box(&query)));
        let hits = hits?.len();
        check(hits == marker_segments, || {
            format!("{hits} keyword hits, expected {marker_segments}")
        })?;
        ops.add("index.search_keyword", 1, ns);

        let (found, ns) = timed(|| index.search_meetings(black_box(&query)));
        let found = found?.len();
        check(found == marker_meetings, || {
            format!("{found} meetings found, expected {marker_meetings}")
        })?;
        ops.add("index.search_meetings", 1, ns);

        let (indexed, ns) = timed(|| index.meeting_ids());
        let indexed = indexed?.len();
        check(indexed == meetings, || {
            format!("{indexed} indexed meeting IDs, expected {meetings}")
        })?;
        ops.add("index.meeting_ids", 1, ns);
    }

    Ok(json!({
        "scenario": "library",
        "meetings": meetings,
        "ops": ops.json(),
        "counters": {
            "segments": segments_total,
            "people": args.people,
            "marker_segments": marker_segments,
            "marker_meetings": marker_meetings,
            "meeting_json_bytes": json_bytes,
            "meeting_md_bytes": md_bytes,
            "index_db_bytes": file_len(&Index::path(lib.root())),
        },
        "wall_ns": u64::try_from(wall.elapsed().as_nanos()).unwrap_or(u64::MAX),
    }))
}

// ---- queue ----

fn queue_scenario(root: &Path, args: &Args) -> Res<Value> {
    let wall = Instant::now();
    let lib = Library::open(root)?;
    // Fixture: index bootstrap (first write) and the queued backlog.
    lib.write(|_| Ok(()))?;
    let queue = Queue::open(lib.root())?;
    let probe = NativePlatform::process_probe();
    lib.write(|tx| {
        for b in 0..args.backlog {
            queue.enqueue(
                tx,
                BACKLOG_KIND,
                &format!("backlog-{b:05}"),
                json!({ "synthetic": b }),
            )?;
        }
        Ok(())
    })?;

    let mut ops = Ops::default();
    for rep in 0..args.reps {
        let mut ids = Vec::with_capacity(args.jobs);
        for j in 0..args.jobs {
            let key = format!("r{rep:02}-j{j:04}");
            let (id, ns) = timed(|| {
                lib.write(|tx| queue.enqueue(tx, CLAIM_KIND, &key, json!({ "synthetic": j })))
            });
            ids.push(id?);
            ops.add("queue.enqueue", 1, ns);
        }

        let (listed, ns) = timed(|| queue.list(CLAIM_KIND));
        let listed = listed?;
        check(
            listed.len() == args.jobs && listed.iter().all(|j| j.state == JobState::Queued),
            || {
                format!(
                    "{} queued jobs listed, expected {}",
                    listed.len(),
                    args.jobs
                )
            },
        )?;
        ops.add("queue.list", 1, ns);

        let (all, ns) = timed(|| queue.jobs());
        let all = all?.len();
        check(all == args.backlog + args.jobs, || {
            format!("{all} jobs, expected {}", args.backlog + args.jobs)
        })?;
        ops.add("queue.jobs", 1, ns);

        for id in &ids {
            let (lease, ns) = timed(|| queue.claim(&lib, id, &*probe));
            let lease =
                lease?.ok_or_else(|| format!("invariant failed: job {id} not claimable"))?;
            ops.add("queue.claim", 1, ns);
            let (r, ns) = timed(|| lease.commit(&lib, JobState::Done, |_, _| Ok(())));
            r?;
            ops.add("queue.commit_done", 1, ns);
            let (r, ns) = timed(|| lease.release(&lib));
            r?;
            ops.add("queue.release", 1, ns);
            let job = queue.get(id)?;
            check(job.state == JobState::Done && job.lease.is_none(), || {
                format!("job {id} is {} after commit and release", job.state)
            })?;
        }

        let (removed, ns) = timed(|| lib.write(|tx| queue.remove_finished(tx, CLAIM_KIND)));
        let removed = removed?;
        check(removed == args.jobs, || {
            format!("{removed} finished jobs removed, expected {}", args.jobs)
        })?;
        ops.add("queue.remove_finished", 1, ns);
    }
    let left = queue.list(BACKLOG_KIND)?.len();
    check(left == args.backlog, || {
        format!("{left} backlog jobs left, expected {}", args.backlog)
    })?;

    Ok(json!({
        "scenario": "queue",
        "ops": ops.json(),
        "counters": {
            "backlog": args.backlog,
            "jobs_per_rep": args.jobs,
        },
        "wall_ns": u64::try_from(wall.elapsed().as_nanos()).unwrap_or(u64::MAX),
    }))
}

// ---- identity ----

fn identity_scenario(root: &Path, args: &Args) -> Res<Value> {
    let wall = Instant::now();
    let lib = Library::open(root)?;
    let mut rng = Rng::new(args.seed ^ 0x4944_454e);
    let dim = args.dim;

    // Fixture: people with confirmed and weak voiceprints stored in the library.
    let bases: Vec<Vec<f32>> = (0..args.people)
        .map(|_| unit_vector(&mut rng, dim))
        .collect();
    let ids: Vec<PersonId> = (0..args.people).map(person_id).collect::<Res<_>>()?;
    let mut stored: Vec<Vec<f32>> = Vec::new();
    for (p, base) in bases.iter().enumerate() {
        let person = Person::new(ids[p].clone(), format!("Synthetic person {p:03}"));
        let mut prints = Voiceprints::empty(person.id.clone());
        for s in 0..args.confirmed + args.weak {
            let confirmed = s < args.confirmed;
            let (kind, strength) = if confirmed {
                (VoiceprintKind::Confirmation, Strength::Confirmed)
            } else {
                (VoiceprintKind::Auto, Strength::Weak)
            };
            let meeting = meeting_id(SAMPLE_MEETING_BASE + s)?;
            let start = s as f64 * 20.0;
            let end = start + 12.0;
            let v = jitter(base, &mut rng);
            prints.embeddings.push(Voiceprint {
                id: schema::voiceprint_id(
                    &person.id,
                    EMBED_MODEL,
                    VoiceprintOrigin::Meeting { id: &meeting },
                    start,
                    end,
                    kind,
                ),
                model: EMBED_MODEL.into(),
                dim,
                vector_b64: schema::encode_vector(&v),
                source: VoiceprintSource {
                    kind,
                    meeting_id: Some(meeting),
                    sha256: None,
                    start,
                    end,
                    ranges: Vec::new(),
                    speaker_id: None,
                    speaker_namespace: String::new(),
                },
                device: None,
                strength,
                created_at: schema::now(),
            });
            if confirmed {
                stored.push(v);
            }
        }
        lib.write(|tx| {
            tx.save_person(&person)?;
            tx.save_voiceprints(&prints)
        })?;
    }
    let truth: Vec<usize> = (0..args.probes).map(|q| q % args.people).collect();
    let probes: Vec<Vec<f32>> = truth.iter().map(|&p| jitter(&bases[p], &mut rng)).collect();
    let same_pairs: usize = (0..args.people)
        .map(|p| {
            let n = truth.iter().filter(|&&t| t == p).count();
            n * n.saturating_sub(1) / 2
        })
        .sum();
    let merge_people = args.people.min(8);
    let merge_inputs: Vec<MergeInput> = (0..2 * merge_people)
        .map(|k| -> Res<MergeInput> {
            Ok(MergeInput {
                id: SpeakerId::new(k as u32 + 1)?,
                centroid: jitter(&bases[k / 2], &mut rng),
                talk_s: 30.0 + k as f64,
                confirmed_person: None,
                rejected: Vec::new(),
            })
        })
        .collect::<Res<_>>()?;
    let cap_pool: Vec<Vec<f32>> = (0..4 * MAX_CONFIRMED)
        .map(|_| jitter(&bases[0], &mut rng))
        .collect();
    let thresholds = Thresholds {
        auto_name: Some(0.90),
        suggest: Some(0.80),
        margin: Some(0.10),
        merge: Some(0.54),
        ..Thresholds::default()
    };

    let mut ops = Ops::default();
    for _ in 0..args.reps {
        let (samples, ns) = timed(|| load_person_samples(&lib, EMBED_MODEL));
        let samples = samples?;
        let people = samples.all();
        check(
            people.len() == args.people
                && people
                    .iter()
                    .all(|p| p.confirmed.len() == args.confirmed && p.weak.len() == args.weak),
            || {
                format!(
                    "{} people with samples, expected {}",
                    people.len(),
                    args.people
                )
            },
        )?;
        ops.add("identity.load_person_samples", args.people, ns);

        let (ranked, ns) = timed(|| {
            black_box(&probes)
                .iter()
                .map(|c| identity::rank(c, people, &[], None, Some(&thresholds)))
                .collect::<Vec<_>>()
        });
        let wrong = ranked
            .iter()
            .zip(&truth)
            .filter(|(r, t)| r.first().map(|b| &b.person_id) != Some(&ids[**t]))
            .count();
        check(wrong == 0, || {
            format!("{wrong} probes ranked the wrong person first")
        })?;
        ops.add("identity.rank", args.probes, ns);

        let (resolved, ns) = timed(|| {
            black_box(&probes)
                .iter()
                .map(|c| identity::resolve_voice(c, people, &[], Some(&thresholds), None, None))
                .collect::<Vec<_>>()
        });
        let wrong = resolved
            .iter()
            .zip(&truth)
            .filter(|(r, t)| {
                r.assignment != Assignment::Auto || r.person_id.as_ref() != Some(&ids[**t])
            })
            .count();
        check(wrong == 0, || {
            format!("{wrong} probes not auto-named as their person")
        })?;
        ops.add("identity.resolve_voice", args.probes, ns);

        let (groups, ns) =
            timed(|| identity::merge_clusters(black_box(&merge_inputs), thresholds.merge, None));
        let person_of = |s: SpeakerId| (s.number() as usize - 1) / 2;
        check(
            groups.len() == merge_people
                && groups.iter().all(|g| {
                    g.merged.len() == 1
                        && g.merged.iter().all(|&m| person_of(m) == person_of(g.kept))
                }),
            || {
                format!(
                    "{} merge groups, expected {merge_people} pairs",
                    groups.len()
                )
            },
        )?;
        ops.add("identity.merge_clusters", merge_inputs.len(), ns);

        let ((above, checksum), ns) = timed(|| {
            let p = black_box(&probes);
            let mut above = 0usize;
            let mut checksum = 0f64;
            for (i, a) in p.iter().enumerate() {
                for b in &p[i + 1..] {
                    let c = identity::cosine(a, b);
                    checksum += f64::from(c);
                    if c > 0.5 {
                        above += 1;
                    }
                }
            }
            (above, checksum)
        });
        check(above == same_pairs && checksum.is_finite(), || {
            format!("{above} probe pairs above 0.5, expected {same_pairs}")
        })?;
        let pairs = args.probes * args.probes.saturating_sub(1) / 2;
        ops.add("identity.cosine_pairs", pairs.max(1), ns);

        let (kept, ns) =
            timed(|| identity::cap_samples(black_box(cap_pool.as_slice()), MAX_CONFIRMED));
        check(
            kept.len() == MAX_CONFIRMED
                && kept.windows(2).all(|w| w[0] < w[1])
                && kept.last().is_some_and(|&k| k < cap_pool.len()),
            || format!("cap_samples kept {} of {}", kept.len(), cap_pool.len()),
        )?;
        ops.add("identity.cap_samples", cap_pool.len(), ns);

        let (roundtrip, ns) = timed(|| {
            black_box(&stored)
                .iter()
                .map(|v| schema::decode_vector(&schema::encode_vector(v), v.len()))
                .collect::<broscribe_core::Result<Vec<_>>>()
        });
        let roundtrip = roundtrip?;
        check(roundtrip == stored, || {
            "vector codec round trip changed a vector".into()
        })?;
        ops.add("schema.vector_codec_roundtrip", stored.len(), ns);
    }

    Ok(json!({
        "scenario": "identity",
        "ops": ops.json(),
        "counters": {
            "people": args.people,
            "confirmed_per_person": args.confirmed,
            "weak_per_person": args.weak,
            "dim": dim,
            "probes": args.probes,
            "merge_speakers": merge_inputs.len(),
            "cap_pool": cap_pool.len(),
            "thresholds": {
                "auto_name": thresholds.auto_name,
                "suggest": thresholds.suggest,
                "margin": thresholds.margin,
                "merge": thresholds.merge,
                "synthetic_only": true,
            },
        },
        "wall_ns": u64::try_from(wall.elapsed().as_nanos()).unwrap_or(u64::MAX),
    }))
}

// ---- DSP ----

/// Whether second `b` of the synthetic system track carries sound (2 of 3).
fn block_active(b: usize) -> bool {
    b % 3 != 2
}

fn dsp_scenario(args: &Args) -> Res<Value> {
    let wall = Instant::now();
    let rate = DSP_RATE;
    let per_second = rate as usize;
    let len = args.dsp_seconds * per_second;
    let mut rng = Rng::new(args.seed ^ 0x4453_5000);

    // Fixture: system bursts of white noise; the mic hears them delayed and
    // attenuated over a faint noise floor.
    let system: Vec<f32> = (0..len)
        .map(|n| {
            if block_active(n / per_second) {
                (0.1 * rng.gaussian()) as f32
            } else {
                0.0
            }
        })
        .collect();
    let mic: Vec<f32> = (0..len)
        .map(|n| {
            let echo = if n >= DSP_DELAY {
                DSP_ECHO_GAIN * system[n - DSP_DELAY]
            } else {
                0.0
            };
            echo + (1e-4 * rng.gaussian()) as f32
        })
        .collect();
    let delay_s = DSP_DELAY as f64 / f64::from(rate);
    let align_cfg = AlignConfig {
        window_s: 5.0,
        hop_s: 5.0,
        ..AlignConfig::default()
    };
    let mut impulse = vec![0.0f32; IMPULSE_LEN];
    impulse[IMPULSE_AT] = 1.0;
    let vad = VadConfig::default();

    let mut ops = Ops::default();
    for _ in 0..args.reps {
        let (alignment, ns) =
            timed(|| estimate_alignment(black_box(&mic), black_box(&system), rate, &align_cfg));
        let alignment = alignment
            .ok_or("invariant failed: no alignment for a delayed copy of the system track")?;
        check(
            (alignment.offset_s - delay_s).abs() < 1e-3 && alignment.drift_ppm.abs() < 50.0,
            || {
                format!(
                    "alignment offset {:.6} s drift {:.3} ppm, expected {delay_s:.6} s and no drift",
                    alignment.offset_s, alignment.drift_ppm
                )
            },
        )?;
        ops.add("dsp.estimate_alignment", args.dsp_seconds, ns);

        let (aligned, ns) =
            timed(|| align_reference(black_box(&system), mic.len(), rate, &alignment, 0.0));
        let (residual, energy) = mic
            .iter()
            .zip(&aligned)
            .fold((0f64, 0f64), |(r, e), (&m, &a)| {
                let d = f64::from(m - DSP_ECHO_GAIN * a);
                (r + d * d, e + f64::from(m) * f64::from(m))
            });
        check(
            aligned.len() == mic.len() && residual < 0.1 * energy,
            || {
                format!(
                    "re-timed reference leaves {:.4} of the mic energy",
                    residual / energy.max(f64::MIN_POSITIVE)
                )
            },
        )?;
        ops.add("dsp.align_reference", args.dsp_seconds, ns);

        let (active, ns) = timed(|| {
            energy_vad(
                &frame_energies(black_box(&system), frame_len(rate)),
                black_box(&vad),
            )
        });
        let frames_per_second = per_second / frame_len(rate);
        let wrong = active
            .iter()
            .enumerate()
            .filter(|&(f, &a)| {
                let (b, pos) = (f / frames_per_second, f % frames_per_second);
                if block_active(b) {
                    !a
                } else {
                    // Past the hangover the frame must be inactive.
                    pos >= vad.hangover_frames + 5 && a
                }
            })
            .count();
        check(
            active.len() == args.dsp_seconds * frames_per_second && wrong == 0,
            || {
                format!(
                    "{wrong} of {} VAD frames contradict the bursts",
                    active.len()
                )
            },
        )?;
        ops.add("dsp.frame_energies_vad", args.dsp_seconds, ns);

        let (gated, ns) = timed(|| {
            residue_gate(
                black_box(&mic),
                black_box(&aligned),
                rate,
                &vad,
                &GateConfig::default(),
            )
        });
        check(
            gated.output.len() == mic.len()
                && gated.gated.len() == gated.system_active.len()
                && gated.erl_db.is_some_and(|e| (-12.0..=0.0).contains(&e)),
            || {
                format!(
                    "residue gate ERL {:?} dB, expected about -6 dB",
                    gated.erl_db
                )
            },
        )?;
        ops.add("dsp.residue_gate", args.dsp_seconds, ns);

        let (lo, hi) = (per_second, 2 * per_second);
        let (corr, ns) =
            timed(|| gcc_phat(black_box(&system[lo..hi]), black_box(&mic[lo..hi]), 1600));
        let corr = corr.ok_or("invariant failed: GCC-PHAT found no correlation")?;
        check((corr.lag - DSP_DELAY as f64).abs() < 0.5, || {
            format!("GCC-PHAT lag {:.3} samples, expected {DSP_DELAY}", corr.lag)
        })?;
        ops.add("dsp.gcc_phat_1s", 1, ns);

        let input = &system[..per_second];
        let (out, ns) = timed(|| convolve(black_box(input), black_box(&impulse)));
        let err = input
            .iter()
            .zip(out.get(IMPULSE_AT..).unwrap_or_default())
            .map(|(x, y)| (x - y).abs())
            .fold(0f32, f32::max);
        check(
            out.len() == input.len() + IMPULSE_LEN - 1 && err < 1e-3,
            || format!("convolution with a shifted impulse differs by {err}"),
        )?;
        ops.add("dsp.convolve_1s_4096", 1, ns);
    }

    Ok(json!({
        "scenario": "dsp",
        "ops": ops.json(),
        "counters": {
            "rate_hz": rate,
            "seconds": args.dsp_seconds,
            "echo_delay_samples": DSP_DELAY,
            "echo_gain": DSP_ECHO_GAIN,
            "active_seconds": (0..args.dsp_seconds).filter(|&b| block_active(b)).count(),
        },
        "wall_ns": u64::try_from(wall.elapsed().as_nanos()).unwrap_or(u64::MAX),
    }))
}

// ---- capture journal ----

/// Invented PCM sample `frame` of `track`, in [-0.1, 0.1): a pure function of
/// the seed, so the journal is checked against it without keeping a copy.
fn journal_pcm(seed: u64, track: journal::Track, frame: u64) -> f32 {
    let mut x = seed
        ^ (track.index() as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ frame.wrapping_mul(0xD1B5_4A32_D192_ED03);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^= x >> 33;
    ((x >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.2
}

/// One journal workload: both tracks get one block per step, a rotation at a
/// third, a bookmark at half and a pause at two thirds of the recording.
struct JournalPlan {
    seconds: u64,
    block_ms: u64,
    chunk_ms: u64,
    /// Indexed by `journal::Track::index`.
    rates: [u32; 2],
    close_delay_ms: u64,
    pause_ms: u64,
    seed: u64,
}

impl JournalPlan {
    fn rate(&self, track: journal::Track) -> u32 {
        self.rates[track.index()]
    }

    /// Frames per appended block (whole, see `Args::validate`).
    fn block_frames(&self, track: journal::Track) -> u64 {
        u64::from(self.rate(track)) * self.block_ms / 1000
    }

    fn blocks(&self) -> u64 {
        self.seconds * 1000 / self.block_ms
    }

    fn block_ns(&self) -> u64 {
        self.block_ms * 1_000_000
    }

    fn pause_ns(&self) -> u64 {
        self.pause_ms * 1_000_000
    }

    /// The block before which the recording pauses.
    fn pause_block(&self) -> u64 {
        2 * self.blocks() / 3
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// Stopped normally: `JournalWriter::finish`.
    Finish,
    /// The writer is dropped without finishing, then `journal::recover` runs.
    Crash,
}

impl Ending {
    fn as_str(self) -> &'static str {
        match self {
            Ending::Finish => "finish",
            Ending::Crash => "crash",
        }
    }
}

fn journal_scenario(root: &Path, seconds: u64, args: &Args) -> Res<Value> {
    let wall = Instant::now();
    let mut policies = Vec::new();
    for &close_delay_ms in &args.journal_close_delay_ms {
        let plan = JournalPlan {
            seconds,
            block_ms: args.journal_block_ms,
            chunk_ms: args.journal_chunk_ms,
            rates: [args.journal_mic_rate, args.journal_system_rate],
            close_delay_ms,
            pause_ms: args.journal_pause_ms,
            seed: args.seed ^ 0x4a4f_5552 ^ seconds,
        };
        let mut ops = Ops::default();
        let mut last_rep = Vec::new();
        for rep in 0..args.reps {
            last_rep.clear();
            for ending in [Ending::Finish, Ending::Crash] {
                let id = format!(
                    "bench-{seconds}s-d{close_delay_ms}-r{rep}-{}",
                    ending.as_str()
                );
                last_rep.push(journal_session(root, &id, &plan, ending, &mut ops)?);
            }
        }
        policies.push(json!({
            "close_delay_ms": close_delay_ms,
            "ops": ops.json(),
            "last_rep": last_rep,
        }));
    }
    Ok(json!({
        "scenario": "journal",
        "seconds": seconds,
        "pacing": "unpaced: blocks are appended back to back, not at the capture rate",
        "policies": policies,
        "wall_ns": u64::try_from(wall.elapsed().as_nanos()).unwrap_or(u64::MAX),
    }))
}

/// Runs one journal workload, verifies everything journaled, reports it and
/// removes the journal.
fn journal_session(
    root: &Path,
    id: &str,
    plan: &JournalPlan,
    ending: Ending,
    ops: &mut Ops,
) -> Res<Value> {
    use journal::Track;

    // Setup: the session folder, its first manifest and both segments, with
    // the policy the workload compares.
    let meta = SessionMeta {
        title: Some("Synthetic journal".into()),
        ..SessionMeta::default()
    };
    let (writer, ns) = timed(|| -> broscribe_core::Result<JournalWriter> {
        let mut w = JournalWriter::create(
            root,
            id,
            meta,
            plan.chunk_ms as f64 / 1000.0,
            std::process::id(),
        )?;
        w.defer_close_writes(Duration::from_millis(plan.close_delay_ms));
        w.keep_closed_audio();
        for track in Track::ALL {
            w.begin_segment(
                track,
                0,
                Some(format!("Synthetic {}", track.as_str())),
                plan.rate(track),
                0,
            )?;
        }
        Ok(w)
    });
    let mut writer = writer?;
    ops.add("journal.setup_create", 1, ns);

    // Capture: every writer call from the first block to the finish or recovery.
    let blocks = plan.blocks();
    let (rotate_at, bookmark_at, pause_at) = (blocks / 3, blocks / 2, plan.pause_block());
    let poll_every = (20 / plan.block_ms).max(1);
    let mut capture_ns = 0u64;
    let mut taken = [None; 2];
    let mut taken_count = 0u64;
    let mut offset = 0u64;
    let mut buf: Vec<f32> = Vec::new();
    for i in 0..blocks {
        let host = i * plan.block_ns() + offset;
        if i == rotate_at {
            let (cutoff, ns) = timed(|| writer.rotate());
            let cutoff = cutoff?;
            check(cutoff.mic.is_some() && cutoff.system.is_some(), || {
                "rotation left a track without a durable chunk".into()
            })?;
            ops.add("journal.rotate", 1, ns);
            capture_ns += ns;
            capture_ns += take_closed(&mut writer, plan, &mut taken, &mut taken_count, ops)?;
        }
        if i == bookmark_at {
            let (r, ns) = timed(|| writer.bookmark(host, Some("synthetic".into())));
            r?;
            ops.add("journal.bookmark", 1, ns);
            capture_ns += ns;
        }
        if i == pause_at {
            let (r, ns) = timed(|| writer.pause(host));
            r?;
            ops.add("journal.pause", 1, ns);
            capture_ns += ns;
            capture_ns += take_closed(&mut writer, plan, &mut taken, &mut taken_count, ops)?;
            let (r, ns) = timed(|| writer.resume(host + plan.pause_ns()));
            r?;
            ops.add("journal.resume", 1, ns);
            capture_ns += ns;
            offset += plan.pause_ns();
        }
        let host = i * plan.block_ns() + offset;
        for track in Track::ALL {
            let frames = plan.block_frames(track);
            let first = i * frames;
            buf.clear();
            buf.extend((first..first + frames).map(|f| journal_pcm(plan.seed, track, f)));
            let (r, ns) = timed(|| writer.append(track, host, &buf));
            check(r?, || {
                format!("{} block {i} was not journaled", track.as_str())
            })?;
            ops.add("journal.append", 1, ns);
            capture_ns += ns;
        }
        capture_ns += take_closed(&mut writer, plan, &mut taken, &mut taken_count, ops)?;
        if (i + 1).is_multiple_of(poll_every) {
            let (r, ns) = timed(|| writer.flush_due());
            r?;
            ops.add("journal.flush_due", 1, ns);
            capture_ns += ns;
        }
    }
    let end_host = blocks * plan.block_ns() + offset;
    let stats = writer.stats();
    let audio_bytes: u64 = Track::ALL
        .iter()
        .map(|&t| 4 * blocks * plan.block_frames(t))
        .sum();
    check(
        stats.audio_bytes == audio_bytes && stats.kept_chunks_taken == taken_count,
        || {
            format!(
                "writer counted {} audio bytes and {} taken chunks, expected {audio_bytes} and {taken_count}",
                stats.audio_bytes, stats.kept_chunks_taken
            )
        },
    )?;
    let dir = writer.dir().to_path_buf();
    let manifest = match ending {
        Ending::Finish => {
            let (m, ns) = timed(|| writer.finish(end_host, "user"));
            let m = m?;
            ops.add("journal.finish", 1, ns);
            capture_ns += ns;
            m
        }
        Ending::Crash => {
            drop(writer);
            let (m, ns) = timed(|| journal::recover(&dir));
            let m = m?;
            ops.add("journal.recover", 1, ns);
            capture_ns += ns;
            m
        }
    };
    ops.add("journal.session_capture", 1, capture_ns);

    let (chunks, recovered) = verify_journal(&dir, plan, &manifest, end_host, ops)?;
    let manifest_bytes = file_len(&dir.join(journal::MANIFEST_FILE));
    let (chunk_files, chunk_file_bytes) = chunk_files(&dir)?;
    check(
        chunk_files == chunks as u64 && chunk_file_bytes == audio_bytes,
        || {
            format!(
                "{chunk_files} chunk files of {chunk_file_bytes} bytes, expected {chunks} of {audio_bytes}"
            )
        },
    )?;

    // Cleanup.
    let (r, ns) = timed(|| journal::remove(&dir));
    r?;
    check(!dir.exists(), || {
        "the removed journal folder still exists".into()
    })?;
    ops.add("journal.cleanup_remove", 1, ns);

    Ok(json!({
        "ending": ending.as_str(),
        "writer_stats": stats_json(stats),
        "writer_stats_scope": "JournalWriter::stats after the last block; the finish write and crash recovery are not counted by the writer",
        "manifest_bytes_final": manifest_bytes,
        "chunk_files": chunk_files,
        "chunk_file_bytes": chunk_file_bytes,
        "chunks_listed": chunks,
        "chunks_recovered": recovered,
        "closed_chunks_taken": taken_count,
        "capture_ns": capture_ns,
    }))
}

fn stats_json(stats: JournalStats) -> Value {
    json!({
        "manifest_writes": stats.manifest_writes,
        "manifest_bytes": stats.manifest_bytes,
        "chunk_fsyncs": stats.chunk_fsyncs,
        "audio_bytes": stats.audio_bytes,
        "kept_chunks_taken": stats.kept_chunks_taken,
    })
}

/// Takes every chunk closed since the last call back from the writer, as the
/// recorder's live preview does, and checks it is exactly what was appended.
/// Returns the time spent in `take_closed_audio`.
fn take_closed(
    writer: &mut JournalWriter,
    plan: &JournalPlan,
    taken: &mut [Option<u64>; 2],
    count: &mut u64,
    ops: &mut Ops,
) -> Res<u64> {
    let mut spent = 0;
    for track in journal::Track::ALL {
        let t = track.index();
        let Some(last) = writer.cutoff().get(track) else {
            continue;
        };
        let next = taken[t].map_or(0, |s| s + 1);
        for seq in next..=last {
            let (start, frames) = {
                let chunks = &writer.manifest().track(track).chunks;
                let pos = chunks.iter().position(|c| c.seq == seq).ok_or_else(|| {
                    format!(
                        "invariant failed: closed {} chunk {seq} is not listed",
                        track.as_str()
                    )
                })?;
                let start: u64 = chunks[..pos].iter().map(|c| c.frames.unwrap_or(0)).sum();
                (start, chunks[pos].frames.unwrap_or(0))
            };
            let (audio, ns) = timed(|| writer.take_closed_audio(track, seq));
            spent += ns;
            let audio = audio.ok_or_else(|| {
                format!(
                    "invariant failed: closed {} chunk {seq} was not kept",
                    track.as_str()
                )
            })?;
            check(
                audio.len() as u64 == frames
                    && audio
                        .iter()
                        .zip(start..)
                        .all(|(s, f)| s.to_bits() == journal_pcm(plan.seed, track, f).to_bits()),
                || {
                    format!(
                        "taken {} chunk {seq} differs from what was appended",
                        track.as_str()
                    )
                },
            )?;
            ops.add("journal.take_closed_audio", 1, ns);
            *count += 1;
        }
        taken[t] = Some(last);
    }
    Ok(spent)
}

/// Checks the stopped journal on disk: the manifest, the one pause, every
/// chunk's start time on the host and recording timelines, its gap flag and
/// every sample, bit for bit. Returns the chunks listed and how many of them
/// recovery sized.
fn verify_journal(
    dir: &Path,
    plan: &JournalPlan,
    manifest: &SessionManifest,
    end_host: u64,
    ops: &mut Ops,
) -> Res<(usize, usize)> {
    check(
        manifest.state == SessionState::Stopped && manifest.stopped_ns == Some(end_host),
        || {
            format!(
                "journal {:?} at {:?}, expected stopped at {end_host}",
                manifest.state, manifest.stopped_ns
            )
        },
    )?;
    let on_disk = journal::load_manifest(dir)?;
    check(on_disk == *manifest, || {
        "session.json differs from the returned manifest".into()
    })?;
    let pause_host = plan.pause_block() * plan.block_ns();
    let pauses = [PauseSpan {
        from_ns: pause_host,
        to_ns: Some(pause_host + plan.pause_ns()),
    }];
    check(manifest.pauses == pauses, || {
        format!("pauses {:?}, expected {pauses:?}", manifest.pauses)
    })?;
    let map = manifest.time_map();
    let (mut listed, mut recovered) = (0usize, 0usize);
    for track in journal::Track::ALL {
        let rate = plan.rate(track);
        let pause_frame = plan.pause_block() * plan.block_frames(track);
        let mut k = 0u64;
        for (n, chunk) in manifest.track(track).chunks.iter().enumerate() {
            let frames = chunk.frames.ok_or_else(|| {
                format!(
                    "invariant failed: {} chunk {} has no frame count",
                    track.as_str(),
                    chunk.seq
                )
            })?;
            let recording = journal::frames_to_ns(k, rate);
            let host = recording + if k >= pause_frame { plan.pause_ns() } else { 0 };
            check(
                chunk.host_start_ns == host
                    && map.recording_ns(chunk.host_start_ns) == recording
                    && chunk.after_gap == (n > 0 && k == pause_frame),
                || {
                    format!(
                        "{} chunk {} starts at host {} (gap {}), expected {host} (gap {})",
                        track.as_str(),
                        chunk.seq,
                        chunk.host_start_ns,
                        chunk.after_gap,
                        n > 0 && k == pause_frame
                    )
                },
            )?;
            let (samples, ns) = timed(|| journal::read_chunk(dir, track, chunk));
            let samples = samples?;
            ops.add("journal.read_chunk", 1, ns);
            check(
                samples.len() as u64 == frames
                    && samples
                        .iter()
                        .zip(k..)
                        .all(|(s, f)| s.to_bits() == journal_pcm(plan.seed, track, f).to_bits()),
                || {
                    format!(
                        "{} chunk {} differs from what was appended",
                        track.as_str(),
                        chunk.seq
                    )
                },
            )?;
            k += frames;
            listed += 1;
            recovered += usize::from(chunk.recovered);
        }
        let total = plan.blocks() * plan.block_frames(track);
        check(k == total, || {
            format!(
                "{} journal holds {k} frames, expected {total}",
                track.as_str()
            )
        })?;
    }
    Ok((listed, recovered))
}

/// Chunk files on disk under both track folders, and their total size.
fn chunk_files(dir: &Path) -> Res<(u64, u64)> {
    let (mut files, mut bytes) = (0u64, 0u64);
    for track in journal::Track::ALL {
        for entry in fs::read_dir(dir.join(track.as_str()))? {
            let meta = entry?.metadata()?;
            if meta.is_file() {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((files, bytes))
}

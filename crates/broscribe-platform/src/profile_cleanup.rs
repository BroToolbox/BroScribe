//! Destructive profile cleanup shared by the CLI and the Windows helper.
//! The allowlist uses literal paths, with one ASCII identifier wildcard per
//! component, with separate wildcards for numeric IDs and digests. Installed
//! models supply a file inventory. Manual-upgrade bundles are whole trees.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result, paths};
use serde::Deserialize;

use crate::release_layout::is_reparse;

pub const ALLOWLIST: &str = include_str!("profile_cleanup.json");

#[derive(Clone, Deserialize)]
struct Shape {
    files: Vec<String>,
    dirs: Vec<String>,
    trees: Vec<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    atomic: Vec<String>,
    #[serde(default)]
    meetings: Vec<String>,
    #[serde(skip)]
    literal: bool,
    #[serde(skip)]
    meeting_media: Option<String>,
    #[serde(default)]
    library_root: bool,
}

static SHAPES: std::sync::LazyLock<BTreeMap<String, Shape>> = std::sync::LazyLock::new(|| {
    serde_json::from_str(ALLOWLIST).expect("valid profile cleanup shapes")
});

// Each component may have one token. Match the literal prefix and suffix
// separately so case-insensitive paths do not broaden a writer's ID grammar.
const TOKENS: &str = "*#~%@+&!?^=:$;{[]}>";

fn matches_path(pattern: &str, path: &str, ignore_case: bool) -> bool {
    let equal = |a: &str, b: &str| a == b || (ignore_case && a.eq_ignore_ascii_case(b));
    let mut path = path.split('/');
    pattern.split('/').all(|part| {
        let Some(name) = path.next() else {
            return false;
        };
        let Some((offset, wildcard)) = part.char_indices().find(|(_, ch)| TOKENS.contains(*ch))
        else {
            return equal(part, name);
        };
        let prefix = &part[..offset];
        let suffix = &part[offset + 1..];
        let Some((head, rest)) = name.split_at_checked(prefix.len()) else {
            return false;
        };
        let Some((id, tail)) = rest.split_at_checked(rest.len().saturating_sub(suffix.len()))
        else {
            return false;
        };
        if id.is_empty() || !equal(prefix, head) || !equal(suffix, tail) {
            return false;
        }
        let alnum = || id.bytes().all(|b| b.is_ascii_alphanumeric());
        let digits = || id.bytes().all(|b| b.is_ascii_digit());
        let lower_hex = |text: &str, count| {
            text.len() == count && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        match wildcard {
            '@' => {
                !matches!(id, "." | "..")
                    && id.bytes().all(|b| {
                        b.is_ascii_lowercase()
                            || (ignore_case && b.is_ascii_uppercase())
                            || b.is_ascii_digit()
                            || b"._-".contains(&b)
                    })
            }
            '+' => {
                if ignore_case {
                    broscribe_core::templates::validate_id(&id.to_ascii_lowercase()).is_ok()
                } else {
                    broscribe_core::templates::validate_id(id).is_ok()
                }
            }
            '&' => bench_set(id, ignore_case),
            '?' => id.split_once('.').is_some_and(|(sha, token)| {
                sha.len() == 64
                    && token.len() == 32
                    && sha
                        .bytes()
                        .chain(token.bytes())
                        .all(|b| b.is_ascii_hexdigit())
            }),
            '!' => bench_result(id, ignore_case),
            '#' => canonical_generation(id),
            '>' => canonical_pid(id),
            '~' => id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            '%' => id
                .bytes()
                .all(|b| b.is_ascii_digit() || b".-TZ".contains(&b.to_ascii_uppercase())),
            '^' => lower_hex(id, 6),
            '=' => digits() && id.parse::<u64>().is_ok_and(|seq| format!("{seq:06}") == id),
            ':' => id.len() <= 8 && alnum(),
            '$' => id.len() == 6 && alnum(),
            ';' => id.len() == 4 && digits(),
            '{' => id.len() == 2 && digits(),
            '[' => lower_hex(id, 16),
            ']' => id
                .strip_prefix("j-")
                .and_then(|id| id.split_once('-'))
                .is_some_and(|(hex, stamp)| {
                    lower_hex(hex, 16) && stamp.parse::<i64>().is_ok_and(|n| n.to_string() == stamp)
                }),
            '}' => id.split_once('.').is_some_and(|(token, ext)| {
                lower_hex(token, 32)
                    && !ext.is_empty()
                    && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            }),
            _ => id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        }
    }) && path.next().is_none()
}

/// Match the same sequence grammar used by profile cleanup.
pub fn is_journal_chunk(name: &str, ignore_case: bool) -> bool {
    matches_path("=.f32", name, ignore_case)
}

/// Match the updater's process-specific staging filename.
pub fn is_update_runner_staging(name: &str, ignore_case: bool) -> bool {
    matches_path(".broscribe-update-runner.>.tmp", name, ignore_case)
}

fn canonical_pid(name: &str) -> bool {
    name.parse::<u32>().is_ok_and(|pid| pid.to_string() == name)
}

/// The photo cache writer names generation folders with `u64::to_string`.
pub fn canonical_generation(name: &str) -> bool {
    name.parse::<u64>()
        .is_ok_and(|generation| generation.to_string() == name)
}

fn bench_set(id: &str, ignore_case: bool) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "results"
        && !(ignore_case && id.eq_ignore_ascii_case("results"))
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn bench_result(stem: &str, ignore_case: bool) -> bool {
    let Some(prefix) = stem.get(..16) else {
        return false;
    };
    if prefix.as_bytes()[8].eq_ignore_ascii_case(&b'T')
        && prefix.ends_with('-')
        && prefix.as_bytes()[..8]
            .iter()
            .chain(&prefix.as_bytes()[9..15])
            .all(u8::is_ascii_digit)
    {
        let set = &stem[16..];
        return bench_set(set, ignore_case)
            || set.rsplit_once('-').is_some_and(|(name, n)| {
                bench_set(name, ignore_case)
                    && n.parse::<u32>().is_ok_and(|n| n >= 2)
                    && !n.starts_with(['0', '+'])
            });
    }
    false
}

fn matches_literal(path: &str, literal: &str, ignore_case: bool) -> bool {
    path == literal || (ignore_case && path.eq_ignore_ascii_case(literal))
}

fn atomic_target(key: &str) -> Option<String> {
    let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
    let (target, _) = broscribe_core::fsutil::atomic_temp_target(name)?;
    Some(if parent.is_empty() {
        target.to_owned()
    } else {
        format!("{parent}/{target}")
    })
}

fn meeting_record(root: &Path) -> Option<broscribe_core::schema::Meeting> {
    let json = root.join(paths::MEETING_JSON);
    if !regular_path(&json, false) {
        return None;
    }
    broscribe_core::fsutil::read_json(&json)
        .and_then(broscribe_core::schema::migrate_meeting)
        .ok()
}

fn markdown_identity(root: &Path) -> bool {
    let md = root.join(paths::MEETING_MD);
    regular_path(&md, false)
        && broscribe_core::render::read_meeting_md_identity(&md).is_ok_and(|id| id.is_some())
}

fn meeting_identity(root: &Path) -> bool {
    meeting_record(root).is_some() || markdown_identity(root)
}

fn meeting_media(meeting: &broscribe_core::schema::Meeting) -> Option<&str> {
    let recording = meeting.recording.as_ref()?;
    if recording.handling == broscribe_core::schema::Handling::Reference {
        return None;
    }
    let name = recording.path.to_str()?;
    let extension = name.strip_prefix("audio.")?;
    (!extension.is_empty() && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then_some(name)
}

fn regular_path(path: &Path, dir: bool) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| !is_reparse(&meta) && if dir { meta.is_dir() } else { meta.is_file() })
}

pub fn library_evidence(root: &Path, require_meeting_identity: bool) -> Result<bool> {
    let children = |path: &Path| -> Result<Vec<PathBuf>> {
        std::fs::read_dir(path)
            .map_err(|error| Error::io_at(path, error))?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|error| Error::io_at(path, error))
            })
            .collect()
    };
    let digits = |path: &Path, count| {
        path.file_name().is_some_and(|name| {
            let name = name.as_encoded_bytes();
            name.len() == count && name.iter().all(u8::is_ascii_digit)
        })
    };
    if !regular_path(root, true) {
        return Ok(false);
    }
    let index = root.join(paths::INDEX_DIR);
    if regular_path(&index, true) && regular_path(&root.join(paths::INDEX_DB), false) {
        return Ok(true);
    }
    for year in children(root)?
        .into_iter()
        .filter(|p| digits(p, 4) && regular_path(p, true))
    {
        for month in children(&year)?
            .into_iter()
            .filter(|p| digits(p, 2) && regular_path(p, true))
        {
            if children(&month)?.into_iter().any(|meeting| {
                regular_path(&meeting, true)
                    && if require_meeting_identity {
                        meeting_identity(&meeting)
                    } else {
                        regular_path(&meeting.join(paths::MEETING_JSON), false)
                            || regular_path(&meeting.join(paths::MEETING_MD), false)
                    }
            }) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// A library opened for writing creates these three directories even before
/// its first meeting or index write. Recognize only a wholly empty skeleton.
pub fn empty_library_skeleton(root: &Path) -> Result<bool> {
    use std::io::ErrorKind::NotFound;
    if !std::fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir() && !is_reparse(&meta)) {
        return Ok(false);
    }
    let mut entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == NotFound => return Ok(false),
        Err(error) => return Err(Error::io_at(root, error)),
    };
    let mut count = 0;
    for entry in &mut entries {
        let entry = entry.map_err(|error| Error::io_at(root, error))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|error| Error::io_at(&path, error))?;
        if !meta.is_dir()
            || is_reparse(&meta)
            || ![paths::PEOPLE_DIR, paths::QUEUE_DIR, paths::INDEX_DIR]
                .iter()
                .any(|name| entry.file_name() == *name)
            || std::fs::read_dir(&path)
                .map_err(|error| Error::io_at(&path, error))?
                .next()
                .transpose()
                .map_err(|error| Error::io_at(&path, error))?
                .is_some()
        {
            return Ok(false);
        }
        count += 1;
    }
    Ok(count == 3)
}

fn refuse_library(path: &Path) -> Result<()> {
    // Evidence for protecting a possible library can be weaker than the
    // evidence for recursively deleting one on the user's request.
    let library_skeleton = [paths::INDEX_DIR, paths::PEOPLE_DIR, paths::QUEUE_DIR]
        .iter()
        .all(|name| {
            std::fs::symlink_metadata(path.join(name))
                .is_ok_and(|meta| meta.is_dir() && !is_reparse(&meta))
        });
    if library_skeleton || library_evidence(path, false)? {
        return Err(Error::InvalidInput(format!(
            "The removal path contains a BroScribe library at {}. Move it out or remove that library explicitly.",
            path.display()
        )));
    }
    Ok(())
}

fn scan_tree(path: &Path) -> Result<()> {
    refuse_library(path)?;
    for entry in std::fs::read_dir(path).map_err(|error| Error::io_at(path, error))? {
        let entry = entry.map_err(|error| Error::io_at(path, error))?;
        let meta = std::fs::symlink_metadata(entry.path())
            .map_err(|error| Error::io_at(&entry.path(), error))?;
        if meta.is_dir() && !is_reparse(&meta) {
            scan_tree(&entry.path())?;
        }
    }
    Ok(())
}

// Model cleanup uses writer-produced inventories for installed files and
// in-progress pulls. Unknown contents of a legacy .partial stay in place.
pub const MODEL_PARTIAL_RECORD: &str = ".entries.json";

#[derive(Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPartialEntries {
    pub id: String,
    pub files: Vec<String>,
    pub dirs: Vec<String>,
}

impl ModelPartialEntries {
    pub fn save(&self, partial: &Path) -> Result<()> {
        broscribe_core::fsutil::write_json_atomic(&partial.join(MODEL_PARTIAL_RECORD), self)
    }
}

fn model_shape(root: &Path) -> Option<Shape> {
    #[derive(Deserialize)]
    struct Marker {
        id: String,
        files: Vec<ModelFile>,
        #[serde(default)]
        sources: Vec<ModelSource>,
    }
    #[derive(Deserialize)]
    struct ModelFile {
        path: String,
    }
    #[derive(Deserialize)]
    struct ModelSource {
        file: String,
    }
    fn read_marker<T: serde::de::DeserializeOwned>(marker: &Path) -> Option<T> {
        const LIMIT: u64 = 4 * 1024 * 1024;
        let meta = std::fs::symlink_metadata(marker).ok()?;
        if !meta.is_file() || is_reparse(&meta) || meta.len() > LIMIT {
            return None;
        }
        let file = std::fs::File::open(marker).ok()?;
        let bytes = std::io::read_to_string(std::io::Read::take(file, LIMIT + 1)).ok()?;
        if bytes.len() as u64 > LIMIT {
            return None;
        }
        serde_json::from_str(&bytes).ok()
    }
    fn safe(path: &str) -> bool {
        !path
            .split('/')
            .any(|part| matches!(part, "" | "." | "..") || part.contains('\\'))
    }
    fn add(shape: &mut Shape, relative: String, is_dir: bool) {
        shape.dirs.extend(
            relative
                .match_indices('/')
                .map(|(index, _)| relative[..index].to_owned()),
        );
        if is_dir {
            shape.dirs.push(relative);
        } else {
            shape.files.push(relative);
        }
    }

    let id = root.file_name()?.to_str()?;
    let mut shape = Shape {
        files: Vec::new(),
        dirs: vec![".partial".into()],
        trees: Vec::new(),
        models: Vec::new(),
        meetings: Vec::new(),
        atomic: vec![
            ".verified".into(),
            format!(".partial/{MODEL_PARTIAL_RECORD}"),
        ],
        literal: true,
        meeting_media: None,
        library_root: false,
    };
    if let Some(marker) = read_marker::<Marker>(&root.join(".verified")).filter(|record| {
        record.id == id
            && record.files.iter().all(|file| safe(&file.path))
            && record.sources.iter().all(|source| safe(&source.file))
    }) {
        shape.files.push(".verified".into());
        for ModelFile { path } in marker.files {
            add(&mut shape, format!(".partial/{path}"), false);
            add(&mut shape, path, false);
        }
        for ModelSource { file } in marker.sources {
            add(&mut shape, format!(".partial/{file}"), false);
        }
    }
    let partial = root.join(".partial");
    if std::fs::symlink_metadata(&partial).is_ok_and(|meta| meta.is_dir() && !is_reparse(&meta))
        && let Some(entries) = read_marker::<ModelPartialEntries>(
            &partial.join(MODEL_PARTIAL_RECORD),
        )
        .filter(|record| {
            record.id == id
                && record
                    .files
                    .iter()
                    .chain(&record.dirs)
                    .all(|path| safe(path))
        })
    {
        shape.files.push(format!(".partial/{MODEL_PARTIAL_RECORD}"));
        for path in entries.files {
            add(&mut shape, format!(".partial/{path}"), false);
            add(&mut shape, path, false);
        }
        for path in entries.dirs {
            add(&mut shape, format!(".partial/{path}"), true);
            add(&mut shape, path, true);
        }
    }
    Some(shape)
}

#[derive(Clone, Copy)]
enum Task {
    File { socket: bool },
    EmptyDir,
    Tree,
}

#[derive(Default)]
pub struct Cleanup {
    pub changed: bool,
    pub kept: Vec<PathBuf>,
}

/// Plan every removal before deleting anything. Unknown entries are left
/// untouched. The caller handles profile identity, overlap and admission.
pub fn clean(
    root: &Path,
    kind: &str,
    apply: bool,
    skip: &[&str],
    ignore_case: bool,
) -> Result<Cleanup> {
    let mut plan = plan_cleanup(root, &SHAPES[kind], skip, kind == "run", ignore_case)?;
    if apply {
        plan.1
            .sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    }
    finish_cleanup(root, plan, apply)
}

/// The model operations that share the cache ownership inventory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModelCleanup {
    Inspect,
    Remove,
    RestartDownload,
    InstallDownload,
}

/// Plans model cache changes using the install and staging inventories.
/// A download refuses unknown entries before changing anything. Removing a
/// download leaves unknown entries alone. Callers serialize writes per model.
pub fn clean_model(
    root: &Path,
    loose_files: impl IntoIterator<Item = String>,
    mode: ModelCleanup,
) -> Result<(bool, Cleanup)> {
    let Some(mut shape) = model_shape(root) else {
        return Ok((false, Cleanup::default()));
    };
    // The downloader reserves this filename, including a damaged marker.
    shape.files.push(".verified".into());
    for file in loose_files {
        shape.files.push(format!(".partial/{file}"));
        shape.files.push(file);
    }
    let mut plan = plan_cleanup(root, &shape, &[], false, cfg!(windows))?;
    let has_files = plan
        .1
        .iter()
        .any(|(_, task)| matches!(task, Task::File { .. }));
    if mode == ModelCleanup::Inspect || (mode == ModelCleanup::Remove && !has_files) {
        return Ok((has_files, plan.0));
    }
    if mode != ModelCleanup::Remove
        && let Some(path) = plan.0.kept.first()
    {
        let relative = path.strip_prefix(root).unwrap_or(Path::new(""));
        let location = if relative.as_os_str().is_empty() {
            "the model folder".into()
        } else {
            format!("the model folder ({})", relative.display())
        };
        return Err(Error::Service {
            code: "model_cache_has_other_files",
            message: format!(
                "BroScribe found an unrecognised entry in {location}. Move it out before downloading again."
            ),
        });
    }

    let staging = root.join(".partial");
    match mode {
        ModelCleanup::RestartDownload => plan.1.retain(|(path, _)| path.starts_with(&staging)),
        ModelCleanup::InstallDownload => plan.1.retain(|(path, _)| !path.starts_with(&staging)),
        _ => {}
    }
    // Keep ownership records until their payloads are gone, including when
    // a locked file causes removal to be retried.
    plan.1.sort_by_key(|(path, task)| match task {
        Task::EmptyDir => (2, std::cmp::Reverse(path.components().count())),
        Task::File { .. }
            if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some(".verified" | MODEL_PARTIAL_RECORD)
            ) =>
        {
            (1, std::cmp::Reverse(0))
        }
        _ => (0, std::cmp::Reverse(0)),
    });
    finish_cleanup(root, plan, true).map(|report| (has_files, report))
}

type CleanupPlan = (Cleanup, Vec<(PathBuf, Task)>);

fn plan_cleanup(
    root: &Path,
    shape: &Shape,
    skip: &[&str],
    allow_sockets: bool,
    ignore_case: bool,
) -> Result<CleanupPlan> {
    let mut cleanup = Cleanup::default();
    let mut tasks = Vec::new();
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((cleanup, tasks)),
        Err(error) => return Err(Error::io_at(root, error)),
    };
    if !meta.is_dir() || is_reparse(&meta) {
        cleanup.kept.push(root.to_path_buf());
        return Ok((cleanup, tasks));
    }
    plan(
        root,
        allow_sockets,
        shape,
        skip,
        ignore_case,
        &mut cleanup,
        &mut tasks,
    )?;
    Ok((cleanup, tasks))
}

fn finish_cleanup(root: &Path, (mut cleanup, tasks): CleanupPlan, apply: bool) -> Result<Cleanup> {
    if apply {
        for (path, task) in tasks {
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(Error::io_at(&path, error)),
            };
            let parent_changed = path
                .ancestors()
                .skip(1)
                .take_while(|part| part.starts_with(root))
                .any(|parent| {
                    !std::fs::symlink_metadata(parent)
                        .is_ok_and(|meta| meta.is_dir() && !is_reparse(&meta))
                });
            if is_reparse(&meta) || parent_changed {
                cleanup.kept.push(path);
                continue;
            }
            let result = match task {
                Task::Tree if meta.is_dir() => {
                    scan_tree(&path)?;
                    crate::release_layout::ReleaseLayout::Macos.validate_partial_archive(&path)?;
                    std::fs::remove_dir_all(&path)
                }
                Task::EmptyDir if meta.is_dir() => std::fs::remove_dir(&path),
                Task::File { socket } if permitted_leaf(&meta, socket) => {
                    std::fs::remove_file(&path)
                }
                _ => {
                    cleanup.kept.push(path);
                    continue;
                }
            };
            match result {
                Ok(()) => cleanup.changed = true,
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(Error::io_at(&path, error)),
            }
        }
    }
    cleanup.kept.sort();
    Ok(cleanup)
}

/// The platform's separately located agent and UI sockets are removable
/// only while they are sockets. A file at the same name is kept.
pub fn clean_socket(path: &Path) -> Result<Cleanup> {
    let mut cleanup = Cleanup::default();
    match std::fs::symlink_metadata(path) {
        Ok(meta) if permitted_leaf(&meta, true) && !is_reparse(&meta) => {
            match std::fs::remove_file(path) {
                Ok(()) => cleanup.changed = true,
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(Error::io_at(path, error)),
            }
        }
        Ok(_) => cleanup.kept.push(path.to_path_buf()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(Error::io_at(path, error)),
    }
    Ok(cleanup)
}

fn permitted_leaf(meta: &std::fs::Metadata, allow_socket: bool) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if allow_socket {
            return meta.file_type().is_socket();
        }
    }
    #[cfg(windows)]
    if allow_socket {
        return false;
    }
    meta.is_file()
}

fn plan(
    root: &Path,
    allow_sockets: bool,
    shape: &Shape,
    skip: &[&str],
    ignore_case: bool,
    cleanup: &mut Cleanup,
    tasks: &mut Vec<(PathBuf, Task)>,
) -> Result<()> {
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((folder, relative)) = pending.pop() {
        if !shape.library_root || folder != root {
            refuse_library(&folder)?;
        }
        for entry in std::fs::read_dir(&folder).map_err(|error| Error::io_at(&folder, error))? {
            let entry = entry.map_err(|error| Error::io_at(&folder, error))?;
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                cleanup.kept.push(path);
                continue;
            };
            if relative.is_empty()
                && skip
                    .iter()
                    .any(|part| *part == name || (ignore_case && part.eq_ignore_ascii_case(name)))
            {
                continue;
            }
            let key = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            let has = |rules: &[String], key: &str| {
                rules.iter().any(|pat| {
                    if shape.literal {
                        matches_literal(key, pat, ignore_case)
                    } else {
                        matches_path(pat, key, ignore_case)
                    }
                })
            };
            let meta =
                std::fs::symlink_metadata(&path).map_err(|error| Error::io_at(&path, error))?;
            let socket = allow_sockets && key.ends_with(".sock");
            let regular_file = permitted_leaf(&meta, socket);
            if !is_reparse(&meta) {
                if regular_file
                    && (shape.meeting_media.as_deref().is_some_and(|name| {
                        name == key || (ignore_case && name.eq_ignore_ascii_case(&key))
                    }) || has(&shape.files, &key)
                        || atomic_target(&key).is_some_and(|target| has(&shape.atomic, &target)))
                {
                    tasks.push((path, Task::File { socket }));
                    continue;
                }
                if meta.is_dir()
                    && has(&shape.models, &key)
                    && let Some(model) = model_shape(&path)
                {
                    plan(&path, false, &model, &[], ignore_case, cleanup, tasks)?;
                    continue;
                }
                if meta.is_dir() && has(&shape.meetings, &key) {
                    let record = meeting_record(&path);
                    if record.is_some() || markdown_identity(&path) {
                        let mut meeting_shape = SHAPES["meeting"].clone();
                        meeting_shape.meeting_media =
                            record.as_ref().and_then(meeting_media).map(str::to_owned);
                        plan(
                            &path,
                            false,
                            &meeting_shape,
                            &[],
                            ignore_case,
                            cleanup,
                            tasks,
                        )?;
                    } else {
                        list_unrecognized_meeting(&path, &mut cleanup.kept)?;
                    }
                    continue;
                }
                if meta.is_dir() && has(&shape.trees, &key) {
                    scan_tree(&path)?;
                    crate::release_layout::ReleaseLayout::Macos.validate_partial_archive(&path)?;
                    tasks.push((path, Task::Tree));
                    continue;
                }
                if meta.is_dir() && has(&shape.dirs, &key) {
                    pending.push((path, key));
                    continue;
                }
            }
            cleanup.kept.push(path);
        }
        // Empty directories are removed only after their children.
        tasks.push((folder, Task::EmptyDir));
    }
    Ok(())
}

fn list_unrecognized_meeting(root: &Path, kept: &mut Vec<PathBuf>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if regular_path(&path, true) {
            for entry in std::fs::read_dir(&path).map_err(|error| Error::io_at(&path, error))? {
                pending.push(entry.map_err(|error| Error::io_at(&path, error))?.path());
            }
        }
        kept.push(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::{
        config::CONFIG_FILE, lifecycle::JOURNAL_DIR, maintenance::PROFILE_MARKER,
        retention::PLAYBACK_CACHE_DIR,
    };

    #[test]
    fn interrupted_model_promotion_uses_partial_inventory_at_model_root() {
        let root = tempfile::tempdir().unwrap();
        let model = root.path().join("models/synthetic.model");
        let partial = model.join(".partial");
        let original = partial.join("assets/model.onnx");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"synthetic model").unwrap();
        ModelPartialEntries {
            id: "synthetic.model".into(),
            files: vec!["assets/model.onnx".into()],
            dirs: vec!["assets".into()],
        }
        .save(&partial)
        .unwrap();
        std::fs::rename(partial.join("assets"), model.join("assets")).unwrap();
        let keep = model.join("assets/family.txt");
        std::fs::write(&keep, b"synthetic user file").unwrap();
        let cleanup = clean(root.path(), "data", true, &[], false).unwrap();
        assert!(cleanup.changed);
        assert_eq!(cleanup.kept, std::slice::from_ref(&keep));
        assert!(!partial.exists() && !model.join("assets/model.onnx").exists());
        assert_eq!(std::fs::read(keep).unwrap(), b"synthetic user file");
    }

    #[test]
    fn cleanup_names_follow_profile_writers() {
        let data = &SHAPES["data"];
        assert!(data.files.iter().any(|file| file == CONFIG_FILE));
        assert!(data.files.iter().any(|file| file == PROFILE_MARKER));
        assert!(data.dirs.iter().any(|dir| dir == JOURNAL_DIR));
        assert!(
            SHAPES["cache"]
                .dirs
                .iter()
                .any(|dir| dir == PLAYBACK_CACHE_DIR)
        );
    }

    #[cfg(unix)]
    #[test]
    fn separate_runtime_cleanup_removes_sockets_and_keeps_a_regular_app_sock() {
        use std::os::unix::net::UnixListener;
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let app = root.path().join("app.sock");
        let agent = root.path().join("broscribed.sock");
        let listener = UnixListener::bind(&agent).unwrap();
        std::fs::write(&app, b"synthetic user file").unwrap();
        let kept = clean_socket(&app).unwrap();
        assert!(!kept.changed && kept.kept == vec![app.clone()]);
        assert!(clean_socket(&agent).unwrap().changed);
        assert!(!agent.exists());
        drop(listener);
        assert_eq!(std::fs::read(app).unwrap(), b"synthetic user file");
    }

    #[test]
    fn recursive_backup_cleanup_refuses_a_meeting_library() {
        let root = tempfile::tempdir().unwrap();
        let backup = root.path().join("BroScribe-1.0.0-42-20261126T010000Z.app");
        let meetings = backup.join("Contents/2024/11/synthetic");
        std::fs::create_dir_all(&meetings).unwrap();
        std::fs::write(meetings.join(paths::MEETING_JSON), "{}").unwrap();
        let result = clean(root.path(), "app-backups", true, &[], false);
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("contains a BroScribe library")
        );
        assert!(meetings.join(paths::MEETING_JSON).exists());
    }
}

#[cfg(test)]
mod parity {
    use super::*;
    use serde_json::{Value, json};

    fn sample(pattern: &str) -> String {
        pattern
            .chars()
            .map(|token| match token {
                '*' => "synthetic-1".into(),
                '#' | '>' => "42".into(),
                '~' => "a".repeat(64),
                '%' => "1.0.0-42-20261126T010000Z".into(),
                '@' => "synthetic.model-1".into(),
                '+' | '&' => "synthetic".into(),
                '!' => "20261126T010000-synthetic".into(),
                '?' => format!("{}.{}", "a".repeat(64), "b".repeat(32)),
                '^' => "12abef".into(),
                '=' => "000042".into(),
                ':' => "aiff".into(),
                '$' => "A0b1C2".into(),
                ';' => "2026".into(),
                '{' => "11".into(),
                '[' => "1234abcdef123456".into(),
                ']' => "j-1234abcdef123456-1795700000000".into(),
                '}' => format!("{}.wav", "b".repeat(32)),
                c => c.to_string(),
            })
            .collect()
    }

    fn orphan(path: &str) -> String {
        if let Some((parent, leaf)) = path.rsplit_once('/') {
            format!("{parent}/.{leaf}.tmp-42-12abcdef")
        } else {
            format!(".{path}.tmp-42-12abcdef")
        }
    }

    fn row(shape: &str, path: &str, kind: &str, pattern: Option<&str>, expected: bool) -> Value {
        let rules = &SHAPES[shape];
        let has = |patterns: &[String], path: &str| {
            patterns
                .iter()
                .any(|pattern| matches_path(pattern, path, true))
        };
        let actual = match kind {
            "file" => {
                has(&rules.files, path)
                    || atomic_target(path).is_some_and(|target| has(&rules.atomic, &target))
            }
            "dir" => has(&rules.dirs, path),
            "trees" => has(&rules.trees, path),
            "models" => has(&rules.models, path),
            "meetings" => has(&rules.meetings, path),
            "pattern" => matches_path(pattern.unwrap(), path, true),
            "literal" => matches_literal(path, pattern.unwrap(), true),
            _ => unreachable!(),
        };
        assert_eq!(actual, expected, "{shape}/{path} ({kind}, {pattern:?})");
        json!({"shape": shape, "path": path, "type": kind, "pattern": pattern, "matches": actual})
    }

    #[test]
    fn windows_cleanup_fixture_matches_rust() {
        let mut cases = Vec::new();
        for (kind, shape) in SHAPES.iter() {
            for (rules, entry_type) in [
                (&shape.files, "file"),
                (&shape.dirs, "dir"),
                (&shape.trees, "trees"),
                (&shape.models, "models"),
                (&shape.meetings, "meetings"),
                (&shape.atomic, "atomic"),
            ] {
                for pattern in rules {
                    let positive = sample(pattern);
                    cases.push(row(kind, &positive, "pattern", Some(pattern), true));
                    cases.push(row(
                        kind,
                        &format!("{positive}/extra"),
                        "pattern",
                        Some(pattern),
                        false,
                    ));
                    if entry_type == "atomic" {
                        let path = orphan(&positive);
                        cases.push(row(kind, &path, "file", None, true));
                        cases.push(row(kind, &format!("{path}-personal"), "file", None, false));
                    } else {
                        cases.push(row(kind, &positive, entry_type, None, true));
                    }
                }
            }
        }
        for (shape, name, kind, expected) in [
            (
                "data",
                "integrations/google/photos/p-family.img",
                "file",
                false,
            ),
            (
                "data",
                "integrations/google/photos/3/p-AB1234.img",
                "file",
                false,
            ),
            (
                "data",
                "integrations/google/photos/3/.p-family.img.tmp-42-12abcdef",
                "file",
                false,
            ),
            ("data", "logs/broscribed.log.bak", "file", false),
            ("data", "logs/broscribe.log.old", "file", false),
            ("data", "journal/synthetic/mic/notes.f32", "file", false),
            (
                "data",
                "journal/synthetic/system/0000001.f32",
                "file",
                false,
            ),
            (
                "data",
                "journal/synthetic/system/18446744073709551615.f32",
                "file",
                true,
            ),
            (
                "data",
                "journal/synthetic/system/18446744073709551616.f32",
                "file",
                false,
            ),
            ("data", "models/..", "dir", false),
            ("data", "models/...lock", "file", false),
            ("data", "bench/results/set.json", "file", false),
            ("data", "bench/RESULTS/set.json", "file", false),
            ("cache", "audio-check-personal.wav", "file", false),
            (
                "cache",
                "setup-capture/synthetic/broscribe-voice-1/try-input.M4A",
                "file",
                true,
            ),
            (
                "cache",
                "setup-capture/synthetic/broscribe-voice-1/try-input.aiff-notes",
                "file",
                false,
            ),
            ("meeting", "templates/notes.txt", "file", false),
            ("library", "2026/11/planning/meeting.md", "file", false),
            ("library", "people/p-family/person.md", "file", false),
        ] {
            cases.push(row(shape, name, kind, None, expected));
        }
        for (pattern, value) in [
            ("~/sample", format!("{}/sample", "a".repeat(63))),
            (
                "?.zip",
                format!("{}.{}.zip", "a".repeat(64), "b".repeat(31)),
            ),
            (
                "]/job.json",
                "j-1234abcdef123456-01795700000000/job.json".into(),
            ),
            ("audio.}", format!("audio.{}.wav", "B".repeat(32))),
            ("batch-[.json", "batch-1234Abcdef123456.json".into()),
            ("try-input.:", "try-input.aiffnotes".into()),
            ("audio-check-$.wav", "audio-check-A0b1C.wav".into()),
            (";/11", "20260/11".into()),
            ("2026/{", "2026/1".into()),
            (
                "BroScribe-%.app",
                "BroScribe-1.0.0-42-20261126T010000Y.app".into(),
            ),
            ("photos/#", "photos/007".into()),
            ("photos/#", "photos/18446744073709551616".into()),
        ] {
            cases.push(row("data", &value, "pattern", Some(pattern), false));
        }
        for (path, expected) in [
            ("Assets/Éxample.onnx", true),
            ("Assets/éxample.onnx", false),
        ] {
            cases.push(row(
                "data",
                path,
                "literal",
                Some("assets/Éxample.onnx"),
                expected,
            ));
        }
        for (path, kind) in [
            ("integrations/google/photos/007/p-123abc.img", "file"),
            ("integrations/google/photos/007", "dir"),
            ("integrations/google/photos/+7", "dir"),
        ] {
            cases.push(row("data", path, kind, None, false));
        }
        for (pid, expected) in [("4294967295", true), ("4294967296", false), ("042", false)] {
            cases.push(row(
                "update-runner",
                &format!(".broscribe-update-runner.{pid}.tmp"),
                "file",
                None,
                expected,
            ));
        }
        cases.push(row("meeting", "audio.txt", "file", None, false));
        cases.push(row(
            "library",
            ".queue/batch-personal.json",
            "file",
            None,
            false,
        ));
        for (suffix, expected) in [
            ("-1", false),
            ("-2", true),
            ("-02", false),
            ("-4294967295", true),
            ("-4294967296", false),
        ] {
            let path = format!(
                "bench/results/20261126T010000-{}{suffix}.json",
                "a".repeat(64)
            );
            cases.push(row("data", &path, "file", None, expected));
        }

        let sample = tempfile::tempdir().unwrap();
        let mut evidence = Vec::new();
        for (name, dirs, files, detected) in [
            (
                "year-only",
                &["2026/11/planning"][..],
                &["2026/11/planning/notes.txt"][..],
                false,
            ),
            (
                "incomplete-skeleton",
                &[".index", "people"][..],
                &[][..],
                false,
            ),
            (
                "empty-skeleton",
                &[".index", "people", ".queue"][..],
                &[][..],
                true,
            ),
            ("index", &[][..], &[".index/broscribe.sqlite"][..], true),
            (
                "markdown",
                &[][..],
                &["2026/11/synthetic/meeting.md"][..],
                true,
            ),
            (
                "json",
                &[][..],
                &["2026/11/synthetic/meeting.json"][..],
                true,
            ),
            (
                "wrong-month",
                &[][..],
                &["2026/3/synthetic/meeting.json"][..],
                false,
            ),
            (
                "directory-not-file",
                &["2026/11/synthetic/meeting.json"][..],
                &[][..],
                false,
            ),
        ] {
            let root = sample.path().join(name);
            std::fs::create_dir(&root).unwrap();
            for dir in dirs {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            for file in files {
                let path = root.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"synthetic file").unwrap();
            }
            assert_eq!(refuse_library(&root).is_err(), detected, "{name}");
            evidence.push(json!({"name":name, "dirs":dirs, "files":files, "detected":detected}));
        }
        let rows = |values: &[Value]| {
            values
                .iter()
                .map(|value| {
                    // Workspace features may enable serde_json's insertion order.
                    let fields: BTreeMap<_, _> = value.as_object().unwrap().iter().collect();
                    format!("    {}", serde_json::to_string(&fields).unwrap())
                })
                .collect::<Vec<_>>()
                .join(",\n")
        };
        let expected = format!(
            "{{\n  \"cases\": [\n{}\n  ],\n  \"evidence\": [\n{}\n  ]\n}}\n",
            rows(&cases),
            rows(&evidence)
        );
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../broscribe-platform-windows/tests/fixtures/cleanup-cases.json");
        if std::env::var_os("BROSCRIBE_REFRESH_CLEANUP_CASES").is_some() {
            std::fs::write(&fixture, &expected).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&fixture).unwrap(),
            expected,
            "regenerate with BROSCRIBE_REFRESH_CLEANUP_CASES=1 cargo test -p broscribe-platform windows_cleanup_fixture_matches_rust"
        );
    }
}

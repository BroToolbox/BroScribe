//! Atomic file writes: temp file in the same folder, fsync, rename, fsync folder.
//!
//! Temporary files are named `.<target>.tmp-<pid>-<8 hex>` beside their
//! target; [`atomic_temp_target`] recognises exactly that form, so
//! [`orphan_temps`] selects (and [`sweep_orphan_temps`] removes) only files
//! these writes left behind.

#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::process::ProcessProbe;
use crate::{Error, Result};

const TEMP_MARK: &str = ".tmp-";
const TEMP_HEX: usize = 8;

/// Replaces `path` with `bytes` so readers see either the old or the new file.
/// The parent folder must exist.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, bytes, None)
}

/// [`write_atomic`] with the new file created with Unix permission bits
/// `mode` (e.g. `0o600` for files that hold a transcript); `None` keeps the
/// process default. The mode is set when the temporary file is created, so
/// the content is never readable with wider permissions. The requested mode
/// is restored on the open file if umask removed any of its bits. Ignored on
/// platforms without Unix permissions.
pub fn write_atomic_with_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    replace(path, bytes, mode, true)
}

/// [`write_atomic`] whose rename is not made durable: the new content is
/// synced before the rename, so a crash leaves the old or the new file whole,
/// but after a power loss the old one may come back. For records where the
/// previous version is as good after a crash (lease renewals).
pub(crate) fn write_atomic_volatile(path: &Path, bytes: &[u8]) -> Result<()> {
    replace(path, bytes, None, false)
}

fn replace(path: &Path, bytes: &[u8], mode: Option<u32>, durable: bool) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::InvalidInput(format!("not a file path: {}", path.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(
        "{TEMP_MARK}{}-{:0width$x}",
        std::process::id(),
        rand::random::<u32>(),
        width = TEMP_HEX
    ));
    let tmp = dir.join(tmp_name);

    let write = || -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = options.open(&tmp)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt as _;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(Error::io_at(path, e));
    }
    if durable {
        sync_dir(dir)?;
    }
    Ok(())
}

/// Syncs folder entries where the platform supports it. Unix filesystems
/// that refuse directory fsync are tolerated; other sync errors propagate.
/// Windows has no directory-fsync operation here. Callers must sync file
/// contents separately; this does not promise directory-entry persistence
/// through power loss on platforms that cannot perform the directory sync.
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let refused = |e: &std::io::Error| {
            matches!(
                e.kind(),
                ErrorKind::Unsupported | ErrorKind::InvalidInput | ErrorKind::PermissionDenied
            )
        };
        match File::open(dir).and_then(|d| d.sync_all()) {
            Ok(()) => Ok(()),
            Err(e) if refused(&e) => Ok(()),
            Err(e) => Err(Error::io_at(dir, e)),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Creates folder `dir` and missing ancestors, requesting a parent sync for
/// each new folder through [`sync_dir`]. Directory-entry durability has that
/// function's platform limits. Returns whether `dir` was created.
pub fn create_dir_durable(dir: &Path) -> Result<bool> {
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut cur = Some(dir);
    while let Some(p) = cur.filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(p) {
            Ok(_) => break,
            Err(e) if e.kind() == ErrorKind::NotFound => missing.push(p.to_path_buf()),
            Err(e) => return Err(Error::io_at(p, e)),
        }
        cur = p.parent();
    }
    if missing.is_empty() {
        return Ok(false);
    }
    fs::create_dir_all(dir).map_err(|e| Error::io_at(dir, e))?;
    // Deepest first: each new folder's entry lives in its parent.
    for created in &missing {
        if let Some(parent) = created.parent().filter(|p| !p.as_os_str().is_empty()) {
            sync_dir(parent)?;
        }
    }
    Ok(true)
}

/// Pretty JSON with a trailing newline, written atomically.
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| Error::Internal(format!("serialising {}: {e}", path.display())))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

/// Reads and parses a JSON file; a missing file is `Error::NotFound`.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(Error::NotFound(path.display().to_string()));
        }
        Err(e) => return Err(Error::io_at(path, e)),
    };
    serde_json::from_slice(&bytes).map_err(|e| Error::Schema(format!("{}: {e}", path.display())))
}

/// The target file name and writer process ID of a temporary file the
/// atomic writes above create (`.<target>.tmp-<pid>-<8 lowercase hex>`);
/// `None` for any other name.
pub fn atomic_temp_target(file_name: &str) -> Option<(&str, u32)> {
    let rest = file_name.strip_prefix('.')?;
    let (target, tail) = rest.rsplit_once(TEMP_MARK)?;
    let (pid, hex) = tail.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if target.is_empty()
        || !digits(pid)
        || hex.len() != TEMP_HEX
        || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    Some((target, pid.parse().ok()?))
}

/// The temporary files of interrupted atomic writes directly in `dir` (not
/// below it), read only: regular files whose name [`atomic_temp_target`]
/// accepts, written by a process other than this one that is no longer
/// alive, and last modified at least `min_age` ago (a library on a shared
/// folder may hold another machine's write in progress). Sorted; a missing
/// `dir` has none. The one rule [`sweep_orphan_temps`] removes by.
pub fn orphan_temps(
    dir: &Path,
    probe: &dyn ProcessProbe,
    min_age: Duration,
) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(dir, e)),
    };
    let me = probe.current_pid();
    let now = SystemTime::now();
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(dir, e))?;
        let name = entry.file_name();
        let Some((_, pid)) = name.to_str().and_then(atomic_temp_target) else {
            continue;
        };
        if pid == me || probe.is_alive(pid) {
            continue;
        }
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::io_at(&path, e)),
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= min_age);
        if meta.file_type().is_file() && old {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Removes the files [`orphan_temps`] selects in `dir`; returns how many
/// were removed. Run it under the lock that guards the writes into `dir`.
pub fn sweep_orphan_temps(
    dir: &Path,
    probe: &dyn ProcessProbe,
    min_age: Duration,
) -> Result<usize> {
    let mut removed = 0;
    for path in orphan_temps(dir, probe, min_age)? {
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io_at(&path, e)),
        }
    }
    Ok(removed)
}

/// Total bytes of regular files under a directory, without following symlinks.
pub fn directory_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    let mut dirs = vec![path.to_owned()];
    while let Some(path) = dirs.pop() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

/// The example audio inside an installed ASR model folder: the Parakeet
/// archive's `…/test_wavs/en.wav` (else the first WAV there), or a WAV the
/// registry installs next to the model file (Whisper's pinned example).
pub fn find_sample_wav(model_dir: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: u8) -> Option<PathBuf> {
        let test = dir.join("test_wavs");
        if test.is_dir() {
            let en = test.join("en.wav");
            if en.is_file() {
                return Some(en);
            }
            let mut wavs: Vec<PathBuf> = std::fs::read_dir(&test)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav")))
                .collect();
            wavs.sort();
            if let Some(first) = wavs.into_iter().next() {
                return Some(first);
            }
        }
        if depth == 0 {
            return None;
        }
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();
        subdirs.into_iter().find_map(|d| walk(&d, depth - 1))
    }
    walk(model_dir, 2).or_else(|| {
        let mut wavs: Vec<PathBuf> = std::fs::read_dir(model_dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav")))
            .collect();
        wavs.sort();
        wavs.into_iter().next()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        me: u32,
        alive: Vec<u32>,
    }

    impl ProcessProbe for Probe {
        fn is_alive(&self, pid: u32) -> bool {
            pid == self.me || self.alive.contains(&pid)
        }
        fn current_pid(&self) -> u32 {
            self.me
        }
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        write_atomic_volatile(&p, b"three").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"three");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("a.json")]);

        write_json_atomic(&p, &serde_json::json!({"k": 1})).unwrap();
        let v: serde_json::Value = read_json(&p).unwrap();
        assert_eq!(v["k"], 1);
        assert!(matches!(
            read_json::<serde_json::Value>(&dir.path().join("missing.json")),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn atomic_write_fails_without_parent() {
        let dir = tempfile::tempdir().unwrap();
        let err = write_atomic(&dir.path().join("nope/a.txt"), b"x").unwrap_err();
        assert_eq!(err.code(), "io");
    }

    #[test]
    fn create_dir_durable_creates_missing_ancestors_once() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        assert!(create_dir_durable(&deep).unwrap());
        assert!(deep.is_dir());
        assert!(!create_dir_durable(&deep).unwrap());
        sync_dir(dir.path()).unwrap();
    }

    #[test]
    fn temp_names_parse_only_in_the_exact_form_written() {
        assert_eq!(
            atomic_temp_target(".meeting.json.tmp-123-0a1b2c3d"),
            Some(("meeting.json", 123))
        );
        assert_eq!(
            atomic_temp_target(".a.tmp-b.tmp-7-ffffffff"),
            Some(("a.tmp-b", 7))
        );
        for other in [
            "meeting.json",
            "meeting.json.tmp-1-0a1b2c3d",
            "..tmp-1-0a1b2c3d",
            ".x.tmp--0a1b2c3d",
            ".x.tmp-1-0A1B2C3D",
            ".x.tmp-1-0a1b2c3",
            ".x.tmp-1-0a1b2c3d4",
            ".x.tmp-1x-0a1b2c3d",
            ".x.tmp-99999999999-0a1b2c3d",
            ".x.tmp-1",
        ] {
            assert_eq!(atomic_temp_target(other), None, "{other}");
        }
    }

    #[test]
    fn sweep_removes_only_old_temps_of_dead_writers() {
        let dir = tempfile::tempdir().unwrap();
        let probe = Probe {
            me: 10,
            alive: vec![20],
        };
        let files = [
            ".a.json.tmp-30-0000000a",  // dead writer: removed
            ".b.json.tmp-20-0000000b",  // live writer (or a reused PID): kept
            ".c.json.tmp-10-0000000c",  // this process: kept
            "d.json",                   // not a temp
            ".e.json.tmp-30-0000000",   // not the exact form
            ".f.json.tmp-30-0000000f1", // not the exact form
        ];
        for f in files {
            fs::write(dir.path().join(f), b"x").unwrap();
        }
        fs::create_dir(dir.path().join(".g.json.tmp-30-0000000a")).unwrap();
        // Recent files are never removed: another machine may be writing.
        assert_eq!(
            sweep_orphan_temps(dir.path(), &probe, Duration::from_secs(3600)).unwrap(),
            0
        );
        assert_eq!(
            sweep_orphan_temps(dir.path(), &probe, Duration::ZERO).unwrap(),
            1
        );
        assert!(!dir.path().join(files[0]).exists());
        for kept in &files[1..] {
            assert!(dir.path().join(kept).exists(), "{kept}");
        }
        assert!(dir.path().join(".g.json.tmp-30-0000000a").is_dir());
        assert_eq!(
            sweep_orphan_temps(&dir.path().join("missing"), &probe, Duration::ZERO).unwrap(),
            0
        );
    }
}

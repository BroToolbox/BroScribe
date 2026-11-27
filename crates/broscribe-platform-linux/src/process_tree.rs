//! The application a microphone helper belongs to. Chromium (and Electron)
//! runs audio in a utility process and Firefox in a content process; the
//! accessibility reader needs the application's own process, so detection
//! walks up from such a helper to its first non-helper ancestor.
use std::fs;
use std::path::Path;

/// Longest parent chain followed.
const MAX_DEPTH: usize = 32;

/// What [`responsible_process`] needs to know about one process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub parent: u32,
    /// File name of the executable (the PipeWire `application.process.binary`
    /// convention), when readable.
    pub binary: Option<String>,
    /// Chromium/Electron `--type=…` or Firefox `-contentproc` child.
    pub helper: bool,
}

/// The application process responsible for `pid` and its binary name, when
/// it differs from `pid`: the first ancestor of a helper that is not itself
/// a helper. `None` for a process that is not a helper, or whose application
/// cannot be found.
pub(crate) fn responsible_process(
    pid: u32,
    info: impl Fn(u32) -> Option<ProcessInfo>,
) -> Option<(u32, Option<String>)> {
    let mut current = info(pid)?;
    if !current.helper {
        return None;
    }
    let mut child = pid;
    for _ in 0..MAX_DEPTH {
        let parent = current.parent;
        if parent <= 1 || parent == child {
            return None;
        }
        current = info(parent)?;
        if !current.helper {
            return Some((parent, current.binary));
        }
        child = parent;
    }
    None
}

/// Whether a command line marks a browser/Electron child process.
pub(crate) fn is_helper_command(args: &[&[u8]]) -> bool {
    args.iter()
        .skip(1)
        .any(|arg| arg.starts_with(b"--type=") || *arg == b"-contentproc")
}

/// [`ProcessInfo`] from `/proc/<pid>`.
pub(crate) fn proc_info(pid: u32) -> Option<ProcessInfo> {
    proc_info_in(Path::new("/proc"), pid)
}

fn proc_info_in(proc: &Path, pid: u32) -> Option<ProcessInfo> {
    let dir = proc.join(pid.to_string());
    let stat = fs::read(dir.join("stat")).ok()?;
    // `pid (comm) state ppid …`: comm may itself contain spaces and `)`.
    let close = stat.iter().rposition(|b| *b == b')')?;
    let mut fields = stat[close + 1..]
        .split(|b| *b == b' ')
        .filter(|f| !f.is_empty());
    let _state = fields.next()?;
    let parent = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
    let mut args: Vec<&[u8]> = cmdline
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .collect();
    // Chromium rewrites its process title into one space-joined string
    // (`base/process/set_process_title.cc`), so a lone argument is split at
    // spaces too. A plain executable path with spaces stays harmless: only
    // later arguments mark a helper.
    if let [title] = args[..] {
        args = title
            .split(|b| *b == b' ')
            .filter(|a| !a.is_empty())
            .collect();
    }
    // An executable replaced on disk (an update) reads as `<path> (deleted)`.
    let binary = fs::read_link(dir.join("exe")).ok().and_then(|exe| {
        let name = exe.file_name()?.to_string_lossy().into_owned();
        Some(
            name.strip_suffix(" (deleted)")
                .map(str::to_owned)
                .unwrap_or(name),
        )
    });
    Some(ProcessInfo {
        parent,
        binary,
        helper: is_helper_command(&args),
    })
}

/// BroScribe's executables: daemon, app, CLI and worker.
const OWN_BINARIES: [&str; 4] = [
    "broscribed",
    "broscribe-app",
    "broscribe",
    "broscribe-worker",
];
/// Players BroScribe starts for playback and audio checks.
const PLAYERS: [&str; 1] = ["ffplay"];

/// Whether `pid` is BroScribe itself (`self_pid` or one of its executables)
/// or a player BroScribe started directly. Deeper descendants are never
/// BroScribe's: a browser opened for sign-in may host the meeting.
pub(crate) fn is_own_process(
    pid: u32,
    self_pid: u32,
    info: impl Fn(u32) -> Option<ProcessInfo>,
) -> bool {
    let own = |pid: u32, process: &ProcessInfo| {
        pid == self_pid
            || process
                .binary
                .as_deref()
                .is_some_and(|b| OWN_BINARIES.contains(&b))
    };
    if pid == self_pid {
        return true;
    }
    let Some(process) = info(pid) else {
        return false;
    };
    own(pid, &process)
        || (process
            .binary
            .as_deref()
            .is_some_and(|b| PLAYERS.contains(&b))
            && (process.parent == self_pid
                || info(process.parent).is_some_and(|parent| own(process.parent, &parent))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tree() -> HashMap<u32, ProcessInfo> {
        let p = |parent, binary: &str, helper| ProcessInfo {
            parent,
            binary: Some(binary.into()),
            helper,
        };
        HashMap::from([
            (1, p(0, "systemd", false)),
            (900, p(1, "gnome-shell", false)),
            // Browser, its zygote, and the audio utility process.
            (1000, p(900, "chrome", false)),
            (1001, p(1000, "chrome", true)),
            (1002, p(1001, "chrome", true)),
            // Firefox content process.
            (2000, p(900, "firefox", false)),
            (2001, p(2000, "firefox", true)),
            // A native app capturing itself.
            (3000, p(900, "zoom", false)),
            // A helper whose parent is gone.
            (4001, p(4000, "chrome", true)),
        ])
    }

    #[test]
    fn audio_helpers_resolve_to_their_application() {
        let tree = tree();
        let info = |pid| tree.get(&pid).cloned();
        assert_eq!(
            responsible_process(1002, info),
            Some((1000, Some("chrome".into())))
        );
        assert_eq!(
            responsible_process(2001, info),
            Some((2000, Some("firefox".into())))
        );
        assert_eq!(responsible_process(3000, info), None);
        assert_eq!(responsible_process(1000, info), None);
        assert_eq!(responsible_process(4001, info), None);
        assert_eq!(responsible_process(5000, info), None);
    }

    #[test]
    fn helper_commands_are_recognised() {
        let chrome: [&[u8]; 3] = [
            b"/opt/google/chrome/chrome",
            b"--type=utility",
            b"--utility-sub-type=audio.mojom.AudioService",
        ];
        assert!(is_helper_command(&chrome));
        assert!(is_helper_command(&[
            b"/usr/lib/firefox/firefox",
            b"-contentproc"
        ]));
        assert!(!is_helper_command(&[
            b"/opt/google/chrome/chrome",
            b"https://example.invalid/"
        ]));
        assert!(!is_helper_command(&[b"--type=utility"]));
    }

    #[test]
    fn own_processes_are_broscribe_and_its_direct_players() {
        let p = |parent, binary: &str| ProcessInfo {
            parent,
            binary: Some(binary.into()),
            helper: false,
        };
        let tree = HashMap::from([
            (100, p(1, "broscribed")),
            (200, p(1, "broscribe-app")),
            (201, p(200, "ffplay")),
            // A browser the app opened for sign-in, and its child.
            (202, p(200, "xdg-open")),
            (203, p(202, "firefox")),
            (204, p(203, "ffplay")),
            (300, p(1, "ffplay")),
            (400, p(1, "zoom")),
            (101, p(100, "ffplay")),
        ]);
        let info = |pid| tree.get(&pid).cloned();
        for pid in [100, 200, 201, 101, 999] {
            assert!(is_own_process(pid, 999, info), "{pid}");
        }
        for pid in [202, 203, 204, 300, 400, 7] {
            assert!(!is_own_process(pid, 999, info), "{pid}");
        }
    }

    #[test]
    fn proc_entries_are_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("1002");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("stat"), b"1002 (chrome (x) y) S 1001 1002 0 0").unwrap();
        fs::write(dir.join("cmdline"), b"/opt/chrome\0--type=utility\0").unwrap();
        std::os::unix::fs::symlink("/opt/google/chrome/chrome", dir.join("exe")).unwrap();
        assert_eq!(
            proc_info_in(tmp.path(), 1002),
            Some(ProcessInfo {
                parent: 1001,
                binary: Some("chrome".into()),
                helper: true,
            })
        );
        // Chromium's rewritten title: one NUL-terminated, space-joined string.
        let rewritten = tmp.path().join("1003");
        fs::create_dir(&rewritten).unwrap();
        fs::write(rewritten.join("stat"), b"1003 (chrome) S 1001 1003 0 0").unwrap();
        fs::write(
            rewritten.join("cmdline"),
            b"/opt/google/chrome/chrome --type=utility --utility-sub-type=audio.mojom.AudioService\0\0",
        )
        .unwrap();
        std::os::unix::fs::symlink("/opt/google/chrome/chrome", rewritten.join("exe")).unwrap();
        assert_eq!(
            proc_info_in(tmp.path(), 1003),
            Some(ProcessInfo {
                parent: 1001,
                binary: Some("chrome".into()),
                helper: true,
            })
        );
        // A main process whose path contains a space is not a helper.
        let spaced = tmp.path().join("1004");
        fs::create_dir(&spaced).unwrap();
        fs::write(spaced.join("stat"), b"1004 (app) S 1 1004 0 0").unwrap();
        fs::write(spaced.join("cmdline"), b"/opt/My App/app\0").unwrap();
        assert_eq!(
            proc_info_in(tmp.path(), 1004).map(|p| p.helper),
            Some(false)
        );
        assert_eq!(proc_info_in(tmp.path(), 7), None);
    }
}

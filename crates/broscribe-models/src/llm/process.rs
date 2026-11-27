//! Child processes for agent CLIs (design §8.4, plan M2 §9.7, §10.5): a fresh
//! mode-0700 temp dir as cwd, mode-0600 files, an environment allow-list, and
//! descendants that never outlive the call: every child is the root of a tree
//! contained by the context's platform control (a process group on Unix, a
//! Job Object on Windows), and when it exits, times out, is cancelled or
//! stops reading its prompt, the whole tree is killed, then it is reaped.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use broscribe_core::{Error, Result};
use broscribe_platform::{ProcessTreeHandle, TreeExit, wait_tree};

use crate::execution::ExecutionContext;

/// Environment variables passed to agent CLIs (needed for the vendor's own
/// login), plus [`ENV_ALLOW_WINDOWS`] on Windows; everything else, including
/// `CLAUDECODE`, `CLAUDE_CODE_*`, other `CODEX_*` and `BROSCRIBE_*`, is
/// dropped.
pub const ENV_ALLOW: [&str; 10] = [
    "HOME",
    "PATH",
    "LANG",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "SECURITYSESSIONID",
    // Vendor login locations; BroScribe never reads them.
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
];

/// Further variables passed on Windows. A `.cmd` shim needs cmd.exe's own
/// environment (`SystemRoot`, `ComSpec`, `PATHEXT`) to start `node`, and a
/// native CLI finds its login under the profile folders. None holds a
/// secret; vendor tokens and API keys stay dropped as on Unix.
pub const ENV_ALLOW_WINDOWS: [&str; 18] = [
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "USERNAME",
    "HOMEDRIVE",
    "HOMEPATH",
    "ProgramData",
    "ProgramFiles",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];

/// Largest stdout accepted from a child (also the cap on a reply file).
pub(super) const MAX_STDOUT: usize = 32 << 20;
/// Bytes of stderr kept: reported only for `--version` / `--help` failures,
/// and searched for a registry `reported_model` stderr header (only the
/// validated model name is kept; stderr itself is never logged).
const STDERR_HEAD: usize = 4096;
/// Time given to the output readers after the child exits (grandchildren may
/// still hold the pipes).
const DRAIN_GRACE: Duration = Duration::from_secs(5);
/// How often a call that both watches a cancel flag and writes a prompt
/// checks the two.
const RELAY_POLL: Duration = Duration::from_millis(20);

/// Whether `name` is passed to agent CLIs. With `windows`, names match
/// [`ENV_ALLOW`] and [`ENV_ALLOW_WINDOWS`] ignoring ASCII case (Windows
/// spells `PATH` as `Path`); otherwise exactly, against [`ENV_ALLOW`] only.
fn env_allowed(name: &OsStr, windows: bool) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if !windows {
        return ENV_ALLOW.contains(&name) || name.starts_with("XDG_");
    }
    ENV_ALLOW
        .iter()
        .chain(&ENV_ALLOW_WINDOWS)
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
        || name
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("XDG_"))
}

/// The allow-listed subset of `vars`, with `extra_path` dirs added to `PATH`
/// (GUI apps do not inherit the shell `PATH`; scripts need e.g. `node`), for
/// this platform; see [`child_env_with`].
pub fn child_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    extra_path: &[PathBuf],
) -> Vec<(OsString, OsString)> {
    child_env_with(vars, extra_path, cfg!(windows))
}

/// [`child_env`] with Windows' rules chosen explicitly (tests cover both on
/// every OS). With `windows`, the extra names of [`ENV_ALLOW_WINDOWS`] pass,
/// names and the `PATH` key match ignoring ASCII case, and the variable keeps
/// its original spelling.
pub fn child_env_with(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    extra_path: &[PathBuf],
    windows: bool,
) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = vars
        .into_iter()
        .filter(|(k, _)| env_allowed(k, windows))
        .collect();
    if !extra_path.is_empty() {
        let is_path = |k: &OsStr| {
            if windows {
                k.eq_ignore_ascii_case("PATH")
            } else {
                k == "PATH"
            }
        };
        let (key, current) = match out.iter().position(|(k, _)| is_path(k)) {
            Some(i) => {
                let (k, v) = out.remove(i);
                (k, Some(v))
            }
            None => (OsString::from("PATH"), None),
        };
        let mut dirs: Vec<PathBuf> = extra_path.to_vec();
        if let Some(p) = &current {
            for d in std::env::split_paths(p) {
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
        }
        if let Ok(joined) = std::env::join_paths(dirs) {
            out.push((key, joined));
        }
    }
    out
}

/// A private temp dir, removed on drop.
pub struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Result<Self> {
        let mut b = tempfile::Builder::new();
        b.prefix("broscribe-llm-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            b.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let dir = b
            .tempdir()
            .map_err(|e| Error::io("creating a temp dir for an LLM call", e))?;
        restrict(dir.path(), 0o700)?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Creates `name` with mode 0600 (never an existing file) and writes `bytes`.
    pub fn write_private(&self, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.dir.path().join(name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path).map_err(|e| Error::io_at(&path, e))?;
        restrict(&path, 0o600)?;
        f.write_all(bytes)
            .and_then(|()| f.flush())
            .map_err(|e| Error::io_at(&path, e))?;
        Ok(path)
    }

    /// Removes the dir now, reporting failures (drop ignores them).
    pub fn close(self) -> Result<()> {
        let path = self.dir.path().to_path_buf();
        self.dir.close().map_err(|e| Error::io_at(&path, e))
    }
}

/// Sets `mode` explicitly (the umask applies at creation).
fn restrict(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| Error::io_at(path, e))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Whether `path` is a file the current user may execute.
pub fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = true;
    meta.is_file() && executable
}

enum Drained {
    Stdout { buf: Vec<u8>, overflow: bool },
    Stderr { head: Vec<u8>, total: usize },
}

pub struct RunOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr_bytes: usize,
    /// At most the first 4 KiB of stderr, lossily decoded. May echo the
    /// prompt: never put it in errors or logs.
    pub stderr_head: String,
}

pub struct RunSpec<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a [(OsString, OsString)],
    /// Written to stdin, then closed; `None` gives the child an empty stdin.
    pub stdin: Option<&'a [u8]>,
    pub timeout: Duration,
}

/// Runs the child as the root of a contained process tree with a cleared
/// environment. When it exits (or on timeout) the rest of its tree is killed
/// before it is reaped. A timeout returns `Error::Inference`, and so does a
/// prompt the child did not read in full, even when it exited successfully;
/// a child that stops reading its prompt is stopped at once rather than at
/// the timeout.
pub fn run(spec: &RunSpec<'_>, execution: &ExecutionContext) -> Result<RunOutput> {
    run_inner(spec, None, execution)
}

/// [`run`], also stopped (tree killed and reaped, `Error::Inference`) as soon
/// as `cancel` is set.
pub fn run_cancellable(
    spec: &RunSpec<'_>,
    cancel: &AtomicBool,
    execution: &ExecutionContext,
) -> Result<RunOutput> {
    run_inner(spec, Some(cancel), execution)
}

fn run_inner(
    spec: &RunSpec<'_>,
    cancel: Option<&AtomicBool>,
    execution: &ExecutionContext,
) -> Result<RunOutput> {
    let name = spec
        .program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut cmd = Command::new(spec.program);
    cmd.args(spec.args)
        .current_dir(spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let tree = execution.process_tree();
    tree.prepare(&mut cmd)?;
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::io(format!("starting {name}"), e))?;
    let mut handle = match tree.attach(&child) {
        Ok(handle) => handle,
        Err(e) => {
            // Not contained: stop and reap the child before anything runs
            // (on Windows it is still suspended).
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };

    let (tx, rx) = mpsc::channel::<Drained>();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        let mut overflow = false;
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
                // Keep draining so the child is not blocked on a full pipe.
                Ok(n) if buf.len() + n > MAX_STDOUT => overflow = true,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let _ = tx_out.send(Drained::Stdout { buf, overflow });
    });
    let mut stderr = child.stderr.take().expect("piped stderr");
    std::thread::spawn(move || {
        let mut head = Vec::new();
        let mut total = 0usize;
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
                Ok(n) => {
                    total += n;
                    let room = STDERR_HEAD.saturating_sub(head.len());
                    head.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        let _ = tx.send(Drained::Stderr { head, total });
    });
    // A child that never reads stdin must not block us: write on a detached
    // thread (a descendant that escaped containment could keep the pipe
    // open), report whether every byte was delivered, and stop the wait at
    // once when the write fails.
    let stdin_failed = Arc::new(AtomicBool::new(false));
    let delivered = match (spec.stdin, child.stdin.take()) {
        (Some(bytes), Some(mut stdin)) => {
            let bytes = bytes.to_vec();
            let failed = Arc::clone(&stdin_failed);
            let (done_tx, done_rx) = mpsc::channel::<bool>();
            std::thread::spawn(move || {
                let ok = stdin.write_all(&bytes).and_then(|()| stdin.flush()).is_ok();
                // Close stdin before reporting, so the child sees its end.
                drop(stdin);
                if !ok {
                    failed.store(true, Ordering::Release);
                }
                let _ = done_tx.send(ok);
            });
            Some(done_rx)
        }
        _ => None,
    };

    // Whatever ends the wait, nothing the child started outlives the call:
    // `wait_tree` kills the remaining tree, then reaps the leader.
    let deadline = Instant::now() + spec.timeout;
    let stdin_watch = delivered.is_some().then_some(&*stdin_failed);
    let exit = match (cancel, stdin_watch) {
        (None, None) => wait_tree(handle.as_mut(), &mut child, Some(deadline), None),
        (Some(stop), None) | (None, Some(stop)) => {
            wait_tree(handle.as_mut(), &mut child, Some(deadline), Some(stop))
        }
        (Some(cancel), Some(stdin_failed)) => {
            wait_until_either(handle.as_mut(), &mut child, deadline, cancel, stdin_failed)
        }
    }
    .map_err(|e| Error::Inference(format!("waiting for {name} failed ({})", e.code())))?;
    let status = match exit {
        TreeExit::Exited(status) => status,
        TreeExit::TimedOut => {
            return Err(Error::Inference(format!(
                "{name} timed out after {} s and was stopped",
                spec.timeout.as_secs()
            )));
        }
        TreeExit::Cancelled if cancel.is_some_and(|c| c.load(Ordering::Acquire)) => {
            return Err(Error::Inference(format!(
                "{name} was cancelled and stopped"
            )));
        }
        TreeExit::Cancelled => {
            return Err(Error::Inference(format!(
                "{name} stopped reading the prompt and was stopped"
            )));
        }
    };

    let mut out = None;
    let mut err = None;
    let drain_deadline = Instant::now() + DRAIN_GRACE;
    while out.is_none() || err.is_none() {
        let left = drain_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Drained::Stdout { buf, overflow }) => out = Some((buf, overflow)),
            Ok(Drained::Stderr { head, total }) => err = Some((head, total)),
            Err(_) => {
                // Processes outside the tree keep the pipes open.
                break;
            }
        }
    }
    if let Some(done) = delivered
        && !done.recv_timeout(DRAIN_GRACE).unwrap_or(false)
    {
        return Err(Error::Inference(format!(
            "{name} exited without reading the whole prompt"
        )));
    }
    let Some((stdout, overflow)) = out else {
        return Err(Error::Inference(format!(
            "{name} exited but its output did not close"
        )));
    };
    if overflow {
        return Err(Error::Inference(format!(
            "{name} wrote more than {} MiB",
            MAX_STDOUT >> 20
        )));
    }
    let (head, total) = err.unwrap_or_default();
    Ok(RunOutput {
        status,
        stdout,
        stderr_bytes: total,
        stderr_head: String::from_utf8_lossy(&head).into_owned(),
    })
}

/// [`wait_tree`] stopped by whichever of `cancel` and `stdin_failed` is set
/// first: a scoped relay folds both into the one flag `wait_tree` watches and
/// ends with the wait.
fn wait_until_either(
    handle: &mut dyn ProcessTreeHandle,
    child: &mut std::process::Child,
    deadline: Instant,
    cancel: &AtomicBool,
    stdin_failed: &AtomicBool,
) -> Result<TreeExit> {
    let stop = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let relay = scope.spawn(|| {
            while !finished.load(Ordering::Acquire) {
                if cancel.load(Ordering::Acquire) || stdin_failed.load(Ordering::Acquire) {
                    stop.store(true, Ordering::Release);
                    return;
                }
                std::thread::park_timeout(RELAY_POLL);
            }
        });
        let exit = wait_tree(handle, child, Some(deadline), Some(&stop));
        finished.store(true, Ordering::Release);
        relay.thread().unpark();
        exit
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_allow_list() {
        let vars: Vec<(OsString, OsString)> = [
            ("HOME", "/h"),
            ("PATH", "/usr/bin"),
            ("LANG", "en_US.UTF-8"),
            ("TMPDIR", "/t"),
            ("USER", "u"),
            ("LOGNAME", "u"),
            ("SHELL", "/bin/zsh"),
            ("SECURITYSESSIONID", "1"),
            ("XDG_CONFIG_HOME", "/x"),
            ("CODEX_HOME", "/c"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "cli"),
            ("CODEX_SANDBOX", "seatbelt"),
            ("BROSCRIBE_MODEL_MIRROR", "/m"),
            ("OPENAI_API_KEY", "sk-test"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let env = child_env(vars, &[]);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        for kept in [
            "HOME",
            "PATH",
            "LANG",
            "TMPDIR",
            "USER",
            "LOGNAME",
            "SHELL",
            "SECURITYSESSIONID",
            "XDG_CONFIG_HOME",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
        ] {
            assert!(names.contains(&kept), "{kept} dropped");
        }
        for dropped in [
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CODEX_SANDBOX",
            "BROSCRIBE_MODEL_MIRROR",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!names.contains(&dropped), "{dropped} kept");
        }
    }

    #[test]
    fn extra_path_goes_first_without_duplicates() {
        let env = child_env(
            [(
                "PATH".into(),
                std::env::join_paths(["/usr/bin", "/opt/x"]).unwrap(),
            )],
            &[PathBuf::from("/opt/x"), PathBuf::from("/a")],
        );
        let path = env.iter().find(|(k, _)| k == "PATH").unwrap().1.clone();
        assert_eq!(
            std::env::split_paths(&path).collect::<Vec<_>>(),
            [
                PathBuf::from("/opt/x"),
                PathBuf::from("/a"),
                PathBuf::from("/usr/bin")
            ]
        );
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn names(env: &[(OsString, OsString)]) -> Vec<&str> {
        env.iter().map(|(k, _)| k.to_str().unwrap()).collect()
    }

    /// Names no mode passes, whatever their case.
    const NEVER_PASSED: [&str; 7] = [
        "CLAUDECODE",
        "claudecode",
        "CLAUDE_CODE_ENTRYPOINT",
        "CODEX_SANDBOX",
        "BROSCRIBE_MODEL_MIRROR",
        "Broscribe_Data_Dir",
        "OPENAI_API_KEY",
    ];

    #[test]
    fn windows_mode_keeps_the_path_spelling_and_merges_it() {
        let input = vars(&[
            ("Path", "/usr/bin"),
            ("SystemRoot", "/w"),
            ("appdata", "/r"),
            ("ComSpec", "/w/cmd.exe"),
        ]);
        let env = child_env_with(input, &[PathBuf::from("/a")], true);
        let kept = names(&env);
        assert!(!kept.contains(&"PATH"), "{kept:?}");
        for name in ["SystemRoot", "appdata", "ComSpec"] {
            assert!(kept.contains(&name), "{name} dropped");
        }
        let path = &env.iter().find(|(k, _)| k == "Path").unwrap().1;
        assert_eq!(
            std::env::split_paths(path).collect::<Vec<_>>(),
            [PathBuf::from("/a"), PathBuf::from("/usr/bin")]
        );

        // Without a `Path` of any spelling, the merged one is `PATH`.
        let env = child_env_with(vars(&[("SystemRoot", "/w")]), &[PathBuf::from("/a")], true);
        assert_eq!(names(&env), ["SystemRoot", "PATH"]);
    }

    #[test]
    fn windows_names_pass_only_in_windows_mode() {
        let mut pairs: Vec<(&str, &str)> = ENV_ALLOW_WINDOWS.iter().map(|n| (*n, "v")).collect();
        pairs.push(("Path", "/usr/bin"));
        let env = child_env_with(vars(&pairs), &[], false);
        assert!(env.is_empty(), "{:?}", names(&env));

        let env = child_env_with(vars(&pairs), &[], true);
        let kept = names(&env);
        for name in ENV_ALLOW_WINDOWS.iter().chain(&["Path"]) {
            assert!(kept.contains(name), "{name} dropped");
        }
    }

    #[test]
    fn agent_and_secret_variables_are_dropped_in_both_modes() {
        let pairs: Vec<(&str, &str)> = NEVER_PASSED.iter().map(|n| (*n, "v")).collect();
        for windows in [false, true] {
            let env = child_env_with(vars(&pairs), &[], windows);
            assert!(env.is_empty(), "windows={windows}: {:?}", names(&env));
        }
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_modes_and_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let sb = Sandbox::new().unwrap();
        let dir = sb.path().to_path_buf();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let f = sb.write_private("p.txt", b"x").unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            sb.write_private("p.txt", b"y").is_err(),
            "never reuses a file"
        );
        sb.close().unwrap();
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    fn path_only() -> Vec<(OsString, OsString)> {
        child_env([("PATH".into(), "/usr/bin:/bin".into())], &[])
    }

    #[cfg(unix)]
    fn sh(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    /// `(prepared, attached, killed)` trees of the synthetic control.
    #[cfg(unix)]
    fn counts(tree: &broscribe_platform::fakes::FakeProcessTreeControl) -> (usize, usize, usize) {
        (
            tree.prepared.load(Ordering::SeqCst),
            tree.attached.load(Ordering::SeqCst),
            tree.kills(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn success_returns_the_final_stdout_and_still_contains_the_tree() {
        let (tree, execution) = crate::execution::test_context();
        let sb = Sandbox::new().unwrap();
        let args = sh("cat; printf '|%s' \"$BROSCRIBE_X\"");
        let out = run(
            &RunSpec {
                program: Path::new("/bin/sh"),
                args: &args,
                cwd: sb.path(),
                env: &child_env(
                    [
                        ("BROSCRIBE_X".into(), "leak".into()),
                        ("PATH".into(), "/usr/bin:/bin".into()),
                    ],
                    &[],
                ),
                stdin: Some(b"hello"),
                timeout: Duration::from_secs(10),
            },
            &execution,
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(
            out.stdout, b"hello|",
            "stdin delivered, environment cleared"
        );
        assert_eq!(
            counts(&tree),
            (1, 1, 1),
            "a normal exit still kills what is left of the tree before reaping"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_cancel_stop_the_tree() {
        for (cancelled, stdin) in [
            (false, None),
            (true, None),
            // Cancel while a prompt was delivered: both flags are watched.
            (true, Some(&b"short prompt"[..])),
        ] {
            let (tree, execution) = crate::execution::test_context();
            let sb = Sandbox::new().unwrap();
            let args = sh("exec sleep 30");
            let env = path_only();
            let cancel = AtomicBool::new(false);
            let spec = RunSpec {
                program: Path::new("/bin/sh"),
                args: &args,
                cwd: sb.path(),
                env: &env,
                stdin,
                timeout: Duration::from_millis(if cancelled { 30_000 } else { 300 }),
            };
            let started = Instant::now();
            let err = std::thread::scope(|s| {
                if cancelled {
                    s.spawn(|| {
                        std::thread::sleep(Duration::from_millis(200));
                        cancel.store(true, Ordering::Release);
                    });
                }
                run_cancellable(&spec, &cancel, &execution)
                    .err()
                    .expect("stopped")
            });
            assert_eq!(err.code(), "inference");
            let expected = if cancelled { "cancelled" } else { "timed out" };
            assert!(err.to_string().contains(expected), "{err}");
            assert!(started.elapsed() < Duration::from_secs(10));
            assert_eq!(counts(&tree), (1, 1, 1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_stops_reading_its_prompt_is_stopped_before_the_timeout() {
        // Closes stdin and keeps running: the write fails long before 30 s.
        let prompt = vec![b'p'; 4 << 20];
        for watch_cancel in [false, true] {
            let (tree, execution) = crate::execution::test_context();
            let sb = Sandbox::new().unwrap();
            let args = sh("exec 0<&-; exec sleep 30");
            let env = path_only();
            let spec = RunSpec {
                program: Path::new("/bin/sh"),
                args: &args,
                cwd: sb.path(),
                env: &env,
                stdin: Some(&prompt),
                timeout: Duration::from_secs(30),
            };
            let started = Instant::now();
            let cancel = AtomicBool::new(false);
            let err = if watch_cancel {
                run_cancellable(&spec, &cancel, &execution)
            } else {
                run(&spec, &execution)
            }
            .err()
            .expect("an unread prompt is a failure");
            assert_eq!(err.code(), "inference");
            assert!(err.to_string().contains("prompt"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(10));
            assert_eq!(counts(&tree), (1, 1, 1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_prompt_the_child_did_not_read_fails_even_with_valid_output() {
        let (tree, execution) = crate::execution::test_context();
        let sb = Sandbox::new().unwrap();
        // Reads one byte of a prompt far larger than a pipe buffer, then
        // answers with valid JSON and exits successfully.
        let args = sh("head -c 1 >/dev/null; printf '{\"summary\":\"x\"}'");
        let prompt = vec![b'p'; 4 << 20];
        let err = run(
            &RunSpec {
                program: Path::new("/bin/sh"),
                args: &args,
                cwd: sb.path(),
                env: &path_only(),
                stdin: Some(&prompt),
                timeout: Duration::from_secs(10),
            },
            &execution,
        )
        .err()
        .expect("an unread prompt is a failure");
        assert!(err.to_string().contains("prompt"), "{err}");
        assert_eq!(err.code(), "inference");
        assert_eq!(counts(&tree), (1, 1, 1));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_whose_tree_cannot_be_attached_is_stopped_and_reaped() {
        let (tree, execution) = crate::execution::test_context();
        tree.fail_attach.store(true, Ordering::SeqCst);
        let sb = Sandbox::new().unwrap();
        let marker = sb.path().join("late");
        let args = sh(&format!("sleep 1; echo late > '{}'", marker.display()));
        let err = run(
            &RunSpec {
                program: Path::new("/bin/sh"),
                args: &args,
                cwd: sb.path(),
                env: &path_only(),
                stdin: None,
                timeout: Duration::from_secs(10),
            },
            &execution,
        )
        .err()
        .expect("an uncontained child is refused");
        assert_eq!(err.code(), "internal", "{err}");
        assert_eq!(counts(&tree), (1, 0, 0));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the uncontained child kept running");
    }
}

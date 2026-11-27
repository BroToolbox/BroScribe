//! Native synthetic CLI fixtures for the app's tests.
//!
//! A fixture is written once per platform so every generic test runs
//! natively: on Unix a `/bin/sh` script; on Windows a `.cmd` file (CRLF,
//! `@echo off`, `exit /b`) that hands its logical argv, one argument per
//! process-local environment variable, to one inline line of the built-in
//! Windows PowerShell. Inline commands need no execution-policy change. The
//! PowerShell prelude gives each body byte-exact stdin, stdout and stderr
//! and the same argv views a shell script has, so both bodies say the same
//! thing:
//!
//! | `/bin/sh`                          | PowerShell body                     |
//! |------------------------------------|-------------------------------------|
//! | `"$*"`                             | `$all`                              |
//! | `"$4"`, `"$4 $5"`                  | `(fx_arg 4)`, `(fx_words 4,5)`      |
//! | `printf '%s\n' "$*" >> log`        | `fx_log_args <log>`                 |
//! | `printf '%s' text` / `echo text`   | `fx_print <s>` / `fx_echo <s>`      |
//! | `printf '\377\376'`                | `fx_bytes 255,254`                  |
//! | the same `>&2`                     | `fx_eprint`, `fx_eecho`, `fx_ebytes`|
//! | `cat file`                         | `fx_cat <path>`                     |
//! | `cat`                              | `fx_copy_stdin`                     |
//! | `cat > file` / `cat >> file`       | `fx_save_stdin` / `fx_append_stdin` |
//! | `cat > /dev/null`                  | `fx_drain_stdin`                    |
//! | `printf text >> file`              | `fx_append <path> <s>`              |
//! | `touch file`, `[ -f file ]`        | `fx_touch <path>`, `(fx_exists <p>)`|
//! | `case "$*" in *pat*)`              | `$all -clike '*pat*'`               |
//!
//! Every path and every text with a double quote or a non-ASCII character
//! goes into a body through [`ps_path`] / [`ps_str`]: the body travels in
//! one double-quoted `cmd.exe` argument, so it never holds a raw `"`, and
//! the `.cmd` file stays ASCII. `%` is escaped for `cmd.exe` here.

use std::path::{Path, PathBuf};

pub(crate) fn admission(data_dir: &Path) -> std::sync::Arc<broscribe_core::maintenance::ModeLease> {
    std::sync::Arc::new(
        broscribe_core::maintenance::ModeLease::acquire(data_dir, std::time::Duration::ZERO)
            .unwrap(),
    )
}

/// Synthetic profiles are explicit overrides, even on native test hosts.
pub(crate) fn app_init(
    mut paths: broscribe_platform::ResolvedPaths,
    config: broscribe_core::config::Config,
    library: broscribe_core::Library,
    cli: PathBuf,
) -> crate::state::AppStateInit {
    paths.data_dir_overridden = true;
    let admission = admission(&paths.data_dir);
    crate::state::AppStateInit::new(paths, config, library, cli, admission).unwrap()
}

pub(crate) fn runner(cli: PathBuf, library: PathBuf) -> crate::jobs::CliRunner {
    let mut paths =
        broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(&library))
            .unwrap();
    paths.data_dir_overridden = true;
    let admission = std::sync::Arc::new(
        broscribe_core::maintenance::ModeLease::acquire(&paths.data_dir, std::time::Duration::ZERO)
            .unwrap(),
    );
    crate::jobs::CliRunner::new(cli, library, &paths, admission).unwrap()
}
/// The file a fixture written at `stem` is run from: `stem` on Unix,
/// `stem.cmd` on Windows (Windows runs a script by its extension).
pub(crate) fn native_cli_path(stem: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let mut path = stem.as_os_str().to_owned();
        path.push(".cmd");
        PathBuf::from(path)
    }
    #[cfg(not(windows))]
    {
        stem.to_path_buf()
    }
}

/// Writes a synthetic CLI at [`native_cli_path`]`(stem)` and returns that
/// path. `sh` is everything after `#!/bin/sh\n` of the Unix script (written
/// verbatim, mode 0700); `ps` is the one-line PowerShell body of the
/// Windows fixture, which ends with exit code 0 unless it exits itself.
pub(crate) fn write_cli(stem: &Path, sh: &str, ps: &str) -> PathBuf {
    let path = native_cli_path(stem);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = ps;
        std::fs::write(&path, format!("#!/bin/sh\n{sh}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(windows)]
    {
        let _ = sh;
        std::fs::write(&path, windows_cmd(ps)).unwrap();
    }
    path
}

/// The PowerShell helpers every Windows body runs after (one line).
#[cfg(windows)]
const PS_PRELUDE: &str = concat!(
    "$ErrorActionPreference='Stop';",
    "$n=[int]$env:BROSCRIBE_FIXTURE_ARGC;",
    "$argv=[string[]]@(for($i=1;$i -le $n;$i++){[string][Environment]::GetEnvironmentVariable('BROSCRIBE_FIXTURE_ARG'+$i)});",
    "$all=$argv -join ' ';",
    "$fx_utf8=New-Object System.Text.UTF8Encoding $false;",
    "$fx_in=[Console]::OpenStandardInput();",
    "$fx_out=[Console]::OpenStandardOutput();",
    "$fx_err=[Console]::OpenStandardError();",
    "function fx_arg([int]$k){if($k -ge 1 -and $k -le $argv.Count){$argv[$k-1]}else{''}};",
    "function fx_words([int[]]$k){@(foreach($j in $k){fx_arg $j}) -join ' '};",
    "function fx_write($s,[byte[]]$b){$s.Write($b,0,$b.Length);$s.Flush()};",
    "function fx_bytes([byte[]]$b){fx_write $fx_out $b};",
    "function fx_ebytes([byte[]]$b){fx_write $fx_err $b};",
    "function fx_print([string]$t){fx_write $fx_out ($fx_utf8.GetBytes($t))};",
    "function fx_echo([string]$t){fx_print ($t+[char]10)};",
    "function fx_eprint([string]$t){fx_write $fx_err ($fx_utf8.GetBytes($t))};",
    "function fx_eecho([string]$t){fx_eprint ($t+[char]10)};",
    "function fx_cat([string]$p){fx_write $fx_out ([IO.File]::ReadAllBytes($p))};",
    "function fx_copy_stdin{$fx_in.CopyTo($fx_out);$fx_out.Flush()};",
    "function fx_drain_stdin{$fx_in.CopyTo([IO.Stream]::Null)};",
    "function fx_to_file([string]$p,$m){$f=New-Object IO.FileStream $p,$m,([IO.FileAccess]::Write);try{$fx_in.CopyTo($f)}finally{$f.Close()}};",
    "function fx_save_stdin([string]$p){fx_to_file $p ([IO.FileMode]::Create)};",
    "function fx_append_stdin([string]$p){fx_to_file $p ([IO.FileMode]::Append)};",
    "function fx_append([string]$p,[string]$t){[IO.File]::AppendAllText($p,$t,$fx_utf8)};",
    "function fx_log_args([string]$p){fx_append $p ($all+[char]10)};",
    "function fx_touch([string]$p){fx_append $p ''};",
    "function fx_exists([string]$p){[IO.File]::Exists($p)};",
);

/// The `.cmd` file running `body`: its arguments, as `cmd.exe` received
/// them from the caller's quoting, each in a process-local variable, then
/// PowerShell's exit code as its own.
#[cfg(windows)]
fn windows_cmd(body: &str) -> String {
    let body = body.trim().trim_end_matches(';');
    let script = if body.is_empty() {
        format!("{PS_PRELUDE}exit 0")
    } else {
        format!("{PS_PRELUDE}{body};exit 0")
    }
    .replace('%', "%%");
    assert!(
        script.is_ascii() && !script.contains(['"', '\r', '\n']),
        "a Windows fixture body is one ASCII line without a raw double quote \
         (use ps_str/ps_path): {body}"
    );
    let run = format!(
        "\"%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" \
         -NoLogo -NoProfile -NonInteractive -Command \"{script}\""
    );
    // cmd.exe reads at most 8191 characters per line.
    assert!(run.len() < 8000, "Windows fixture body too long: {body}");
    let lines = [
        "@echo off",
        "setlocal DisableDelayedExpansion",
        "set BROSCRIBE_FIXTURE_ARGC=0",
        ":fixture_arg",
        "if [%1]==[] goto fixture_run",
        "set /a BROSCRIBE_FIXTURE_ARGC+=1",
        "set \"BROSCRIBE_FIXTURE_ARG%BROSCRIBE_FIXTURE_ARGC%=%~1\"",
        "shift",
        "goto fixture_arg",
        ":fixture_run",
        &run,
        "exit /b %ERRORLEVEL%",
    ];
    let mut cmd = lines.join("\r\n");
    cmd.push_str("\r\n");
    cmd
}

/// `text` as a PowerShell expression without a raw `"` and in ASCII:
/// quoted runs of plain ASCII, other characters as `[char]` codes.
pub(crate) fn ps_str(text: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut run = String::new();
    for unit in text.encode_utf16() {
        match char::from_u32(u32::from(unit)) {
            Some(c) if (' '..='~').contains(&c) && c != '"' => {
                if c == '\'' {
                    run.push('\'');
                }
                run.push(c);
            }
            _ => {
                if !run.is_empty() {
                    parts.push(format!("'{}'", std::mem::take(&mut run)));
                }
                parts.push(format!("[string][char]{unit}"));
            }
        }
    }
    if !run.is_empty() || parts.is_empty() {
        parts.push(format!("'{run}'"));
    }
    format!("({})", parts.join("+"))
}

/// `path` as a PowerShell expression ([`ps_str`]).
pub(crate) fn ps_path(path: &Path) -> String {
    ps_str(path.to_str().expect("a fixture path is Unicode"))
}

thread_local! {
    /// The buffer [`LogCapture`] fills on this thread, if one is active.
    static CAPTURE: std::cell::RefCell<Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>> =
        const { std::cell::RefCell::new(None) };
}

/// What the calling thread logs while the guard of [`LogCapture::start`]
/// lives, as plain text (every level, no colours): for tests that check
/// what reaches the log and what never does.
///
/// One global subscriber, installed once for the test binary, writes each
/// event to the buffer of the thread that emitted it and drops it on any
/// other thread. Its filter is evaluated per event, so `tracing`'s
/// process-wide callsite cache can't hide an event from a capture that
/// started while another test thread first hit the same callsite.
pub(crate) struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

/// Ends a [`LogCapture`] on its thread when dropped.
pub(crate) struct LogCaptureGuard(Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>);

impl Drop for LogCaptureGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        CAPTURE.with(|capture| *capture.borrow_mut() = previous);
    }
}

/// Writes to the emitting thread's [`CAPTURE`], if any.
struct ThreadWriter;

impl std::io::Write for ThreadWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with(|capture| {
            if let Some(buffer) = capture.borrow().as_ref() {
                buffer.lock().unwrap().extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    pub(crate) fn start() -> (Self, LogCaptureGuard) {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            use tracing_subscriber::Layer as _;
            use tracing_subscriber::layer::SubscriberExt as _;
            // Asked per event (`sometimes`), never cached per callsite.
            let capturing = tracing_subscriber::filter::dynamic_filter_fn(|_, _| {
                CAPTURE.with(|capture| capture.borrow().is_some())
            })
            .with_callsite_filter(|_| tracing::subscriber::Interest::sometimes());
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(|| ThreadWriter)
                .with_filter(capturing);
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
                .expect("no other global subscriber in the app's tests");
        });
        let buffer = std::sync::Arc::default();
        let previous =
            CAPTURE.with(|capture| capture.replace(Some(std::sync::Arc::clone(&buffer))));
        (Self(buffer), LogCaptureGuard(previous))
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

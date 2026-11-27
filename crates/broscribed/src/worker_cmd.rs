//! How the daemon starts `broscribe-worker`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// Name of the worker binary, installed next to `broscribed`.
pub const WORKER_BIN: &str = "broscribe-worker";

/// Program plus leading arguments; the daemon appends
/// `--library <root> <subcommand> …`. Tests point it at a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub prefix: Vec<OsString>,
    /// Explicit resolved profile only; `None` preserves native default cache placement.
    pub data_dir_override: Option<PathBuf>,
}

impl WorkerCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            prefix: Vec::new(),
            data_dir_override: None,
        }
    }

    /// A development sibling, or the enclosing application's MacOS folder
    /// when this process is the nested login-item helper app.
    pub fn sibling() -> io::Result<Self> {
        Ok(Self::new(Self::bundled_executable(WORKER_BIN)?))
    }

    /// Finds a packaged executable by its stem, adding the platform suffix.
    pub fn bundled_executable(name: &str) -> io::Result<PathBuf> {
        let exe = std::env::current_exe()?;
        Self::locate_from(&exe, name)
    }

    fn locate_from(exe: &Path, name: &str) -> io::Result<PathBuf> {
        let parent = exe
            .parent()
            .ok_or_else(|| io::Error::other("executable has no folder"))?;
        let mut direct = parent.join(name);
        direct.as_mut_os_string().push(std::env::consts::EXE_SUFFIX);
        if direct.is_file() {
            return Ok(direct);
        }
        for ancestor in parent.ancestors() {
            if ancestor.file_name().is_some_and(|s| s == "Contents") {
                let mut candidate = ancestor.join("MacOS").join(name);
                candidate
                    .as_mut_os_string()
                    .push(std::env::consts::EXE_SUFFIX);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{name}{} is not installed beside the daemon or in its application bundle",
                std::env::consts::EXE_SUFFIX
            ),
        ))
    }

    /// The full argument list for `library` and `args`.
    pub fn args(&self, library: &Path, args: &[&OsStr]) -> Vec<OsString> {
        let mut out = self.prefix.clone();
        out.push("--library".into());
        out.push(library.as_os_str().to_owned());
        out.extend(args.iter().map(|a| (*a).to_owned()));
        out
    }

    pub fn tokio_command(&self, library: &Path, args: &[&OsStr]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(self.args(library, args));
        match &self.data_dir_override {
            Some(data_dir) => {
                cmd.env(broscribe_platform::paths::ENV_DATA_DIR, data_dir);
            }
            None => {
                cmd.env_remove(broscribe_platform::paths::ENV_DATA_DIR);
            }
        }
        cmd
    }

    pub fn std_command(&self, library: &Path, args: &[&OsStr]) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(self.args(library, args));
        match &self.data_dir_override {
            Some(data_dir) => {
                cmd.env(broscribe_platform::paths::ENV_DATA_DIR, data_dir);
            }
            None => {
                cmd.env_remove(broscribe_platform::paths::ENV_DATA_DIR);
            }
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_flag_comes_first() {
        let w = WorkerCommand {
            program: "/bin/sh".into(),
            prefix: vec!["-c".into(), "exit 0".into(), "sh".into()],
            data_dir_override: None,
        };
        let args = w.args(
            Path::new("/lib"),
            &[OsStr::new("run-one"), OsStr::new("j-1")],
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            ["-c", "exit 0", "sh", "--library", "/lib", "run-one", "j-1"]
        );
    }

    #[test]
    fn nested_agent_finds_outer_bundle_workers() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("BroScribe.app/Contents/MacOS");
        std::fs::create_dir_all(&outer).unwrap();
        let file = format!("{WORKER_BIN}{}", std::env::consts::EXE_SUFFIX);
        let worker = outer.join(&file);
        std::fs::write(&worker, b"synthetic binary").unwrap();
        let helper = tmp.path().join("BroScribe.app/Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed");
        assert_eq!(
            WorkerCommand::locate_from(&helper, WORKER_BIN).unwrap(),
            worker
        );
        let direct = tmp.path().join(&file);
        std::fs::write(&direct, b"synthetic sibling binary").unwrap();
        assert_eq!(
            WorkerCommand::locate_from(&tmp.path().join("broscribed"), WORKER_BIN).unwrap(),
            direct
        );
    }
}

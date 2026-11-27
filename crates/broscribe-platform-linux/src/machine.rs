use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use broscribe_platform::{
    AppPaths, Error, PriorityClass, ProcessPriority, ProcessProbe, ProcessUsage, ResourceUsage,
    Result, SystemInfo,
};

#[derive(Clone, Debug)]
pub struct LinuxPaths {
    home: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    runtime: Option<PathBuf>,
}

impl LinuxPaths {
    pub fn new() -> Self {
        let home = home_dir();
        Self {
            data: xdg_dir("XDG_DATA_HOME", home.join(".local/share")).join("broscribe"),
            cache: xdg_dir("XDG_CACHE_HOME", home.join(".cache")).join("broscribe"),
            config: xdg_dir("XDG_CONFIG_HOME", home.join(".config")),
            runtime: std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            home,
        }
    }
}
impl Default for LinuxPaths {
    fn default() -> Self {
        Self::new()
    }
}
impl AppPaths for LinuxPaths {
    fn data_dir(&self) -> PathBuf {
        self.data.clone()
    }
    fn cache_dir(&self) -> PathBuf {
        self.cache.clone()
    }
    fn documents_dir(&self) -> PathBuf {
        std::fs::read_to_string(self.config.join("user-dirs.dirs"))
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    let value = line.strip_prefix("XDG_DOCUMENTS_DIR=")?.trim();
                    let value = value.strip_prefix('"')?.strip_suffix('"')?;
                    if value == "$HOME" {
                        return Some(self.home.clone());
                    }
                    if let Some(relative) = value.strip_prefix("$HOME/") {
                        return Some(self.home.join(relative));
                    }
                    let absolute = PathBuf::from(value);
                    absolute.is_absolute().then_some(absolute)
                })
            })
            .unwrap_or_else(|| self.home.join("Documents"))
    }
    fn default_library_dir(&self) -> PathBuf {
        self.home.join("BroScribe")
    }
    fn uninstall_retry_env(&self) -> Vec<(&'static str, &Path)> {
        let mut env: Vec<(&'static str, &Path)> = vec![
            ("HOME", self.home.as_path()),
            ("XDG_DATA_HOME", self.data.parent().expect("XDG data home")),
            (
                "XDG_CACHE_HOME",
                self.cache.parent().expect("XDG cache home"),
            ),
            ("XDG_CONFIG_HOME", self.config.as_path()),
        ];
        if let Some(runtime) = self.runtime.as_deref() {
            env.push(("XDG_RUNTIME_DIR", runtime));
        }
        env
    }
}

pub(crate) fn xdg_dir(key: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(fallback)
}
pub(crate) fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", home_dir().join(".config"))
}
pub(crate) fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return home;
    }
    // getpwuid_r is reentrant; unlike getpwuid its backing memory is ours.
    let mut size = 4096;
    loop {
        let mut buffer = vec![0u8; size];
        let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all output pointers and backing storage remain live until copied.
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                pwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            size *= 2;
            continue;
        }
        if rc == 0 && !result.is_null() {
            // SAFETY: libc returned a populated passwd and NUL-terminated pw_dir.
            let path = unsafe { std::ffi::CStr::from_ptr((*result).pw_dir) };
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()));
            if path.is_absolute() {
                return path;
            }
        }
        // AppPaths cannot return an error. A process without HOME or a passwd
        // entry gets a per-user private location, never another user's home.
        let path = PathBuf::from(format!(
            "/tmp/broscribe-{}",
            broscribe_platform_unix::current_uid()
        ));
        broscribe_platform_unix::ensure_private_dir(&path)
            .expect("cannot establish a private Linux home fallback");
        return path;
    }
}

#[derive(Default)]
pub struct LinuxProcessProbe;
impl ProcessProbe for LinuxProcessProbe {
    fn is_alive(&self, pid: u32) -> bool {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        // SAFETY: signal zero probes without delivering a signal.
        unsafe {
            libc::kill(pid as i32, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    fn current_pid(&self) -> u32 {
        std::process::id()
    }
}

#[derive(Default)]
pub struct LinuxSystemInfo;
impl SystemInfo for LinuxSystemInfo {
    fn application_installed(&self, identities: &[String]) -> Option<bool> {
        use std::os::unix::fs::PermissionsExt as _;
        let paths = std::env::var_os("PATH").unwrap_or_default();
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let home = home_dir();
        let user_data = xdg_dir("XDG_DATA_HOME", home.join(".local/share"));
        let flatpak = home.join(".local/share/flatpak/exports/share");
        let desktop_dirs = [user_data, flatpak]
            .into_iter()
            .chain(std::env::split_paths(&data_dirs))
            .chain([std::path::PathBuf::from("/var/lib/flatpak/exports/share")]);
        let desktop_dirs = desktop_dirs
            .filter(|dir| dir.is_absolute())
            .collect::<Vec<_>>();
        Some(
            identities
                .iter()
                .filter(|id| {
                    !id.is_empty()
                        && id
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                })
                .any(|id| {
                    desktop_dirs.iter().any(|dir| {
                        dir.join("applications")
                            .join(format!("{id}.desktop"))
                            .is_file()
                    }) || std::env::split_paths(&paths)
                        .filter(|dir| dir.is_absolute())
                        .any(|dir| {
                            dir.join(id).metadata().is_ok_and(|file| {
                                file.is_file() && file.permissions().mode() & 0o111 != 0
                            })
                        })
                }),
        )
    }

    fn chip(&self) -> String {
        fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    matches!(key.trim(), "model name" | "Hardware").then(|| value.trim().to_owned())
                })
            })
            .unwrap_or_default()
    }
    fn memory_bytes(&self) -> u64 {
        fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    line.strip_prefix("MemTotal:")?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()
                })
            })
            .unwrap_or(0)
            .saturating_mul(1024)
    }
    fn free_disk_bytes(&self, path: &Path) -> Result<u64> {
        let mut ancestor = path;
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
        }
        let name = CString::new(ancestor.as_os_str().as_bytes())
            .map_err(|_| Error::InvalidInput("disk path contains NUL".into()))?;
        let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid NUL-terminated path and writable statvfs storage.
        if unsafe { libc::statvfs(name.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(Error::io_at(ancestor, std::io::Error::last_os_error()));
        }
        // SAFETY: statvfs succeeded.
        let info = unsafe { info.assume_init() };
        Ok(info.f_bavail.saturating_mul(info.f_frsize))
    }
    fn cpu_cores(&self) -> usize {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    }
}

#[derive(Default)]
pub struct LinuxResourceUsage;
impl ResourceUsage for LinuxResourceUsage {
    fn self_usage(&self) -> Result<ProcessUsage> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: writable rusage storage, valid selector.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: getrusage succeeded; Linux reports ru_maxrss in KiB.
        let usage = unsafe { usage.assume_init() };
        let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
        Ok(ProcessUsage {
            peak_rss_bytes: (usage.ru_maxrss.max(0) as u64).saturating_mul(1024),
            user_cpu_s: seconds(usage.ru_utime),
            system_cpu_s: seconds(usage.ru_stime),
        })
    }
}

#[derive(Default)]
pub struct LinuxProcessPriority;
fn set_priority(tid: u32, class: PriorityClass) -> Result<()> {
    let nice = match class {
        PriorityClass::Capture => -10,
        PriorityClass::Interactive => 0,
        PriorityClass::Utility => 5,
        PriorityClass::Background => 10,
    };
    // SAFETY: PRIO_PROCESS with a task id affects that Linux task; errors
    // (including missing CAP_SYS_NICE) are propagated, never claimed success.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
impl ProcessPriority for LinuxProcessPriority {
    fn set_current_thread(&self, class: PriorityClass) -> Result<()> {
        set_priority(0, class)
    }
    fn set_current_process(&self, class: PriorityClass) -> Result<()> {
        // Linux nice values are per-thread, so apply to all extant tasks.
        // Callers set this before spawning threads so later tasks inherit it.
        for task in fs::read_dir("/proc/self/task")? {
            let task = task?;
            let tid = task
                .file_name()
                .to_str()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| Error::Internal("invalid task id in procfs".into()))?;
            if let Err(error) = set_priority(tid, class) {
                if matches!(&error, Error::Io { source, .. } if source.raw_os_error() == Some(libc::ESRCH))
                {
                    continue;
                }
                return Err(error);
            }
        }
        Ok(())
    }
}

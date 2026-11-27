use broscribe_platform::{
    AppPaths, Error, PriorityClass, ProcessPriority, ProcessProbe, ProcessUsage, ResourceUsage,
    Result, SystemInfo,
};
use std::{
    mem::size_of,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, FILETIME, HANDLE},
        Storage::FileSystem::GetDiskFreeSpaceExW,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
            Threading::*,
        },
    },
    core::PCWSTR,
};

pub fn system_directory() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows::Win32::Foundation::MAX_PATH;
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut system = [0; MAX_PATH as usize];
    // SAFETY: Windows writes at most the supplied buffer length.
    let len = unsafe { GetSystemDirectoryW(Some(&mut system)) } as usize;
    if len == 0 || len >= system.len() {
        return Err(Error::Internal(
            "could not locate the Windows system directory".into(),
        ));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&system[..len])))
}

pub(crate) struct OwnedHandle(pub HANDLE);
unsafe impl Send for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[derive(Clone, Debug)]
pub struct WinPaths {
    roaming: PathBuf,
    local: PathBuf,
    home: PathBuf,
}
impl WinPaths {
    pub fn new() -> Self {
        Self::try_new().expect("Windows current-user known folders must be available")
    }
    pub fn try_new() -> Result<Self> {
        use windows::Win32::UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData,
        };
        Ok(Self {
            roaming: known_folder(&FOLDERID_RoamingAppData)?,
            local: known_folder(&FOLDERID_LocalAppData)?,
            home: known_folder(&FOLDERID_Profile)?,
        })
    }
}
pub(crate) fn known_folder(id: &windows::core::GUID) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    };
    unsafe {
        let folder = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
            .map_err(|e| crate::error("resolving current-user Windows folder", e))?;
        let path = PathBuf::from(std::ffi::OsString::from_wide(folder.as_wide()));
        CoTaskMemFree(Some(folder.0.cast()));
        Ok(path)
    }
}
impl Default for WinPaths {
    fn default() -> Self {
        Self::new()
    }
}
impl AppPaths for WinPaths {
    fn data_dir(&self) -> PathBuf {
        self.roaming.join("BroScribe")
    }
    fn default_data_dir_source(&self) -> &'static str {
        "the Windows Roaming AppData known folder"
    }
    fn cache_dir(&self) -> PathBuf {
        self.local.join("BroScribe/Cache")
    }
    fn documents_dir(&self) -> PathBuf {
        known_folder(&windows::Win32::UI::Shell::FOLDERID_Documents)
            .unwrap_or_else(|_| self.home.join("Documents"))
    }
    fn default_library_dir(&self) -> PathBuf {
        self.home.join("BroScribe")
    }
}
pub struct WinProcessProbe;
impl ProcessProbe for WinProcessProbe {
    fn current_pid(&self) -> u32 {
        std::process::id()
    }
    fn is_alive(&self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        unsafe {
            match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(h) => {
                    let h = OwnedHandle(h);
                    let mut code = 0;
                    GetExitCodeProcess(h.0, &mut code).is_ok() && code == 259
                }
                Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
            }
        }
    }
}
pub(crate) fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OwnedHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        QueryFullProcessImageNameW(
            h.0,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}
/// Stable detection identity independent of installation location.
pub(crate) fn app_identity(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

pub(crate) fn find_binary(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe().ok();
    find_binary_in(
        name,
        current.as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}
fn find_binary_in(
    name: &str,
    current: Option<&Path>,
    paths: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(beside) = current.and_then(Path::parent).map(|dir| dir.join(name))
        && beside.is_file()
    {
        return Some(beside);
    }
    std::env::split_paths(paths?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}
pub struct WinSystemInfo;
impl SystemInfo for WinSystemInfo {
    fn application_installed(&self, identities: &[String]) -> Option<bool> {
        use windows::{
            Win32::Foundation::ERROR_SUCCESS,
            Win32::System::Registry::{
                HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
            },
            core::PCWSTR,
        };
        let path = std::env::var_os("PATH");
        Some(
            identities
                .iter()
                .filter(|id| {
                    id.ends_with(".exe")
                        && id
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
                })
                .any(|id| {
                    if path.as_ref().is_some_and(|path| {
                        std::env::split_paths(path)
                            .filter(|dir| dir.is_absolute())
                            .any(|dir| dir.join(id).is_file())
                    }) {
                        return true;
                    }
                    let subkey =
                        format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{id}");
                    let wide = subkey.encode_utf16().chain([0]).collect::<Vec<_>>();
                    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
                        .into_iter()
                        .any(|hive| {
                            let mut bytes = 0;
                            // SAFETY: the key is NUL-terminated, and this only queries the default value’s size.
                            unsafe {
                                RegGetValueW(
                                    hive,
                                    PCWSTR(wide.as_ptr()),
                                    PCWSTR::null(),
                                    RRF_RT_REG_SZ,
                                    None,
                                    None,
                                    Some(&mut bytes),
                                ) == ERROR_SUCCESS
                            }
                        })
                }),
        )
    }

    fn chip(&self) -> String {
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default()
    }
    fn memory_bytes(&self) -> u64 {
        let mut s = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if unsafe { GlobalMemoryStatusEx(&mut s) }.is_ok() {
            s.ullTotalPhys
        } else {
            0
        }
    }
    fn free_disk_bytes(&self, path: &Path) -> Result<u64> {
        let mut path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        while !path.exists() {
            if !path.pop() {
                return Err(Error::NotFound("volume ancestor".into()));
            }
        }
        let name = crate::wide(path);
        let mut free = 0;
        unsafe { GetDiskFreeSpaceExW(PCWSTR(name.as_ptr()), Some(&mut free), None, None) }
            .map_err(|e| crate::error("querying disk space", e))?;
        Ok(free)
    }
    fn cpu_cores(&self) -> usize {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    }
}
pub struct WinResourceUsage;
impl ResourceUsage for WinResourceUsage {
    fn self_usage(&self) -> Result<ProcessUsage> {
        unsafe {
            let h = GetCurrentProcess();
            let mut mem = PROCESS_MEMORY_COUNTERS::default();
            GetProcessMemoryInfo(h, &mut mem, size_of::<PROCESS_MEMORY_COUNTERS>() as u32)
                .map_err(|e| crate::error("process memory", e))?;
            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user)
                .map_err(|e| crate::error("process CPU times", e))?;
            let seconds = |t: FILETIME| {
                ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64
                    / 10_000_000.0
            };
            Ok(ProcessUsage {
                peak_rss_bytes: mem.PeakWorkingSetSize as u64,
                user_cpu_s: seconds(user),
                system_cpu_s: seconds(kernel),
            })
        }
    }
}
pub struct WinProcessPriority;
impl ProcessPriority for WinProcessPriority {
    fn set_current_thread(&self, class: PriorityClass) -> Result<()> {
        let priority = match class {
            PriorityClass::Capture => THREAD_PRIORITY_HIGHEST,
            PriorityClass::Interactive => THREAD_PRIORITY_NORMAL,
            PriorityClass::Utility => THREAD_PRIORITY_BELOW_NORMAL,
            PriorityClass::Background => THREAD_PRIORITY_LOWEST,
        };
        unsafe { SetThreadPriority(GetCurrentThread(), priority) }
            .map_err(|e| crate::error("setting thread priority", e))
    }
    fn set_current_process(&self, class: PriorityClass) -> Result<()> {
        let priority = match class {
            PriorityClass::Capture => ABOVE_NORMAL_PRIORITY_CLASS,
            PriorityClass::Interactive => NORMAL_PRIORITY_CLASS,
            PriorityClass::Utility => BELOW_NORMAL_PRIORITY_CLASS,
            PriorityClass::Background => IDLE_PRIORITY_CLASS,
        };
        unsafe { SetPriorityClass(GetCurrentProcess(), priority) }
            .map_err(|e| crate::error("setting process priority", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_detection_uses_executable_filename_not_installation_path() {
        assert_eq!(
            app_identity(Path::new(
                r"C:\Users\Synthetic\AppData\Local\Microsoft\Teams\ms-teams.exe"
            ))
            .as_deref(),
            Some("ms-teams.exe")
        );
        assert_eq!(
            app_identity(Path::new(
                r"C:\Program Files\Google\Chrome\Application\chrome.exe"
            ))
            .as_deref(),
            Some("chrome.exe")
        );
    }

    #[test]
    fn bundled_ffmpeg_tools_are_found_without_path() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("broscribe-app.exe");
        for name in ["ffmpeg.exe", "ffprobe.exe", "ffplay.exe"] {
            let tool = temporary.path().join(name);
            std::fs::write(&tool, b"synthetic tool").unwrap();
            assert_eq!(find_binary_in(name, Some(&executable), None), Some(tool));
        }
    }
}

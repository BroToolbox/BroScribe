use broscribe_platform::{
    AppPaths, Autostart, BrowserLauncher, Error, HostPlatform, LoginJob, LoginRegistrationGuard,
    Notifier, ProcessProbe, RegisteredProfile, Result, SecretStore, SettingsPane,
    SystemSettingsOpener, plain_verbatim,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, GENERIC_WRITE, HANDLE,
            PROPERTYKEY, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile,
                StructuredStorage::*,
            },
            Memory::{GetProcessHeap, HEAP_FLAGS, HeapAlloc, HeapFree},
            Registry::*,
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateMutexW, CreateProcessW,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
                InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ReleaseMutex,
                STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
                WaitForSingleObject,
            },
            Variant::VT_LPWSTR,
        },
        UI::{
            Shell::{PropertiesSystem::IPropertyStore, *},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::{GUID, HSTRING, Interface, PCWSTR, PWSTR, w},
};

/// The app's identity: toast AUMID and the default credential service.
pub(crate) const SERVICE: &str = "org.broscribe.BroScribe";
/// The default data folder's Credential Manager service; other profiles
/// append `.profile-<hash>` (keyring's targets read `{key}.{service}`).
pub const SECRET_SERVICE_BASE: &str = SERVICE;
pub struct WinSecretStore {
    slots: CredentialSlots,
}
impl WinSecretStore {
    /// The store of one data folder's namespace (`secret_namespace`).
    pub fn with_service(service: &str) -> Self {
        Self {
            slots: CredentialSlots {
                service: service.to_owned(),
            },
        }
    }
}
/// One Credential Manager entry per slot (manifest, chunk and pending alike,
/// all under one service); `secret_chunks` spreads a secret over several
/// when it exceeds one password's 2,560 bytes.
struct CredentialSlots {
    service: String,
}
/// How long a change waits for another process changing the same key.
const KEY_LOCK_WAIT: Duration = Duration::from_secs(15);
/// A held per-key named mutex (this logon session); released on drop. An
/// owner that died mid-change abandons it, and the next owner's recovery
/// finishes that change.
struct KeyLock(crate::native::OwnedHandle);
impl Drop for KeyLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0.0);
        }
    }
}
fn lock_failure(detail: impl Into<String>) -> keyring::Error {
    keyring::Error::PlatformFailure(detail.into().into())
}
/// The per-key named mutex, scoped like the credentials (the default
/// namespace keeps the pre-profile name, so mixed versions still serialize).
/// It hashes the key so credential names never appear in the object namespace.
fn key_lock_name(service: &str, key: &str) -> String {
    format!(
        r"Local\{service}.secret.{:016x}",
        crate::secret_chunks::checksum(key)
    )
}
impl crate::secret_chunks::Slots for CredentialSlots {
    type Error = keyring::Error;
    type Guard = KeyLock;
    fn get(&self, key: &str) -> std::result::Result<Option<String>, keyring::Error> {
        match keyring::Entry::new(&self.service, key).and_then(|e| e.get_password()) {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn set(&self, key: &str, value: &str) -> std::result::Result<(), keyring::Error> {
        keyring::Entry::new(&self.service, key).and_then(|e| e.set_password(value))
    }
    fn delete(&self, key: &str) -> std::result::Result<(), keyring::Error> {
        match keyring::Entry::new(&self.service, key).and_then(|e| e.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e),
        }
    }
    fn exclusive(&self, key: &str) -> std::result::Result<KeyLock, keyring::Error> {
        let name = crate::wide(key_lock_name(&self.service, key));
        let handle = crate::native::OwnedHandle(
            unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
                .map_err(|e| lock_failure(format!("creating secret lock: {e}")))?,
        );
        let wait = unsafe { WaitForSingleObject(handle.0, KEY_LOCK_WAIT.as_millis() as u32) };
        if wait == windows::Win32::Foundation::WAIT_OBJECT_0
            || wait == windows::Win32::Foundation::WAIT_ABANDONED
        {
            Ok(KeyLock(handle))
        } else {
            Err(lock_failure(
                "another BroScribe process is still changing this secret",
            ))
        }
    }
}
fn secret_error(context: &str, e: crate::secret_chunks::ChunkError<keyring::Error>) -> Error {
    match e {
        crate::secret_chunks::ChunkError::Store(e) => crate::error(context, e),
        crate::secret_chunks::ChunkError::Corrupt(why) => crate::error(context, why),
    }
}
impl SecretStore for WinSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        crate::secret_chunks::read(&self.slots, key)
            .map_err(|e| secret_error("reading Credential Manager", e))
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        // Unique per write: process, time and a counter.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let generation = format!(
            "{:x}{:x}{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        crate::secret_chunks::write(&self.slots, key, value, &generation)
            .map_err(|e| secret_error("writing Credential Manager", e))
    }
    fn delete(&self, key: &str) -> Result<()> {
        crate::secret_chunks::delete(&self.slots, key)
            .map_err(|e| secret_error("deleting Credential Manager entry", e))
    }
}
pub struct WinBrowserLauncher;
impl BrowserLauncher for WinBrowserLauncher {
    fn open(&self, url: &str) -> Result<()> {
        broscribe_platform::system::check_web_url(url)?;
        open_url(url, "opening default browser")
    }
}

pub struct WinSystemSettings;

fn settings_url(pane: SettingsPane) -> Option<&'static str> {
    Some(match pane {
        SettingsPane::Microphone => "ms-settings:privacy-microphone",
        SettingsPane::Notifications => "ms-settings:notifications",
        SettingsPane::LoginItems => "ms-settings:startupapps",
        SettingsPane::Taskbar => "ms-settings:taskbar",
        SettingsPane::SystemAudio | SettingsPane::Accessibility => return None,
    })
}

impl SystemSettingsOpener for WinSystemSettings {
    fn host_platform(&self) -> HostPlatform {
        HostPlatform::Windows
    }

    fn supports(&self, pane: SettingsPane) -> bool {
        settings_url(pane).is_some()
    }

    fn open(&self, pane: SettingsPane) -> Result<()> {
        let url = settings_url(pane)
            .ok_or_else(|| Error::Unsupported("system settings pane unavailable".into()))?;
        open_url(url, "opening Windows Settings")
    }
}

fn open_url(url: &str, operation: &'static str) -> Result<()> {
    let url = crate::wide(url);
    let verb = crate::wide("open");
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(url.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        return Err(Error::io(
            operation,
            std::io::Error::other(format!("ShellExecute error {}", result.0 as isize)),
        ));
    }
    Ok(())
}
/// Resolve only packaged siblings (never an unrelated executable on PATH).
pub(crate) fn resolve_child_executable(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\', ':']) || name == "." || name == ".." {
        return Err(Error::InvalidInput(
            "child executable must be a simple filename".into(),
        ));
    }
    let filename = if name.to_ascii_lowercase().ends_with(".exe") {
        name.to_owned()
    } else {
        format!("{name}.exe")
    };
    let current = std::env::current_exe()?;
    let path = current
        .parent()
        .ok_or_else(|| Error::Internal("executable has no directory".into()))?
        .join(filename);
    if !path.is_file() {
        return Err(Error::NotFound(format!(
            "packaged child executable {}",
            path.display()
        )));
    }
    std::fs::canonicalize(&path).map_err(|e| Error::io_at(&path, e))
}
struct RegKey(HKEY);
impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn run_key() -> Result<RegKey> {
    open_user_key(
        w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
        "opening user logon Run key",
    )
}

fn existing_run_key(access: REG_SAM_FLAGS) -> Result<Option<RegKey>> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            None,
            access,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    status
        .ok()
        .map_err(|e| crate::error("opening user logon Run key", e))?;
    Ok(Some(RegKey(key)))
}

pub(crate) fn command_arguments(command: &[u16]) -> Option<Vec<String>> {
    let command = command.strip_suffix(&[0]).unwrap_or(command);
    if command.is_empty() || command.contains(&0) {
        return None;
    }
    let mut terminated = command.to_vec();
    terminated.push(0);
    let mut count = 0;
    // SAFETY: the command is terminated, count is writable, and the returned
    // argv stays valid until LocalFree below.
    unsafe {
        let argv = CommandLineToArgvW(PCWSTR(terminated.as_ptr()), &mut count);
        if argv.is_null() {
            return None;
        }
        let args = std::slice::from_raw_parts(argv, count as usize)
            .iter()
            .map(|arg| arg.to_string().ok())
            .collect();
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            argv.cast(),
        )));
        args
    }
}

fn run_profile(command: &[u16], default: &Path) -> Option<RegisteredProfile> {
    let args = command_arguments(command)?;
    let (executable, args) = args.split_first()?;
    let executable = Path::new(executable);
    let name = executable.file_name()?.to_str()?;
    if !executable.is_absolute() || !name.eq_ignore_ascii_case("broscribed.exe") {
        // Legacy used a shared name: only a clearly identifiable daemon
        // binary can make this value ours.
        return None;
    }
    let mut profile = RegisteredProfile::from(default.to_path_buf());
    let (pairs, remaining) = args.as_chunks::<2>();
    for [flag, value] in pairs {
        let path = Path::new(value);
        if !path.is_absolute() {
            return None;
        }
        match flag.as_str() {
            "--data-dir" => profile.data_dir = path.to_path_buf(),
            "--library" => profile.library = Some(path.to_path_buf()),
            _ => return None,
        }
    }
    remaining.is_empty().then_some(profile)
}

fn run_profiles(key: &RegKey, default: &Path) -> Result<Vec<(PCWSTR, RegisteredProfile)>> {
    let mut profiles = Vec::new();
    for (name, legacy) in [(AGENT_RUN_NAME, false), (LEGACY_RUN_NAME, true)] {
        if let Some(command) = read_run_value(key, name)? {
            if let Some(profile) = run_profile(&command, default) {
                profiles.push((name, profile));
            } else if !legacy {
                return Err(Error::Service {
                    code: "login_job_unverified",
                    message: "The BroScribe Agent Run entry's profile could not be verified".into(),
                });
            }
        }
    }
    Ok(profiles)
}

fn open_user_key(path: PCWSTR, context: &str) -> Result<RegKey> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()
    }
    .map_err(|e| crate::error(context, e))?;
    Ok(RegKey(key))
}
/// How long a liveness check waits for a busy daemon pipe.
const PIPE_CONNECT: Duration = Duration::from_secs(3);
#[derive(Clone, Debug)]
pub struct WinAutostart {
    executable: PathBuf,
}
/// The data folder the daemon resolves (platform default with the
/// `BROSCRIBE_DATA_DIR` override): its pipe is keyed by it.
fn daemon_data_dir() -> Result<PathBuf> {
    Ok(broscribe_platform::resolve_paths(&crate::native::WinPaths::try_new()?)?.data_dir)
}
/// A non-empty override from the environment, made absolute (the logon
/// session starts the daemon in another working folder).
fn override_path(name: &str) -> Result<Option<PathBuf>> {
    match std::env::var_os(name).filter(|value| !value.is_empty()) {
        Some(value) => std::path::absolute(PathBuf::from(&value))
            .map(Some)
            .map_err(|e| Error::io(format!("resolving {name}"), e)),
        None => Ok(None),
    }
}
/// Daemon arguments preserving this process's data/library overrides.
fn override_args() -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    Ok((
        override_path(broscribe_platform::paths::ENV_DATA_DIR)?,
        override_path(broscribe_platform::paths::ENV_LIBRARY)?,
    ))
}

/// The opaque attribute list needs the Windows heap's alignment. Delete only
/// initialized lists, then release the allocation on every error path too.
struct ProcessAttributes {
    heap: HANDLE,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
    initialized: bool,
}
impl ProcessAttributes {
    fn new() -> Result<Self> {
        let mut bytes = 0;
        match unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut bytes) } {
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() && bytes != 0 => {}
            Err(e) => return Err(crate::error("sizing process attributes", e)),
            Ok(()) => {
                return Err(Error::Internal(
                    "process attributes unexpectedly needed no allocation".into(),
                ));
            }
        }
        let heap =
            unsafe { GetProcessHeap() }.map_err(|e| crate::error("opening process heap", e))?;
        let allocation = unsafe { HeapAlloc(heap, HEAP_FLAGS(0), bytes) };
        if allocation.is_null() {
            return Err(Error::io(
                "allocating process attributes",
                std::io::ErrorKind::OutOfMemory.into(),
            ));
        }
        let mut attributes = Self {
            heap,
            list: LPPROC_THREAD_ATTRIBUTE_LIST(allocation),
            initialized: false,
        };
        unsafe { InitializeProcThreadAttributeList(Some(attributes.list), 1, None, &mut bytes) }
            .map_err(|e| crate::error("initializing process attributes", e))?;
        attributes.initialized = true;
        Ok(attributes)
    }
}
impl Drop for ProcessAttributes {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                DeleteProcThreadAttributeList(self.list);
            }
            let _ = HeapFree(self.heap, HEAP_FLAGS(0), Some(self.list.0));
        }
    }
}

/// Stable `Command` inherits other handles even with null stdio. Whitelist
/// a single NUL handle for all three streams so caller capture pipes reach EOF.
fn spawn_detached(
    command: &mut [u16],
    working_dir: &Path,
    environment: &[u16],
) -> Result<(crate::native::OwnedHandle, u32)> {
    let working_dir = crate::wide(working_dir);
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    let nul = crate::native::OwnedHandle(
        unsafe {
            CreateFileW(
                w!("NUL"),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                Some(&security),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| crate::error("opening detached standard streams", e))?,
    );
    // UpdateProcThreadAttribute retains this array's address until deletion.
    // Declare it before the guard so both it and NUL outlive that guard.
    let inherit = [nul.0];
    let attributes = ProcessAttributes::new()?;
    unsafe {
        UpdateProcThreadAttribute(
            attributes.list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(inherit.as_ptr().cast()),
            std::mem::size_of_val(&inherit),
            None,
            None,
        )
    }
    .map_err(|e| crate::error("setting detached handle whitelist", e))?;
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: nul.0,
            hStdOutput: nul.0,
            hStdError: nul.0,
            ..Default::default()
        },
        lpAttributeList: attributes.list,
    };
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.as_ptr().cast()),
            PCWSTR(working_dir.as_ptr()),
            &startup.StartupInfo,
            &mut information,
        )
    }
    .map_err(|e| crate::error("starting detached BroScribe process", e))?;
    let process = crate::native::OwnedHandle(information.hProcess);
    let thread = crate::native::OwnedHandle(information.hThread);
    drop(thread);
    Ok((process, information.dwProcessId))
}

/// Launch a helper with the supplied command's arguments, overlay environment
/// and working directory. The handle list always limits inheritance to NUL.
pub(crate) fn spawn_detached_command(
    command: &std::process::Command,
) -> Result<(crate::native::OwnedHandle, u32)> {
    let mut line = detached_command_line(command)?;
    let mut vars = std::env::vars_os().collect::<Vec<_>>();
    for (name, value) in command.get_envs() {
        vars.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        if let Some(value) = value {
            vars.push((name.to_owned(), value.to_owned()));
        }
    }
    let environment = environment_block(vars, false);
    let system = crate::native::system_directory()?;
    spawn_detached(
        &mut line,
        command.get_current_dir().unwrap_or(&system),
        &environment,
    )
}

pub(crate) fn detached_command_line(command: &std::process::Command) -> Result<Vec<u16>> {
    let arguments = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|value| {
            value
                .to_str()
                .and_then(crate::install_location::quote)
                .ok_or_else(|| Error::InvalidInput("Invalid detached process argument".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let line = crate::wide(arguments.join(" "));
    // CreateProcessW includes the terminating NUL in its UTF-16 limit.
    if line.len() > 32_767 {
        return Err(Error::InvalidInput(
            "Detached process command exceeds the Windows command-line limit".into(),
        ));
    }
    Ok(line)
}

fn environment_block(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    clear_data_override: bool,
) -> Vec<u16> {
    use broscribe_platform::paths::{ENV_DATA_DIR, ENV_MAINTENANCE_CONFIG_BASE};
    use std::os::windows::ffi::OsStrExt as _;
    // The maintenance base belongs to the relocated CLI alone. Ordinary starts
    // still inherit the caller's data override, including update fallback starts.
    let mut vars: Vec<_> = vars
        .into_iter()
        .filter(|(name, _)| {
            !(name.eq_ignore_ascii_case(ENV_MAINTENANCE_CONFIG_BASE)
                || clear_data_override && name.eq_ignore_ascii_case(ENV_DATA_DIR))
        })
        .collect();
    vars.sort_by_cached_key(|(name, _)| name.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (name, value) in vars {
        block.extend(name.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn detached_exit_code(process: &crate::native::OwnedHandle) -> Result<Option<u32>> {
    match unsafe { WaitForSingleObject(process.0, 0) } {
        WAIT_TIMEOUT => Ok(None),
        WAIT_OBJECT_0 => {
            let mut code = 0;
            unsafe { GetExitCodeProcess(process.0, &mut code) }
                .map_err(|e| crate::error("reading detached agent exit code", e))?;
            Ok(Some(code))
        }
        WAIT_FAILED => Err(Error::io(
            "waiting for detached agent",
            std::io::Error::last_os_error(),
        )),
        other => Err(Error::Internal(format!(
            "unexpected detached agent wait result: 0x{:08X}",
            other.0
        ))),
    }
}

impl WinAutostart {
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
    /// The canonical daemon beside the (canonical) calling executable, so a
    /// shortcut or symlink never registers its own path. Refuses network and
    /// temporary locations instead of moving the installation.
    fn daemon(&self) -> Result<PathBuf> {
        let caller = std::fs::canonicalize(&self.executable)
            .map_err(|e| Error::io_at(&self.executable, e))?;
        let path = if caller
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("broscribed.exe"))
        {
            caller
        } else {
            let sibling = caller.with_file_name("broscribed.exe");
            if sibling.is_file() {
                sibling
            } else {
                resolve_child_executable("broscribed")?
            }
        };
        let daemon = std::fs::canonicalize(&path).map_err(|e| Error::io_at(&path, e))?;
        let temp = std::env::temp_dir()
            .canonicalize()
            .ok()
            .map(|t| t.to_string_lossy().into_owned());
        if let Some(problem) =
            crate::install_location::location_problem(&daemon.to_string_lossy(), temp.as_deref())
        {
            return Err(Error::Unsupported(problem.into()));
        }
        Ok(daemon)
    }
    fn command(daemon: &Path) -> Result<Vec<u16>> {
        let (data_dir, library) = override_args()?;
        Self::profile_command(daemon, data_dir.as_deref(), library.as_deref())
    }
    fn profile_command(
        daemon: &Path,
        data_dir: Option<&Path>,
        library: Option<&Path>,
    ) -> Result<Vec<u16>> {
        let command =
            crate::install_location::run_command(daemon, data_dir, library).ok_or_else(|| {
                Error::InvalidInput(
                    "the BroScribe paths cannot be written to a logon command".into(),
                )
            })?;
        Ok(crate::wide(command))
    }
    fn update_command(
        daemon: &Path,
        default: &Path,
        data_dir: &Path,
        library: &Path,
    ) -> Result<Vec<u16>> {
        let custom = crate::profile_identity(data_dir) != crate::profile_identity(default);
        Self::profile_command(daemon, custom.then_some(data_dir), Some(library))
    }

    pub fn install(&self) -> Result<()> {
        self.set_enabled(true)
    }
    pub fn uninstall(&self) -> Result<()> {
        self.stop()?;
        self.set_enabled(false)
    }
    pub fn start(&self) -> Result<()> {
        self.start_profile(&daemon_data_dir()?, false, Self::command)
    }
    fn start_profile(
        &self,
        data_dir: &Path,
        clear_data_override: bool,
        command: impl FnOnce(&Path) -> Result<Vec<u16>>,
    ) -> Result<()> {
        if self.is_running_for(data_dir)? {
            return Ok(());
        }
        let executable = self.daemon()?;
        let mut command = command(&executable)?;
        let working_dir = crate::native::system_directory()?;
        let environment = environment_block(std::env::vars_os(), clear_data_override);
        let (child, _) = spawn_detached(&mut command, &working_dir, &environment)?;
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if self.is_running_for(data_dir)? {
                return Ok(());
            }
            if let Some(code) = detached_exit_code(&child)? {
                return Err(Error::Internal(format!(
                    "daemon exited during startup: {code} (0x{code:08X})"
                )));
            }
            if Instant::now() >= until {
                return Err(Error::io(
                    "waiting for daemon IPC endpoint",
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "daemon did not become ready",
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    pub fn is_running(&self) -> Result<bool> {
        self.is_running_for(&daemon_data_dir()?)
    }
    fn is_running_for(&self, data_dir: &Path) -> Result<bool> {
        match crate::WinPipeStream::connect(data_dir, Instant::now() + PIPE_CONNECT) {
            Ok(stream) => {
                let pid = stream.server_process_id()?;
                self.check_daemon(pid)?;
                Ok(true)
            }
            Err(e) if matches!(e.raw_os_error(), Some(2 | 3)) => Ok(false),
            Err(e) => Err(crate::error("checking daemon endpoint", e)),
        }
    }
    fn check_daemon(&self, pid: u32) -> Result<()> {
        let actual = crate::native::process_path(pid)
            .ok_or_else(|| Error::Internal("cannot inspect named pipe server executable".into()))?;
        let expected = self.daemon()?;
        if actual
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase()
            != expected
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_lowercase()
        {
            return Err(Error::InvalidInput(
                "daemon endpoint belongs to another executable".into(),
            ));
        }
        Ok(())
    }
    pub fn stop(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = match crate::WinPipeStream::connect(&daemon_data_dir()?, deadline) {
            Ok(s) => s,
            Err(e) if matches!(e.raw_os_error(), Some(2 | 3)) => return Ok(()),
            Err(e) => return Err(crate::error("connecting to stop daemon", e)),
        };
        let pid = stream.server_process_id()?;
        self.check_daemon(pid)?;
        let request = broscribe_ipc::Request::new(broscribe_ipc::Call::Shutdown);
        let mut line = request
            .to_line()
            .map_err(|e| crate::error("serializing daemon stop", e))?;
        line.push('\n');
        stream.write_all_within(line.as_bytes(), deadline)?;
        let line = stream.read_line(
            deadline.saturating_duration_since(Instant::now()),
            broscribe_ipc::MAX_LINE_BYTES,
        )?;
        match broscribe_ipc::ServerMessage::parse(&line)
            .map_err(|e| crate::error("daemon stop response", e))?
        {
            broscribe_ipc::ServerMessage::Response(response) if response.id == request.id => {
                response
                    .outcome
                    .map_err(|e| crate::error("daemon refused shutdown", e))?;
            }
            _ => {
                return Err(Error::Internal(
                    "unexpected daemon shutdown response".into(),
                ));
            }
        }
        let until = Instant::now() + Duration::from_secs(10);
        while crate::WinProcessProbe.is_alive(pid) {
            if Instant::now() >= until {
                return Err(Error::io(
                    "waiting for daemon shutdown",
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "capture shutdown did not finish",
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }
    pub fn restart(&self) -> Result<()> {
        self.stop()?;
        self.start()
    }
}
const AGENT_RUN_NAME: PCWSTR = w!("BroScribe Agent");
const LEGACY_RUN_NAME: PCWSTR = w!("BroScribe");

fn read_run_value(key: &RegKey, name: PCWSTR) -> Result<Option<Vec<u16>>> {
    let mut size = 0;
    let mut kind = REG_VALUE_TYPE::default();
    let status =
        unsafe { RegQueryValueExW(key.0, name, None, Some(&mut kind), None, Some(&mut size)) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    status
        .ok()
        .map_err(|e| crate::error("reading Run key", e))?;
    if kind != REG_SZ || size > 65536 || size % 2 != 0 {
        return invalid_run_value(name);
    }
    let mut bytes = vec![0u8; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    status
        .ok()
        .map_err(|e| crate::error("reading Run value", e))?;
    if kind != REG_SZ || size % 2 != 0 {
        return invalid_run_value(name);
    }
    bytes.truncate(size as usize);
    Ok(Some(
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|value| u16::from_le_bytes(*value))
            .collect(),
    ))
}

fn invalid_run_value(name: PCWSTR) -> Result<Option<Vec<u16>>> {
    // Run value names are NUL-terminated strings, and their addresses can differ.
    if unsafe { name.as_wide() == AGENT_RUN_NAME.as_wide() } {
        Err(Error::Service {
            code: "login_job_unverified",
            message: "The BroScribe Agent Run entry is not a supported command string".into(),
        })
    } else {
        Ok(None)
    }
}

fn write_run_value(key: &RegKey, name: PCWSTR, value: &[u16]) -> Result<()> {
    write_string_value(key, name, value, "installing user logon daemon")
}

fn write_string_value(key: &RegKey, name: PCWSTR, value: &[u16], context: &str) -> Result<()> {
    let bytes: Vec<u8> = value.iter().flat_map(|value| value.to_le_bytes()).collect();
    unsafe { RegSetValueExW(key.0, name, None, REG_SZ, Some(&bytes)).ok() }
        .map_err(|e| crate::error(context, e))
}

fn remove_run_value(key: &RegKey, name: PCWSTR) -> Result<()> {
    let status = unsafe { RegDeleteValueW(key.0, name) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    status
        .ok()
        .map_err(|e| crate::error("removing user logon daemon", e))
}

fn remove_legacy_run_value(key: &RegKey, value: &[u16], daemon: &Path) -> Result<()> {
    let command = value.strip_suffix(&[0]).unwrap_or(value);
    if String::from_utf16(command).is_ok_and(|command| {
        !command.contains('\0') && crate::install_location::legacy_run_targets(&command, daemon)
    }) {
        remove_run_value(key, LEGACY_RUN_NAME)?;
    }
    Ok(())
}

fn enable_agent_run(key: &RegKey, daemon: &Path, command: &[u16]) -> Result<()> {
    // Write the replacement first so an interrupted migration still starts
    // the agent. The daemon's instance lock prevents a second active process.
    write_run_value(key, AGENT_RUN_NAME, command)?;
    if let Some(legacy) = read_run_value(key, LEGACY_RUN_NAME)? {
        remove_legacy_run_value(key, &legacy, daemon)?;
    }
    Ok(())
}

impl Autostart for WinAutostart {
    fn agent_status(&self) -> Result<broscribe_platform::AgentServiceStatus> {
        use broscribe_platform::AgentServiceStatus::*;
        Ok(if self.is_running().unwrap_or(false) {
            Running
        } else if self.is_enabled()? {
            Stopped
        } else {
            NotInstalled
        })
    }
    fn is_enabled(&self) -> Result<bool> {
        let key = run_key()?;
        let current = read_run_value(&key, AGENT_RUN_NAME)?;
        let legacy = read_run_value(&key, LEGACY_RUN_NAME)?;
        if current.is_none() && legacy.is_none() {
            return Ok(false);
        }
        let expected = Self::command(&self.daemon()?)?;
        Ok([current, legacy]
            .into_iter()
            .flatten()
            .any(|value| value == expected))
    }
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        let guard = self.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.set_enabled_with_registration(enabled, &guard)
    }
    fn set_enabled_with_registration(
        &self,
        enabled: bool,
        guard: &LoginRegistrationGuard,
    ) -> Result<()> {
        if enabled {
            let daemon = self.daemon()?;
            enable_agent_run(&run_key()?, &daemon, &Self::command(&daemon)?)
        } else {
            self.unregister_profile(&daemon_data_dir()?, guard)
        }
    }
    fn lock_login_registration(&self, wait: Duration) -> Result<LoginRegistrationGuard> {
        let cache = crate::native::WinPaths::try_new()?.cache_dir();
        let root = cache.parent().expect("cache parent");
        std::fs::create_dir_all(root)?;
        LoginRegistrationGuard::acquire(&root.join("login-registration.lock"), wait)
    }

    fn registered_profiles(&self) -> Result<Vec<RegisteredProfile>> {
        let Some(key) = existing_run_key(KEY_QUERY_VALUE)? else {
            return Ok(Vec::new());
        };
        let default = crate::native::WinPaths::try_new()?.data_dir();
        Ok(run_profiles(&key, &default)?
            .into_iter()
            .map(|(_, profile)| profile)
            .collect())
    }

    fn login_job(&self, data_dir: &Path) -> LoginJob {
        match self.registered_profiles() {
            Ok(profiles) if profiles.is_empty() => LoginJob::Absent,
            Ok(profiles) => LoginJob::Registered {
                serves_profile: Some(profiles.iter().any(|profile| {
                    crate::profile_identity(&profile.data_dir) == crate::profile_identity(data_dir)
                })),
            },
            Err(_) => LoginJob::Registered {
                serves_profile: None,
            },
        }
    }

    fn unregister_profile(&self, data_dir: &Path, _: &LoginRegistrationGuard) -> Result<()> {
        let Some(key) = existing_run_key(KEY_QUERY_VALUE | KEY_SET_VALUE)? else {
            return Ok(());
        };
        let default = crate::native::WinPaths::try_new()?.data_dir();
        let selected = crate::profile_identity(data_dir);
        for (name, profile) in run_profiles(&key, &default)? {
            if crate::profile_identity(&profile.data_dir) == selected {
                remove_run_value(&key, name)?;
            }
        }
        Ok(())
    }

    fn start(&self) -> Result<()> {
        WinAutostart::start(self)
    }
    fn start_for_update(
        &self,
        _: &LoginRegistrationGuard,
        data_dir: &Path,
        library: &Path,
    ) -> Result<()> {
        if !data_dir.is_absolute() || !library.is_absolute() {
            return Err(Error::InvalidInput(
                "an update restart requires absolute data and library paths".into(),
            ));
        }
        let default = crate::native::WinPaths::try_new()?.data_dir();
        self.start_profile(data_dir, true, |daemon| {
            Self::update_command(daemon, &default, data_dir, library)
        })
    }
    fn stop(&self) -> Result<()> {
        WinAutostart::stop(self)
    }
    fn stop_login_job(&self, _: &broscribe_platform::LoginRegistrationGuard) -> Result<()> {
        Err(Error::Unsupported(
            "Windows has no persistent login job to unload".into(),
        ))
    }
}

pub(crate) fn profile_app_id(data_dir: Option<&Path>) -> String {
    match data_dir {
        None => SERVICE.to_owned(),
        Some(dir) => {
            broscribe_platform::secret_namespace(SERVICE, &crate::profile_identity(dir), &[])
        }
    }
}

pub struct WinNotifier {
    app_id: String,
    profile: Option<PathBuf>,
}
impl WinNotifier {
    pub fn new(data_dir: Option<&Path>) -> Self {
        Self {
            app_id: profile_app_id(data_dir),
            profile: data_dir.map(Path::to_owned),
        }
    }

    pub(crate) fn app_id(&self) -> HSTRING {
        HSTRING::from(&self.app_id)
    }

    /// Register a per-user Start-menu shortcut carrying the exact toast AUMID.
    pub fn register_shortcut(&self) -> Result<()> {
        let _com = crate::Com::new()?;
        unsafe {
            let programs = SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None)
                .map_err(|e| crate::error("Start menu programs folder", e))?;
            let folder = programs
                .to_string()
                .map(PathBuf::from)
                .map_err(|e| crate::error("Start menu folder", e));
            CoTaskMemFree(Some(programs.0.cast()));
            let folder = folder?;
            std::fs::create_dir_all(&folder)?;
            let executable =
                crate::wide(plain_verbatim(&resolve_child_executable("broscribe-app")?));
            let suffix = self.app_id.strip_prefix(SERVICE).unwrap_or(&self.app_id);
            let shortcut = crate::wide(folder.join(format!("BroScribe{suffix}.lnk")));
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| crate::error("creating toast shortcut", e))?;
            link.SetPath(PCWSTR(executable.as_ptr()))
                .map_err(|e| crate::error("setting shortcut target", e))?;
            if let Some(profile) = &self.profile {
                let directory = profile
                    .to_str()
                    .and_then(crate::install_location::quote)
                    .ok_or_else(|| {
                        broscribe_platform::Error::InvalidInput(
                            "A notification shortcut path cannot be passed to the app".into(),
                        )
                    })?;
                let arguments = crate::wide(format!("--dock-data-dir {directory}"));
                link.SetArguments(PCWSTR(arguments.as_ptr()))
                    .map_err(|e| crate::error("setting notification shortcut arguments", e))?;
            }
            let appid = crate::wide(&self.app_id);
            // PROPVARIANT clears an owned VT_LPWSTR with CoTaskMemFree.
            let owned_appid = SHStrDupW(PCWSTR(appid.as_ptr()))
                .map_err(|e| crate::error("allocating shortcut AppUserModelID", e))?;
            let value = PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_LPWSTR,
                        Anonymous: PROPVARIANT_0_0_0 {
                            pwszVal: owned_appid,
                        },
                        ..Default::default()
                    }),
                },
            };
            let key = PROPERTYKEY {
                fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
                pid: 5,
            };
            let store: IPropertyStore = link
                .cast()
                .map_err(|e| crate::error("shortcut property store", e))?;
            store
                .SetValue(&key, &value)
                .and_then(|_| store.Commit())
                .map_err(|e| crate::error("setting shortcut AppUserModelID", e))?;
            let persist: IPersistFile = link
                .cast()
                .map_err(|e| crate::error("persisting toast shortcut", e))?;
            persist
                .Save(PCWSTR(shortcut.as_ptr()), true)
                .map_err(|e| crate::error("saving toast shortcut", e))?;
        }
        // Register the unpackaged identity before WinRT queries its setting.
        // Explorer may not have indexed a newly created shortcut yet.
        let path = crate::wide(format!(r"Software\Classes\AppUserModelId\{}", self.app_id));
        let key = open_user_key(PCWSTR(path.as_ptr()), "opening toast app identity")?;
        write_string_value(
            &key,
            w!("DisplayName"),
            &crate::wide("BroScribe"),
            "writing toast display name",
        )
    }
}
impl Notifier for WinNotifier {
    fn authorization(&self) -> broscribe_platform::system::NotificationAuthorization {
        crate::toast::authorization(&self.app_id())
    }

    fn request_authorization(&self) -> broscribe_platform::system::NotificationAuthorization {
        if self.register_shortcut().is_err() {
            return broscribe_platform::system::NotificationAuthorization::Unknown;
        }
        self.authorization()
    }

    fn notify(&self, title: &str, body: &str) -> Result<()> {
        let _runtime = crate::toast::WinRt::new()?;
        self.register_shortcut()?;
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&apos;")
        };
        let xml = format!(
            "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
            escape(title),
            escape(body)
        );
        let document = windows::Data::Xml::Dom::XmlDocument::new()
            .map_err(|e| crate::error("creating toast XML", e))?;
        document
            .LoadXml(&HSTRING::from(xml))
            .map_err(|e| crate::error("loading toast XML", e))?;
        let toast =
            windows::UI::Notifications::ToastNotification::CreateToastNotification(&document)
                .map_err(|e| crate::error("creating toast", e))?;
        windows::UI::Notifications::ToastNotificationManager::CreateToastNotifierWithId(
            &self.app_id(),
        )
        .and_then(|n| n.Show(&toast))
        .map_err(|e| crate::error("showing toast", e))
    }
    fn notify_actionable(
        &self,
        notification: broscribe_platform::system::Notification,
        callback: std::sync::Arc<
            dyn Fn(broscribe_platform::system::NotificationAction) + Send + Sync,
        >,
    ) -> Result<broscribe_platform::system::NotificationDelivery> {
        crate::toast::show(self, notification, callback)
    }
    fn withdraw(&self, id: &str) -> Result<()> {
        crate::toast::withdraw(&self.app_id(), id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_value_identifies_the_stored_profile_independently_of_the_caller() {
        let default = Path::new(r"C:\Synthetic\Default");
        let data = Path::new(r"D:\Profile A");
        let daemon = Path::new(r"C:\Synthetic\BroScribe\broscribed.exe");
        let library = Path::new(r"D:\Other Library");
        let command =
            crate::install_location::run_command(daemon, Some(data), Some(library)).unwrap();
        let registered = run_profile(&crate::wide(command), default).unwrap();
        assert_eq!(registered.data_dir, data);
        assert_eq!(registered.library.as_deref(), Some(library));
        let command = crate::install_location::run_command(daemon, None, None).unwrap();
        assert_eq!(
            run_profile(&crate::wide(command), default)
                .unwrap()
                .data_dir,
            default
        );
        assert!(
            run_profile(
                &crate::wide(r#""C:\Synthetic\wrapper.exe" --data-dir "D:\Other""#),
                default
            )
            .is_none()
        );
    }

    #[test]
    fn update_restart_leaves_the_default_data_path_implicit_and_pins_the_library() {
        let default = Path::new(r"C:\Synthetic\Default");
        let custom = Path::new(r"D:\Profile A");
        let daemon = Path::new(r"C:\Synthetic\BroScribe\broscribed.exe");
        let library = Path::new(r"D:\Admitted Library");
        for (data, has_flag) in [(default, false), (custom, true)] {
            let command = WinAutostart::update_command(daemon, default, data, library).unwrap();
            let args = command_arguments(&command).unwrap();
            assert_eq!(args.iter().any(|arg| arg == "--data-dir"), has_flag);
            let selected = run_profile(&command, default).unwrap();
            assert_eq!(selected.data_dir, data);
            assert_eq!(selected.library.as_deref(), Some(library));
        }
    }

    #[test]
    fn detached_environment_always_strips_the_maintenance_config_base() {
        let vars = [
            ("broscribe_data_dir".into(), "stale override".into()),
            (
                "broscribe_maintenance_config_base".into(),
                "stale base".into(),
            ),
            ("SystemRoot".into(), r"C:\Windows".into()),
        ];
        assert_eq!(
            String::from_utf16(&environment_block(vars.clone(), true)).unwrap(),
            "SystemRoot=C:\\Windows\0\0"
        );
        assert_eq!(
            String::from_utf16(&environment_block(vars, false)).unwrap(),
            "broscribe_data_dir=stale override\0SystemRoot=C:\\Windows\0\0"
        );
    }

    #[test]
    fn default_and_custom_notification_identities_use_the_shell_profile() {
        let default = WinNotifier::new(None);
        let path = Path::new(r"C:\Synthetic\Profile");
        let custom = WinNotifier::new(Some(path));
        assert_eq!(default.app_id(), HSTRING::from(SERVICE));
        assert_eq!(custom.app_id(), HSTRING::from(profile_app_id(Some(path))));
        assert!(
            custom
                .app_id
                .starts_with("org.broscribe.BroScribe.profile-")
        );
        assert_ne!(custom.app_id(), default.app_id());
        assert_ne!(
            custom.app_id(),
            WinNotifier::new(Some(Path::new(r"C:\Synthetic\Other"))).app_id()
        );
    }

    #[test]
    fn settings_open_only_the_supported_native_destinations() {
        assert_eq!(
            settings_url(SettingsPane::Microphone),
            Some("ms-settings:privacy-microphone")
        );
        assert_eq!(
            settings_url(SettingsPane::Notifications),
            Some("ms-settings:notifications")
        );
        assert_eq!(
            settings_url(SettingsPane::LoginItems),
            Some("ms-settings:startupapps")
        );
        assert_eq!(
            settings_url(SettingsPane::Taskbar),
            Some("ms-settings:taskbar")
        );
        assert!(!WinSystemSettings.supports(SettingsPane::Accessibility));
        assert!(!WinSystemSettings.supports(SettingsPane::SystemAudio));
    }

    #[test]
    fn the_default_namespace_keeps_todays_key_lock_and_profiles_get_their_own() {
        let key = "provider/synthetic/api_key";
        let checksum = crate::secret_chunks::checksum(key);
        assert_eq!(
            key_lock_name(SECRET_SERVICE_BASE, key),
            format!(r"Local\org.broscribe.BroScribe.secret.{checksum:016x}")
        );
        let profile = broscribe_platform::secret_namespace(
            SECRET_SERVICE_BASE,
            br"c:\synthetic\profile",
            br"c:\users\synthetic\appdata\roaming\broscribe",
        );
        let name = key_lock_name(&profile, key);
        assert!(name.starts_with(r"Local\org.broscribe.BroScribe.profile-"));
        assert!(name.ends_with(&format!(".secret.{checksum:016x}")));
        assert_ne!(name, key_lock_name(SECRET_SERVICE_BASE, key));
    }

    #[test]
    fn detached_agent_does_not_hold_caller_pipes_and_keeps_usable_stdio() {
        const CHILD: &str = "BROSCRIBE_TEST_DETACHED_AGENT_PIPES";
        if std::env::var_os(CHILD).is_none() {
            // Ordinary Command spawns inherit every inheritable handle in their
            // process. Give the fixture a handle table of its own so other
            // tests running in parallel cannot acquire these pipe writers.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shell::tests::detached_agent_does_not_hold_caller_pipes_and_keeps_usable_stdio",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use crate::native::OwnedHandle;
        use windows::Win32::{
            Foundation::ERROR_BROKEN_PIPE,
            System::{
                Pipes::{CreatePipe, PeekNamedPipe},
                Threading::TerminateProcess,
            },
        };

        struct FixtureChild(OwnedHandle);
        impl Drop for FixtureChild {
            fn drop(&mut self) {
                unsafe {
                    let _ = TerminateProcess(self.0.0, 1);
                    let _ = WaitForSingleObject(self.0.0, 5_000);
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let ready = tmp.path().join("stdio-ready");
        let finish = tmp.path().join("finish");
        let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let script = format!(
            r#"$ErrorActionPreference='Stop';
$out=[Console]::OpenStandardOutput(); $err=[Console]::OpenStandardError();
if([object]::ReferenceEquals($out,[IO.Stream]::Null) -or [object]::ReferenceEquals($err,[IO.Stream]::Null)){{exit 21}};
[Console]::Out.WriteLine('synthetic detached stdout');
[Console]::Error.WriteLine('synthetic detached stderr');
[IO.File]::WriteAllText('{ready}',[Environment]::CurrentDirectory);
$until=[DateTime]::UtcNow.AddSeconds(30);
while(-not [IO.File]::Exists('{finish}')){{if([DateTime]::UtcNow -ge $until){{exit 22}}; Start-Sleep -Milliseconds 20}};
exit 0"#,
            ready = ready.to_str().unwrap().replace('\'', "''"),
            finish = finish.to_str().unwrap().replace('\'', "''"),
        );
        let mut command = crate::wide(format!(
            "{} -NoLogo -NoProfile -NonInteractive -Command \"{script}\"",
            crate::install_location::quote(powershell.to_str().unwrap()).unwrap(),
        ));
        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        let pipe = || {
            let mut read = HANDLE::default();
            let mut write = HANDLE::default();
            unsafe { CreatePipe(&mut read, &mut write, Some(&security), 0) }.unwrap();
            (OwnedHandle(read), OwnedHandle(write))
        };
        let (read_out, write_out) = pipe();
        let (read_err, write_err) = pipe();
        let working_dir = crate::native::system_directory().unwrap();
        let environment = environment_block(std::env::vars_os(), false);
        let child = FixtureChild(
            spawn_detached(&mut command, &working_dir, &environment)
                .unwrap()
                .0,
        );
        drop((write_out, write_err));

        let until = Instant::now() + Duration::from_secs(10);
        while !ready.is_file() {
            assert_eq!(
                detached_exit_code(&child.0).unwrap(),
                None,
                "fixture exited before its standard-stream writes succeeded"
            );
            assert!(Instant::now() < until, "fixture did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(unsafe { WaitForSingleObject(child.0.0, 0) }, WAIT_TIMEOUT);
        assert_eq!(
            PathBuf::from(std::fs::read_to_string(&ready).unwrap()),
            crate::native::system_directory().unwrap()
        );
        // Both unlisted pipes must reach EOF while the child is still alive.
        // Successful marker writes prove usable stdio, not the device's identity.
        for read in [&read_out, &read_err] {
            let error = unsafe { PeekNamedPipe(read.0, None, 0, None, None, None) }
                .expect_err("a detached child retained a caller pipe or wrote its marker there");
            assert_eq!(error.code(), ERROR_BROKEN_PIPE.to_hresult());
        }
        std::fs::write(&finish, b"finish").unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(child.0.0, 10_000) },
            WAIT_OBJECT_0
        );
        assert_eq!(detached_exit_code(&child.0).unwrap(), Some(0));
    }

    #[test]
    fn cli_and_app_autostart_target_the_canonical_sibling_daemon() {
        // Give the child separate synthetic install and temp roots. The source
        // tree may itself live under the actual system temp folder.
        const INSTALL: &str = "BROSCRIBE_TEST_AUTOSTART_INSTALL";
        let Some(installed) = std::env::var_os(INSTALL).map(PathBuf::from) else {
            let fixture = tempfile::tempdir().unwrap();
            let temp = fixture.path().join("temp");
            std::fs::create_dir(&temp).unwrap();
            let existing = ["broscribe.exe", "broscribe-app.exe", "broscribed.exe"];
            for name in existing {
                std::fs::write(fixture.path().join(name), b"leave alone").unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shell::tests::cli_and_app_autostart_target_the_canonical_sibling_daemon",
                ])
                .env(INSTALL, fixture.path())
                .env("TMP", &temp)
                .env("TEMP", &temp)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}"
            );
            for name in existing {
                assert_eq!(
                    std::fs::read(fixture.path().join(name)).unwrap(),
                    b"leave alone"
                );
            }
            return;
        };
        let fixture = tempfile::tempdir_in(installed).unwrap();
        let installed = fixture.path();
        let daemon = installed.join("broscribed.exe");
        std::fs::write(&daemon, b"synthetic daemon").unwrap();
        let expected = std::fs::canonicalize(&daemon).unwrap();
        let (data_dir, library) = override_args().unwrap();
        let run = crate::install_location::run_command(
            &expected,
            data_dir.as_deref(),
            library.as_deref(),
        )
        .unwrap();
        for name in ["broscribe.exe", "broscribe-app.exe", "broscribed.exe"] {
            let caller = installed.join(name);
            if caller != daemon {
                std::fs::write(&caller, b"synthetic caller").unwrap();
            }
            // A relative spelling resolves to the same canonical daemon.
            let spelled = installed.join(".").join(name);
            let autostart = WinAutostart::new(spelled);
            let daemon = autostart.daemon().unwrap();
            assert_eq!(daemon, expected);
            let command = WinAutostart::command(&daemon).unwrap();
            assert_eq!(
                String::from_utf16(&command[..command.len() - 1]).unwrap(),
                run
            );
        }
    }

    #[test]
    fn temporary_install_locations_are_refused_before_registration() {
        let temporary = tempfile::tempdir().unwrap();
        let daemon = temporary.path().join("broscribed.exe");
        std::fs::write(&daemon, b"synthetic daemon").unwrap();
        let error = WinAutostart::new(daemon).daemon().unwrap_err();
        assert_eq!(error.code(), "unsupported");
    }

    #[test]
    fn agent_run_entry_replaces_only_the_same_installations_legacy_entry() {
        // Exercise the registry API in a volatile test key, away from Run.
        struct TestKey(Vec<u16>);
        impl Drop for TestKey {
            fn drop(&mut self) {
                unsafe {
                    let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(self.0.as_ptr()));
                }
            }
        }
        let name = TestKey(crate::wide(format!(
            r"Software\BroScribe-Autostart-Test-{}",
            std::process::id()
        )));
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(name.0.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .unwrap();
        let key = RegKey(key);
        let daemon = Path::new(r"C:\Synthetic\BroScribe\broscribed.exe");
        let command =
            crate::wide(r#""C:\Synthetic\BroScribe\broscribed.exe" --data-dir "D:\New Profile""#);
        let legacy =
            crate::wide(r#""C:\Synthetic\BroScribe\broscribed.exe" --data-dir "D:\Old Profile""#);
        write_run_value(&key, LEGACY_RUN_NAME, &legacy).unwrap();
        enable_agent_run(&key, daemon, &command).unwrap();
        assert_eq!(
            read_run_value(&key, AGENT_RUN_NAME).unwrap(),
            Some(command.clone())
        );
        assert_eq!(read_run_value(&key, LEGACY_RUN_NAME).unwrap(), None);

        let unrelated = crate::wide(r#""C:\Other\startup.exe""#);
        write_run_value(&key, LEGACY_RUN_NAME, &unrelated).unwrap();
        enable_agent_run(&key, daemon, &command).unwrap();
        assert_eq!(
            read_run_value(&key, LEGACY_RUN_NAME).unwrap(),
            Some(unrelated)
        );
    }
}

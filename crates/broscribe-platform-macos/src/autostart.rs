//! SMAppService registration for the bundled capture agent. Development
//! installs use a LaunchAgent that still targets the bundled helper, never
//! an anonymous bare daemon (stable TCC identity).
use broscribe_platform::RegisteredProfile;
use broscribe_platform::about::is_temporary;
use broscribe_platform::bounded::{LOGIN_MANAGER_TIMEOUT, login_manager_output};
use broscribe_platform::{
    AppPaths, Autostart, Error, LoginJob, LoginRegistrationGuard, Result, stable_path,
};
use broscribe_platform_unix::dirs::{account_home, open_login_registration_lock};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use crate::about::temporary_folders;

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

pub const AGENT_PLIST: &str = "org.broscribe.broscribed.plist";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
}

pub struct MacAutostart {
    executable: PathBuf,
}
impl MacAutostart {
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
    /// The calling executable with symlinks resolved, so a CLI reached
    /// through `/usr/local/bin` still finds its app bundle.
    fn canonical_executable(&self) -> Result<PathBuf> {
        std::fs::canonicalize(&self.executable).map_err(|e| Error::io_at(&self.executable, e))
    }
    /// Refuses to register from a translocated, temporary or read-only
    /// location (a quarantined download, a folder the system may empty or
    /// a mounted disk image): the registration would point at a path that
    /// disappears. Temporary folders are About's rule. The installation is
    /// never moved.
    fn check_location(path: &Path) -> Result<()> {
        match location_problem(path, read_only_volume(path), &temporary_folders()) {
            Some(problem) => Err(Error::Unsupported(problem.into())),
            None => Ok(()),
        }
    }
    fn service(&self) -> Retained<AnyObject> {
        // SAFETY: macOS 14.2 minimum; SMAppService's factory returns a retained
        // service object with this documented NSString argument.
        unsafe {
            msg_send![class!(SMAppService), agentServiceWithPlistName: &*NSString::from_str(AGENT_PLIST)]
        }
    }
    pub fn status(&self) -> AgentStatus {
        // SAFETY: status is the NSInteger SMAppServiceStatus getter.
        match unsafe { msg_send![&*self.service(), status] } {
            1isize => AgentStatus::Enabled,
            2isize => AgentStatus::RequiresApproval,
            3isize => AgentStatus::NotFound,
            _ => AgentStatus::NotRegistered,
        }
    }
    fn dev_plist(&self) -> Result<PathBuf> {
        let home =
            std::env::var_os("HOME").ok_or_else(|| Error::InvalidInput("HOME is unset".into()))?;
        Ok(PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(AGENT_PLIST))
    }
    pub fn installed(&self) -> Result<bool> {
        Ok(matches!(
            self.status(),
            AgentStatus::Enabled | AgentStatus::RequiresApproval
        ) || self.dev_plist()?.is_file())
    }
    fn dev_profile(&self) -> Result<Option<PathBuf>> {
        Ok(self.dev_job_paths()?.map(|job| job.data))
    }

    fn dev_job_paths(&self) -> Result<Option<JobPaths>> {
        let path = self.dev_plist()?;
        if !path.try_exists()? {
            return Ok(None);
        }
        let profile = path.to_str().and_then(|path| {
            // The plist is data. Never execute its program to identify it.
            #[allow(deprecated)]
            let plist = unsafe {
                NSDictionary::<NSString, AnyObject>::dictionaryWithContentsOfFile(
                    &NSString::from_str(path),
                )?
            };
            let arguments = plist
                .objectForKey(&NSString::from_str("ProgramArguments"))?
                .downcast::<NSArray>()
                .ok()?;
            let arguments = arguments
                .iter()
                .map(|arg| arg.downcast::<NSString>().ok().map(|arg| arg.to_string()))
                .collect::<Option<Vec<_>>>()?;
            let program = match plist.objectForKey(&NSString::from_str("Program")) {
                Some(program) => Some(program.downcast::<NSString>().ok()?.to_string()),
                None => None,
            };
            let env = match plist.objectForKey(&NSString::from_str("EnvironmentVariables")) {
                Some(env) => Some(env.downcast::<NSDictionary>().ok()?),
                None => None,
            };
            let mut job = paths_from_job(
                program.as_deref(),
                arguments.iter().map(String::as_str),
                |name| {
                    env.as_ref()
                        .and_then(|env| env.objectForKey(&NSString::from_str(name)))
                        .and_then(|value| value.downcast::<NSString>().ok())
                        .map(|value| value.to_string())
                },
                account_home,
            )?;
            job.working_dir = plist
                .objectForKey(&NSString::from_str("WorkingDirectory"))
                .and_then(|value| value.downcast::<NSString>().ok())
                .map_or_else(
                    || PathBuf::from("/"),
                    |value| PathBuf::from(value.to_string()),
                );
            Some(job)
        });
        profile.map(Some).ok_or_else(|| {
            Error::InvalidInput(
                "the installed BroScribe LaunchAgent's profile could not be verified".into(),
            )
        })
    }

    fn bundled_profile(&self) -> Result<Option<PathBuf>> {
        if !matches!(
            self.status(),
            AgentStatus::Enabled | AgentStatus::RequiresApproval
        ) {
            return Ok(None);
        }
        account_home()
            .map(|home| Some(crate::MacPaths { home }.data_dir()))
            .ok_or_else(|| {
                Error::InvalidInput("the bundled agent's default profile is unknown".into())
            })
    }

    fn bundled(&self) -> bool {
        self.canonical_executable()
            .is_ok_and(|executable| bundled_at(&executable))
    }
    fn change_service(&self, enabled: bool) -> Result<()> {
        let service = self.service();
        let mut error: *mut NSError = std::ptr::null_mut();
        // SAFETY: documented BOOL registerAndReturnError:/unregisterAndReturnError:
        // selectors with a valid autoreleasing NSError out-pointer.
        let ok: bool = unsafe {
            if enabled {
                msg_send![&*service, registerAndReturnError: &mut error]
            } else {
                msg_send![&*service, unregisterAndReturnError: &mut error]
            }
        };
        if ok {
            return Ok(());
        }
        let detail = if error.is_null() {
            "service registration failed".into()
        } else {
            unsafe { &*error }.localizedDescription().to_string()
        };
        Err(Error::Unsupported(detail))
    }
    fn dev_install(&self, enabled: bool, registration: &LoginRegistrationGuard) -> Result<()> {
        let plist = self.dev_plist()?;
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        if enabled {
            let caller = self.canonical_executable()?;
            let executable = helper_executable(&caller).ok_or_else(|| Error::Unsupported("Build BroScribe.app first; daemon installs require BroScribe Agent.app for microphone identity".into()))?;
            Self::check_location(&executable)?;
            let parent = plist.parent().expect("plist parent");
            std::fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
            let executable = xml(&executable.to_string_lossy());
            let text = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>org.broscribe.broscribed</string><key>ProgramArguments</key><array><string>{executable}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>"
            );
            std::fs::write(&plist, text).map_err(|e| Error::io_at(&plist, e))?;
            launchctl_bootstrap(&domain, &plist)?;
        } else if plist.exists() {
            self.stop_registered_job(registration)?;
            std::fs::remove_file(&plist).map_err(|e| Error::io_at(&plist, e))?;
        }
        Ok(())
    }
    /// Unregisters and registers the bundled agent; its plist's `RunAtLoad`
    /// starts it.
    fn reregister(&self) -> Result<()> {
        self.change_service(false)?;
        self.change_service(true)
    }
    fn stop_registered_job(&self, registration: &LoginRegistrationGuard) -> Result<()> {
        let label = format!("gui/{}/org.broscribe.broscribed", unsafe { libc::getuid() });
        if launchctl_print(&label)?.is_none() {
            return Ok(());
        }
        self.stop_login_job(registration)
    }
}
impl Autostart for MacAutostart {
    fn agent_install_location_advisory(&self) -> bool {
        self.canonical_executable().is_ok_and(|executable| {
            Self::check_location(&executable).is_err()
                || (bundled_at(&executable) && !executable.starts_with("/Applications"))
        })
    }

    fn agent_status(&self) -> Result<broscribe_platform::AgentServiceStatus> {
        use broscribe_platform::AgentServiceStatus::*;
        if self.bundled() && self.status() == AgentStatus::RequiresApproval {
            return Ok(NeedsApproval);
        }
        let label = format!("gui/{}/org.broscribe.broscribed", unsafe { libc::getuid() });
        let job = launchctl_print(&label)?;
        if start_action(job.as_deref(), self.bundled()) == StartAction::AlreadyRunning {
            Ok(Running)
        } else if self.installed()? {
            Ok(Stopped)
        } else {
            Ok(NotInstalled)
        }
    }
    fn is_enabled(&self) -> Result<bool> {
        self.installed()
    }
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        let registration = self.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.set_enabled_with_registration(enabled, &registration)
    }
    fn set_enabled_with_registration(
        &self,
        enabled: bool,
        registration: &LoginRegistrationGuard,
    ) -> Result<()> {
        if self.bundled() {
            if enabled {
                Self::check_location(&self.canonical_executable()?)?;
            }
            self.change_service(enabled)
        } else {
            self.dev_install(enabled, registration)
        }
    }
    fn start(&self) -> Result<()> {
        let registration = self.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.start_with_registration(&registration)
    }
    fn check_update_restart(&self) -> Result<()> {
        let can_start = if self.bundled() {
            self.status() == AgentStatus::Enabled
        } else {
            self.dev_plist()?.is_file()
        };
        if can_start {
            Ok(())
        } else {
            Err(Error::Service {
                code: "update_agent_unproven",
                message: "The macOS login job must be enabled in System Settings before updating BroScribe.".into(),
            })
        }
    }
    fn update_restart_library(&self, configured: Option<&Path>, default: &Path) -> Result<PathBuf> {
        let mut job = if self.bundled() {
            let label = format!("gui/{}/org.broscribe.broscribed", unsafe { libc::getuid() });
            launchctl_print(&label)?
                .as_deref()
                .and_then(|job| login_job_paths(job, account_home))
        } else {
            self.dev_job_paths()?
        }
        .ok_or_else(|| Error::Service {
            code: "update_agent_unproven",
            message: "Could not verify the library used by the launchd agent restart.".into(),
        })?;
        if configured.is_none()
            && job.library.is_none()
            && job.library_from_env.is_none()
            && job.home.is_none()
        {
            job.home = account_home();
        }
        job.restart_library(configured, default)
    }
    fn start_with_registration(&self, _: &LoginRegistrationGuard) -> Result<()> {
        // Register even for a stopped service; an already-enabled registration
        // is harmless, but approval-required must remain an explicit error.
        if self.bundled() {
            Self::check_location(&self.canonical_executable()?)?;
            if let Err(error) = self.change_service(true)
                && self.status() != AgentStatus::Enabled
            {
                return Err(error);
            }
            if self.status() == AgentStatus::RequiresApproval {
                return Err(Error::Unsupported("Approve BroScribe Agent in System Settings › General › Login Items before starting it".into()));
            }
        } else if !self.dev_plist()?.is_file() {
            return Err(Error::Unsupported(
                "Install BroScribe Agent before starting it".into(),
            ));
        }
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let label = format!("{domain}/org.broscribe.broscribed");
        let bundled = self.bundled();
        // Never `kickstart -k`: it kills a running helper and its recording.
        match start_action(launchctl_print(&label)?.as_deref(), bundled) {
            StartAction::AlreadyRunning => Ok(()),
            StartAction::Kickstart => match launchctl(&["kickstart", &label]) {
                Err(Error::Unsupported(_)) if bundled => self.reregister(),
                result => result.map(drop),
            },
            StartAction::Reregister => self.reregister(),
            StartAction::Bootstrap => {
                let plist = self.dev_plist()?;
                launchctl_bootstrap(&domain, &plist)?;
                launchctl(&["kickstart", &label]).map(drop)
            }
        }
    }
    fn stop(&self) -> Result<()> {
        let registration = self.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.stop_registered_job(&registration)
    }
    fn lock_login_registration(&self, wait: Duration) -> Result<LoginRegistrationGuard> {
        let (path, file) = open_login_registration_lock()
            .map_err(|e| Error::io("opening the login registration lock", e))?;
        LoginRegistrationGuard::acquire_open_file(&path, file, wait)
    }
    fn registered_profiles(&self) -> Result<Vec<RegisteredProfile>> {
        let mut profiles: Vec<_> = self
            .bundled_profile()?
            .into_iter()
            .map(Into::into)
            .collect();
        if let Some(job) = self.dev_job_paths()? {
            profiles.extend(job.registered_profiles()?);
        }
        let label = format!("gui/{}/org.broscribe.broscribed", unsafe { libc::getuid() });
        if let Some(job) = launchctl_print(&label)? {
            let paths = login_job_paths(&job, account_home).ok_or_else(|| Error::Service {
                code: "login_job_unverified",
                message: "The loaded BroScribe LaunchAgent's profile could not be verified".into(),
            })?;
            profiles.extend(paths.registered_profiles()?);
        }
        Ok(profiles)
    }

    fn unregister_profile(&self, data_dir: &Path, guard: &LoginRegistrationGuard) -> Result<()> {
        let selected = stable_path(data_dir);
        let bundled = self.bundled_profile()?;
        let dev = self.dev_profile()?;
        let owns = |path: &Path| stable_path(path) == selected;
        if bundled.as_deref().is_some_and(owns) {
            self.change_service(false)?;
        }
        if dev.as_deref().is_some_and(owns) {
            self.dev_install(false, guard)?;
        }
        // A job bootstrapped only for this session can have no registration.
        if bundled.is_none() && dev.is_none() {
            self.stop_registered_job(guard)?;
        }
        Ok(())
    }

    fn login_job(&self, data_dir: &Path) -> LoginJob {
        let label = format!("gui/{}/org.broscribe.broscribed", unsafe { libc::getuid() });
        let job =
            login_job_from_output(launchctl_output(&["print", &label]), data_dir, account_home);
        unloaded_job(
            job,
            || {
                Ok(self
                    .bundled_profile()?
                    .into_iter()
                    .chain(self.dev_profile()?)
                    .collect())
            },
            data_dir,
        )
    }
    fn login_job_matches(&self, data_dir: &Path) -> Option<bool> {
        let label = format!("gui/{}/org.broscribe.broscribed", unsafe { libc::getuid() });
        let output = launchctl_output(&["print", &label]).ok()?;
        if !output.status.success() {
            return None;
        }
        login_profile_matches(&output.stdout, data_dir, account_home)
    }
    fn stop_login_job(&self, _: &LoginRegistrationGuard) -> Result<()> {
        let label = format!("gui/{}/org.broscribe.broscribed", unsafe { libc::getuid() });
        // bootout stops a service without deleting its installed plist.
        launchctl(&["bootout", &label]).map(drop)
    }
}

fn unloaded_job(
    job: LoginJob,
    profiles: impl FnOnce() -> Result<Vec<PathBuf>>,
    data_dir: &Path,
) -> LoginJob {
    if job != LoginJob::Absent {
        return job;
    }
    match profiles() {
        Ok(paths) if paths.is_empty() => LoginJob::Absent,
        Ok(paths) => LoginJob::Registered {
            serves_profile: Some(
                paths
                    .iter()
                    .any(|path| stable_path(path) == stable_path(data_dir)),
            ),
        },
        Err(_) => LoginJob::Registered {
            serves_profile: None,
        },
    }
}

fn login_job_from_output(
    output: Result<Output>,
    data_dir: &Path,
    account_home: impl FnOnce() -> Option<PathBuf>,
) -> LoginJob {
    match output {
        Ok(output) if output.status.success() => {
            login_job_from_print(&output.stdout, data_dir, account_home)
        }
        Ok(output) if launchctl_job_absent(&output) => LoginJob::Absent,
        _ => LoginJob::Unknown,
    }
}

fn login_job_from_print(
    output: &[u8],
    data_dir: &Path,
    account_home: impl FnOnce() -> Option<PathBuf>,
) -> LoginJob {
    let job = String::from_utf8_lossy(output);
    let property = |name| {
        job.lines()
            .filter_map(|line| line.strip_prefix('\t'))
            .find_map(|line| line.strip_prefix(name))
    };
    if let Some(pid) = property("pid = ") {
        return pid.parse().map_or(LoginJob::Unknown, LoginJob::Running);
    }
    match property("state = ") {
        Some("not running" | "spawn scheduled") => LoginJob::Idle {
            serves_profile: login_profile_matches(output, data_dir, account_home),
        },
        _ => LoginJob::Unknown,
    }
}

fn login_profile_matches(
    output: &[u8],
    data_dir: &Path,
    account_home: impl FnOnce() -> Option<PathBuf>,
) -> Option<bool> {
    let registered = login_data_dir(std::str::from_utf8(output).ok()?, account_home)?;
    Some(stable_path(&registered) == stable_path(data_dir))
}

/// Read the loaded job, which can come from another app bundle or a development
/// LaunchAgent. Its arguments take precedence over launchd's environment.
fn login_data_dir(job: &str, account_home: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    Some(login_job_paths(job, account_home)?.data)
}

fn login_job_paths(job: &str, account_home: impl FnOnce() -> Option<PathBuf>) -> Option<JobPaths> {
    fn block<'a>(job: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> {
        job.lines()
            .map(str::trim_start)
            .skip_while(move |line| line.strip_suffix(" = {") != Some(key))
            .skip(1)
            .take_while(|line| *line != "}")
    }

    let env = |variable| {
        [
            "environment",
            "inherited environment",
            "default environment",
        ]
        .into_iter()
        .find_map(|key| {
            block(job, key).find_map(|line| {
                let (name, value) = line.split_once(" => ")?;
                (name == variable).then_some(value)
            })
        })
    };
    let program = job
        .lines()
        .find_map(|line| line.strip_prefix("\tprogram = "));
    let bundle_program = job.lines().find_map(|line| {
        line.strip_prefix("\tprogram identifier = ").map(|path| {
            path.strip_suffix(')')
                .and_then(|path| path.rsplit_once(" (mode: "))
                .filter(|(_, mode)| mode.parse::<u32>().is_ok())
                .map_or(path, |(program, _)| program)
        })
    });
    let mut executables = [program, bundle_program].into_iter().flatten();
    let program = executables.next();
    if executables.any(|path| !is_daemon(path)) {
        return None;
    }
    let mut paths = paths_from_job(
        program,
        block(job, "arguments"),
        |key| env(key).map(str::to_owned),
        account_home,
    )?;
    if let Some(cwd) = job
        .lines()
        .find_map(|line| line.strip_prefix("\tworking directory = "))
    {
        paths.working_dir = PathBuf::from(cwd);
    }
    Some(paths)
}

fn is_daemon(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .is_some_and(|name| name == "broscribed")
}

struct JobPaths {
    data: PathBuf,
    library: Option<PathBuf>,
    library_from_env: Option<PathBuf>,
    home: Option<PathBuf>,
    working_dir: PathBuf,
}

impl JobPaths {
    fn registered_profiles(self) -> Result<Vec<RegisteredProfile>> {
        if !self.working_dir.is_absolute() {
            return Err(Error::InvalidInput(
                "The launchd working directory is unknown.".into(),
            ));
        }
        let mut profiles: Vec<_> = [self.library, self.library_from_env]
            .into_iter()
            .flatten()
            .map(|path| RegisteredProfile {
                data_dir: self.data.clone(),
                library: Some(self.working_dir.join(path)),
            })
            .collect();
        if profiles.is_empty() {
            profiles.push(self.data.into());
        }
        Ok(profiles)
    }

    fn restart_library(&self, configured: Option<&Path>, default: &Path) -> Result<PathBuf> {
        if !self.working_dir.is_absolute() {
            return Err(Error::InvalidInput(
                "The launchd working directory is unknown.".into(),
            ));
        }
        let default = self.home.as_ref().map_or_else(
            || default.to_path_buf(),
            |home| crate::MacPaths { home: home.clone() }.default_library_dir(),
        );
        Ok(self.working_dir.join(
            self.library
                .as_deref()
                .or(self.library_from_env.as_deref())
                .or(configured)
                .unwrap_or(&default),
        ))
    }
}

fn paths_from_job<'a>(
    program: Option<&str>,
    mut arguments: impl Iterator<Item = &'a str>,
    env: impl Fn(&'static str) -> Option<String>,
    account_home: impl FnOnce() -> Option<PathBuf>,
) -> Option<JobPaths> {
    let argv0 = arguments.next();
    let mut executables = [program, argv0].into_iter().flatten();
    if !executables.next().is_some_and(is_daemon) || !executables.all(is_daemon) {
        return None;
    }
    let mut data = None;
    let mut library = None;
    while let Some(arg) = arguments.next() {
        if arg == "--data-dir" {
            data = Some(arguments.next()?);
        } else if let Some(path) = arg.strip_prefix("--data-dir=") {
            data = Some(path);
        } else if arg == "--library" {
            library = Some(arguments.next()?);
        } else if let Some(path) = arg.strip_prefix("--library=") {
            library = Some(path);
        }
    }
    let mut home = env("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute());
    let data = match data
        .map(str::to_owned)
        .or_else(|| env(broscribe_platform::paths::ENV_DATA_DIR))
        .filter(|path| !path.is_empty())
    {
        Some(path) => PathBuf::from(path),
        None => {
            home = home.or_else(account_home).filter(|home| home.is_absolute());
            crate::MacPaths {
                home: home.clone()?,
            }
            .data_dir()
        }
    };
    let library_from_env = env(broscribe_platform::paths::ENV_LIBRARY)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    data.is_absolute().then(|| JobPaths {
        data,
        library: library.map(PathBuf::from),
        library_from_env,
        home,
        working_dir: PathBuf::from("/"),
    })
}

/// What `start` does, given `launchctl print` of the service (`None`: not
/// loaded) and whether the agent is the app's bundled one.
#[derive(Debug, PartialEq, Eq)]
enum StartAction {
    AlreadyRunning,
    /// Loaded but stopped: start it.
    Kickstart,
    /// Development agent not loaded: load its plist, then start it.
    Bootstrap,
    /// Bundled agent not loaded (after a `stop`): its `BundleProgram` plist
    /// loads only through SMAppService, so unregister and register it again.
    Reregister,
}

fn start_action(print: Option<&str>, bundled: bool) -> StartAction {
    match print {
        None if bundled => StartAction::Reregister,
        None => StartAction::Bootstrap,
        Some(print) if print.lines().any(|line| line.trim() == "state = running") => {
            StartAction::AlreadyRunning
        }
        Some(_) => StartAction::Kickstart,
    }
}

fn run_launchctl(command: &mut Command, timeout: Duration) -> Result<Output> {
    login_manager_output(command, timeout).map_err(|error| match error {
        Error::Io { source, .. } => Error::io("controlling capture agent with launchctl", source),
        error => error,
    })
}

fn launchctl_output(args: &[&str]) -> Result<Output> {
    run_launchctl(
        Command::new("/bin/launchctl").args(args),
        LOGIN_MANAGER_TIMEOUT,
    )
}

fn launchctl_bootstrap(domain: &str, plist: &Path) -> Result<()> {
    let output = run_launchctl(
        Command::new("/bin/launchctl")
            .args(["bootstrap", domain])
            .arg(plist),
        LOGIN_MANAGER_TIMEOUT,
    )?;
    launchctl_success("bootstrap", output).map(drop)
}

fn launchctl_job_absent(output: &Output) -> bool {
    // 112 is a missing GUI domain and 113 a missing service.
    matches!(output.status.code(), Some(112 | 113))
}

fn launchctl_print(label: &str) -> Result<Option<String>> {
    let output = launchctl_output(&["print", label])?;
    if launchctl_job_absent(&output) {
        return Ok(None);
    }
    launchctl_success("print", output).map(Some)
}

/// Runs `launchctl`; its standard output on success.
fn launchctl(args: &[&str]) -> Result<String> {
    launchctl_success(args[0], launchctl_output(args)?)
}

fn launchctl_success(action: &str, output: Output) -> Result<String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::Unsupported(format!(
            "launchctl {action} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn helper_executable(executable: &Path) -> Option<PathBuf> {
    for ancestor in executable.ancestors() {
        let helper = ancestor
            .join("Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed");
        if helper.is_file() {
            return Some(helper);
        }
        let development = ancestor.join(
            "BroScribe.app/Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed",
        );
        if development.is_file() {
            return Some(development);
        }
        if ancestor
            .file_name()
            .is_some_and(|name| name == "BroScribe Agent.app")
            && executable.is_file()
        {
            return Some(executable.to_owned());
        }
    }
    let helper = executable.parent()?.join(
        "BroScribe.app/Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed",
    );
    helper.is_file().then_some(helper)
}

/// Whether `executable` lies inside an app bundle carrying the agent plist.
fn bundled_at(executable: &Path) -> bool {
    executable.ancestors().any(|path| {
        path.join("Contents/Library/LaunchAgents")
            .join(AGENT_PLIST)
            .is_file()
    })
}

/// Why a (canonical) path must not be registered as a login agent, given
/// whether its volume is read-only and the canonical `temporary` folders.
fn location_problem(path: &Path, read_only: bool, temporary: &[PathBuf]) -> Option<&'static str> {
    if path
        .components()
        .any(|component| component.as_os_str() == "AppTranslocation")
    {
        return Some(
            "macOS is running BroScribe from a temporary translocated copy; move BroScribe.app to Applications and open it from there before installing BroScribe Agent",
        );
    }
    if is_temporary(path, temporary) {
        return Some(
            "BroScribe is running from a temporary folder the system may empty; move BroScribe.app to Applications and open it from there before installing BroScribe Agent",
        );
    }
    if read_only {
        return Some(
            "BroScribe is running from a read-only volume (a disk image?); copy BroScribe.app to Applications before installing BroScribe Agent",
        );
    }
    None
}

/// Whether the volume holding `path` is mounted read-only. An unreadable
/// mount state is not treated as read-only.
fn read_only_volume(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: zeroed statfs is a valid out-parameter; `path` is NUL-terminated.
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stats) } != 0 {
        return false;
    }
    stats.f_flags & libc::MNT_RDONLY as u32 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unloaded_registered_job_keeps_its_profile() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("profile-a");
        let b = root.path().join("profile-b");
        assert_eq!(
            unloaded_job(LoginJob::Absent, || Ok(vec![a.clone()]), &a),
            LoginJob::Registered {
                serves_profile: Some(true)
            }
        );
        assert_eq!(
            unloaded_job(LoginJob::Absent, || Ok(vec![a]), &b),
            LoginJob::Registered {
                serves_profile: Some(false)
            }
        );
        assert_eq!(
            unloaded_job(LoginJob::Absent, || Ok(Vec::new()), &b),
            LoginJob::Absent
        );
    }

    #[test]
    fn dev_plist_profile_uses_direct_arguments_before_environment() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("Profile A");
        let args = [
            "/Example/BroScribe.app/Contents/MacOS/broscribed",
            "--data-dir",
            a.to_str().unwrap(),
        ];
        assert_eq!(
            paths_from_job(None, args.into_iter(), |_| Some("/other".into()), || None)
                .map(|job| job.data),
            Some(a)
        );
        assert!(
            paths_from_job(
                None,
                ["/example/wrapper", "broscribed"].into_iter(),
                |_| None,
                || None
            )
            .is_none()
        );
    }

    #[test]
    fn launchd_restart_library_respects_job_arguments_environment_and_working_directory() {
        let job = concat!(
            "\tprogram = /Synthetic/broscribed\n",
            "\targuments = {\n\t\t/Synthetic/broscribed\n\t\t--library\n\t\tflag library\n\t}\n",
            "\tworking directory = /Synthetic/Service\n",
            "\tenvironment = {\n\t\tBROSCRIBE_LIBRARY => /Synthetic/Environment\n\t}\n",
        );
        let default = Path::new("/Synthetic/Default");
        let config = Path::new("configured");
        let parse =
            |text: &str| login_job_paths(text, || Some(PathBuf::from("/Synthetic/Home"))).unwrap();
        assert_eq!(
            parse(job).restart_library(Some(config), default).unwrap(),
            Path::new("/Synthetic/Service/flag library")
        );
        let libraries: Vec<_> = parse(job)
            .registered_profiles()
            .unwrap()
            .into_iter()
            .filter_map(|profile| profile.library)
            .collect();
        assert_eq!(
            libraries,
            [
                PathBuf::from("/Synthetic/Service/flag library"),
                PathBuf::from("/Synthetic/Environment"),
            ]
        );
        let job = job.replace("\t\t--library\n\t\tflag library\n", "");
        assert_eq!(
            parse(&job).restart_library(Some(config), default).unwrap(),
            Path::new("/Synthetic/Environment")
        );
        let job = job.replace("\t\tBROSCRIBE_LIBRARY => /Synthetic/Environment\n", "");
        assert_eq!(
            parse(&job).restart_library(Some(config), default).unwrap(),
            Path::new("/Synthetic/Service/configured")
        );
    }

    const BUNDLED_JOB: &str = concat!(
        "gui/501/org.broscribe.broscribed = {\n",
        "\tactive count = 1\n",
        "\tpath = (submitted by smd.101)\n",
        "\ttype = LaunchAgent\n",
        "\tmanaged_by = com.apple.xpc.ServiceManagement\n",
        "\tstate = running\n\n",
        "\tprogram identifier = Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed (mode: 2)\n",
        "\tparent bundle identifier = org.broscribe.BroScribe\n",
        "\tparent bundle version = 1\n",
        "\tBTM uuid = 00000000-0000-0000-0000-000000000000\n",
        "\tinherited environment = {\n\t\tSSH_AUTH_SOCK => /private/tmp/com.apple.launchd.synthetic/Listeners\n\t}\n\n",
        "\tdefault environment = {\n\t\tPATH => /usr/bin:/bin:/usr/sbin:/sbin\n\t}\n\n",
        "\tenvironment = {\n\t\tOSLogRateLimit => 64\n\t\tXPC_SERVICE_NAME => org.broscribe.broscribed\n\t}\n",
        "\tpid = 4242\n}\n",
    );

    #[test]
    fn launchctl_bundled_job_proves_the_default_profile_for_running_and_idle_jobs() {
        let home = PathBuf::from("/synthetic/account");
        let default = crate::MacPaths { home: home.clone() }.data_dir();
        let account = || Some(home.clone());
        let other = Path::new("/synthetic/other-profile");
        let running = BUNDLED_JOB.as_bytes();
        assert_eq!(
            login_job_from_print(running, &default, account),
            LoginJob::Running(4242.try_into().unwrap())
        );
        assert_eq!(
            login_profile_matches(running, &default, account),
            Some(true)
        );
        assert_eq!(login_profile_matches(running, other, account), Some(false));

        let idle = BUNDLED_JOB
            .replace("active count = 1", "active count = 0")
            .replace("state = running", "state = spawn scheduled")
            .replace("\tpid = 4242\n", "");
        for (data_dir, matches) in [(&*default, true), (other, false)] {
            assert_eq!(
                login_job_from_print(idle.as_bytes(), data_dir, account),
                LoginJob::Idle {
                    serves_profile: Some(matches),
                }
            );
        }
        for key in [
            "environment",
            "inherited environment",
            "default environment",
        ] {
            let with_override = idle.replace(
                &format!("\t{key} = {{\n"),
                &format!("\t{key} = {{\n\t\tBROSCRIBE_DATA_DIR => /synthetic/other-profile\n"),
            );
            assert_eq!(
                login_job_from_print(with_override.as_bytes(), other, || unreachable!()),
                LoginJob::Idle {
                    serves_profile: Some(true),
                }
            );
        }
        let unknown = idle.replace("/MacOS/broscribed (mode:", "/MacOS/other-helper (mode:");
        assert_eq!(
            login_job_from_print(unknown.as_bytes(), &default, account),
            LoginJob::Idle {
                serves_profile: None
            }
        );
        let wrapper = idle.replace(
            "\tparent bundle identifier",
            "\targuments = {\n\t\t/bin/sh\n\t}\n\tparent bundle identifier",
        );
        assert_eq!(
            login_profile_matches(wrapper.as_bytes(), &default, account),
            None
        );
    }

    #[test]
    fn hung_launchctl_reads_are_unknown_and_mutations_error() {
        let hung = || {
            run_launchctl(
                Command::new("/bin/sleep").arg("30"),
                Duration::from_millis(100),
            )
        };
        assert_eq!(
            login_job_from_output(hung(), Path::new("/synthetic"), || unreachable!()),
            LoginJob::Unknown
        );
        let mutation = hung().and_then(|output| launchctl_success("bootout", output));
        assert!(matches!(
            mutation,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn launchctl_captures_stdout_and_error_diagnostics_separately() {
        let output = run_launchctl(
            Command::new("/bin/sh").args(["-c", "printf stdout; printf stderr >&2; exit 1"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");
        assert!(
            launchctl_success("bootout", output)
                .unwrap_err()
                .to_string()
                .contains("stderr")
        );
    }

    #[test]
    fn launchctl_print_uses_the_top_level_live_pid() {
        use std::num::NonZeroU32;

        let print = b"gui/501/org.broscribe.broscribed = {\n\tstate = running\n\tpid = 4242\n\tproperties = {\n\t\tpid = 123\n\t}\n}";
        assert_eq!(
            login_job_from_print(print, Path::new("/synthetic"), || unreachable!()),
            LoginJob::Running(NonZeroU32::new(4242).unwrap())
        );
        let stopped = b"gui/501/org.broscribe.broscribed = {\n\tstate = not running\n\tproperties = {\n\t\tpid = 123\n\t}\n}";
        assert_eq!(
            login_job_from_print(stopped, Path::new("/synthetic"), || None),
            LoginJob::Idle {
                serves_profile: None
            }
        );
        for output in [
            b"\tstate = running".as_slice(),
            b"\tpid = malformed",
            b"\tpid = 0",
            b"unexpected output",
        ] {
            assert_eq!(
                login_job_from_print(output, Path::new("/synthetic"), || unreachable!()),
                LoginJob::Unknown
            );
        }
    }

    #[test]
    fn launchctl_missing_domain_or_service_is_absent() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{ExitStatus, Output};

        for (code, job) in [
            (112, LoginJob::Absent),
            (113, LoginJob::Absent),
            (1, LoginJob::Unknown),
        ] {
            let output = Output {
                status: ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
            assert_eq!(
                login_job_from_output(Ok(output), Path::new("/synthetic"), || unreachable!()),
                job
            );
        }
    }

    #[test]
    fn launchctl_idle_job_uses_its_arguments_and_environment() {
        let data = Path::new("/synthetic/profile a");
        let environment = "\tinherited environment = {\n\t\tBROSCRIBE_DATA_DIR => /synthetic/inherited\n\t}\n\tenvironment = {\n\t\tBROSCRIBE_DATA_DIR => /synthetic/profile a\n\t}\n";
        let idle =
            format!("\tstate = spawn scheduled\n\tprogram = /synthetic/broscribed\n{environment}");
        let parse = |job: &str| login_job_from_print(job.as_bytes(), data, || unreachable!());
        assert_eq!(
            parse(&idle),
            LoginJob::Idle {
                serves_profile: Some(true)
            }
        );
        let other = format!(
            "{idle}\targuments = {{\n\t\tbroscribed\n\t\t--data-dir\n\t\t/synthetic/profile b\n\t}}\n"
        );
        assert_eq!(
            parse(&other),
            LoginJob::Idle {
                serves_profile: Some(false)
            }
        );
        let equals = other.replace(
            "--data-dir\n\t\t/synthetic/profile b",
            "--data-dir=/synthetic/profile a",
        );
        assert_eq!(
            parse(&equals),
            LoginJob::Idle {
                serves_profile: Some(true)
            }
        );
        let unknown = other.replace(
            "--data-dir\n\t\t/synthetic/profile b",
            "--data-dir=relative",
        );
        assert_eq!(
            parse(&unknown),
            LoginJob::Idle {
                serves_profile: None
            }
        );

        let home = PathBuf::from("/synthetic/account");
        let default = crate::MacPaths { home: home.clone() }.data_dir();
        assert_eq!(
            login_job_from_print(
                b"\tstate = not running\n\targuments = {\n\t\t/synthetic/broscribed\n\t}",
                &default,
                || Some(home)
            ),
            LoginJob::Idle {
                serves_profile: Some(true)
            }
        );
    }

    #[test]
    fn launchctl_idle_job_never_infers_a_profile_for_wrappers_or_an_unknown_program() {
        let home = PathBuf::from("/synthetic/account");
        let default = crate::MacPaths { home: home.clone() }.data_dir();
        for definition in [
            "",
            "\targuments = {\n\t\t/usr/bin/env\n\t\tBROSCRIBE_DATA_DIR=/synthetic/other\n\t\t/synthetic/broscribed\n\t}",
            "\tprogram = /bin/sh\n\targuments = {\n\t\t/bin/sh\n\t\t/synthetic/wrapper\n\t}",
            "\tprogram = /bin/sh\n\targuments = {\n\t\t/synthetic/broscribed\n\t}",
            "\tprogram = /usr/bin/env",
        ] {
            let job = format!(
                "\tstate = not running\n\tdefault environment = {{\n\t\tHOME => /synthetic/account\n\t}}\n{definition}"
            );
            assert_eq!(
                login_job_from_print(job.as_bytes(), &default, || Some(home.clone())),
                LoginJob::Idle {
                    serves_profile: None
                },
                "{definition}"
            );
        }
    }

    #[test]
    fn symlinked_callers_resolve_to_their_bundle_and_helper() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Applications/BroScribe.app");
        let binary = app.join("Contents/MacOS/broscribe");
        let plist = app.join("Contents/Library/LaunchAgents").join(AGENT_PLIST);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"synthetic").unwrap();
        std::fs::write(&plist, b"synthetic").unwrap();
        let link = dir.path().join("bin/broscribe");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&binary, &link).unwrap();
        // The link's own ancestors hold no bundle; its target's do.
        assert!(!bundled_at(&link));
        assert!(MacAutostart::new(link.clone()).bundled());
        assert_eq!(
            MacAutostart::new(link).canonical_executable().unwrap(),
            std::fs::canonicalize(&binary).unwrap()
        );

        // A development binary reached through a symlink finds the built helper.
        let debug = dir.path().join("target/debug/broscribe");
        let helper = dir.path().join("target/BroScribe.app/Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed");
        std::fs::create_dir_all(debug.parent().unwrap()).unwrap();
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&debug, b"synthetic").unwrap();
        std::fs::write(&helper, b"synthetic").unwrap();
        let debug_link = dir.path().join("bin/broscribe-dev");
        std::os::unix::fs::symlink(&debug, &debug_link).unwrap();
        let canonical = MacAutostart::new(debug_link)
            .canonical_executable()
            .unwrap();
        assert_eq!(
            helper_executable(&canonical),
            Some(std::fs::canonicalize(&helper).unwrap())
        );
    }
    #[test]
    fn translocated_temporary_and_read_only_locations_are_refused() {
        let temporary = temporary_folders();
        let translocated = Path::new(
            "/private/var/folders/xy/abc/T/AppTranslocation/1234-5678/d/BroScribe.app/Contents/MacOS/broscribe",
        );
        assert!(location_problem(translocated, false, &temporary).is_some());
        let installed = Path::new("/Applications/BroScribe.app/Contents/MacOS/broscribe");
        assert!(location_problem(installed, false, &temporary).is_none());
        assert!(location_problem(installed, true, &temporary).is_some());
        // A folder merely named like it elsewhere in a component is fine.
        assert!(
            location_problem(
                Path::new("/Users/x/AppTranslocationNotes/b"),
                false,
                &temporary
            )
            .is_none()
        );
        // `/tmp` and `/var/tmp` as the file system resolves them.
        for path in [
            "/private/tmp/x/BroScribe.app/Contents/MacOS/broscribe",
            "/private/var/tmp/x/BroScribe.app/Contents/MacOS/broscribe",
        ] {
            assert!(
                location_problem(Path::new(path), false, &temporary).is_some(),
                "{path}"
            );
        }
        let writable = tempfile::tempdir().unwrap();
        assert!(!read_only_volume(writable.path()));
        // The sealed system volume is mounted read-only.
        assert!(read_only_volume(Path::new("/System/Library")));
    }
    #[test]
    fn a_bundle_in_a_temporary_folder_is_refused_before_registration() {
        // The guard `set_enabled(true)`, `start` and the development install
        // pass before registering anything, for a bundled caller reached
        // through a symlink in the canonical temporary folder.
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Applications/BroScribe.app");
        let binary = app.join("Contents/MacOS/broscribe");
        let plist = app.join("Contents/Library/LaunchAgents").join(AGENT_PLIST);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"synthetic").unwrap();
        std::fs::write(&plist, b"synthetic").unwrap();
        let link = dir.path().join("bin/broscribe");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&binary, &link).unwrap();
        let autostart = MacAutostart::new(link);
        assert!(autostart.bundled());
        let canonical = autostart.canonical_executable().unwrap();
        assert!(matches!(
            MacAutostart::check_location(&canonical),
            Err(Error::Unsupported(_))
        ));
        // The same bundle is otherwise registrable: only its folder is refused.
        assert!(location_problem(&canonical, false, &[]).is_none());
    }
    #[test]
    fn never_uses_bare_development_binary_for_capture() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("broscribed");
        std::fs::write(&bare, b"synthetic").unwrap();
        assert!(helper_executable(&bare).is_none());
        let helper = dir.path().join("BroScribe.app/Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&helper, b"synthetic").unwrap();
        assert_eq!(helper_executable(&bare), Some(helper));
    }
    #[test]
    fn finds_target_bundle_from_debug_binary() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("target/debug/broscribe");
        let helper = dir.path().join("target/BroScribe.app/Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"synthetic").unwrap();
        std::fs::write(&helper, b"synthetic").unwrap();
        assert_eq!(helper_executable(&binary), Some(helper));
    }
    #[test]
    fn start_leaves_a_running_helper_alone() {
        let running = "gui/501/org.broscribe.broscribed = {\n\tactive count = 1\n\ttype = LaunchAgent\n\tstate = running\n\tpid = 4242\n}\n";
        let stopped = "gui/501/org.broscribe.broscribed = {\n\tactive count = 0\n\ttype = LaunchAgent\n\tstate = not running\n}\n";
        for bundled in [false, true] {
            assert_eq!(
                start_action(Some(running), bundled),
                StartAction::AlreadyRunning
            );
            assert_eq!(start_action(Some(stopped), bundled), StartAction::Kickstart);
        }
        assert_eq!(start_action(None, false), StartAction::Bootstrap);
    }
    #[test]
    fn bundled_start_after_stop_reregisters_instead_of_bootstrapping() {
        assert_eq!(start_action(None, true), StartAction::Reregister);
    }
}

use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use broscribe_platform::about::install_folder;
use broscribe_platform::uninstall::ReleaseRemovalCheck;
use broscribe_platform::uninstall::{
    DeferredProfile, DirectoryIdentity, Layout, owns_namespace, remove_path,
};
use broscribe_platform::update::UpdatePlatform as _;
use broscribe_platform::{AppPaths, Error, Result, plain_verbatim, stable_path};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, HANDLE, MAX_PATH};
use windows::Win32::Security::Credentials::{
    CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredEnumerateW, CredFree,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    GetFileInformationByHandle,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateGuid, CoCreateInstance, IPersistFile, STGM_READ,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
use windows::Win32::UI::Shell::{
    DestinationList, FOLDERID_Programs, ICustomDestinationList, IShellLinkW, ShellLink,
};
use windows::core::{HSTRING, Interface as _, PCWSTR};

pub const SECRET_BASE: &str = crate::shell::SECRET_SERVICE_BASE;

fn release_root(executable: &Path) -> Option<PathBuf> {
    let executable = plain_verbatim(&executable.canonicalize().ok()?);
    let folder = executable.parent()?;
    let root = install_folder(&executable)?;
    (matches!(
        executable.file_name()?.to_str()?,
        "broscribe.exe" | "broscribe-app.exe"
    ) && folder.join("broscribed.exe").is_file()
        && folder.join("broscribe-app.exe").is_file())
    .then_some(root)
}

pub fn layout(executable: &Path) -> Result<Layout> {
    Ok(Layout {
        installation: release_root(executable),
        ..Layout::default()
    })
}

pub fn registration_lock_paths() -> Result<Vec<PathBuf>> {
    let cache = crate::native::WinPaths::try_new()?.cache_dir();
    Ok(vec![
        cache
            .parent()
            .expect("cache parent")
            .join("login-registration.lock"),
    ])
}

fn shortcut_name(name: &str) -> Option<&str> {
    let suffix = name.strip_prefix("BroScribe")?.strip_suffix(".lnk")?;
    (suffix.is_empty() || owns_namespace("BroScribe", &format!("BroScribe{suffix}")))
        .then_some(suffix)
}

fn shortcut_profile(path: &Path, default: &Path) -> Result<Option<PathBuf>> {
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| crate::error("reading BroScribe shortcut", e))?;
    let persist: IPersistFile = link
        .cast()
        .map_err(|e| crate::error("reading BroScribe shortcut", e))?;
    let filename = crate::wide(path);
    unsafe { persist.Load(PCWSTR(filename.as_ptr()), STGM_READ) }
        .map_err(|e| crate::error("reading BroScribe shortcut", e))?;
    let mut target = [0; MAX_PATH as usize];
    unsafe { link.GetPath(&mut target, std::ptr::null_mut(), 0) }
        .map_err(|e| crate::error("reading BroScribe shortcut target", e))?;
    let target = String::from_utf16_lossy(target.split(|c| *c == 0).next().unwrap_or_default());
    if !Path::new(&target)
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("broscribe-app.exe"))
    {
        return Ok(None);
    }
    let mut arguments = vec![0; 32768];
    unsafe { link.GetArguments(&mut arguments) }
        .map_err(|e| crate::error("reading BroScribe shortcut arguments", e))?;
    let arguments = arguments.split(|c| *c == 0).next().unwrap_or_default();
    if arguments.is_empty() {
        return Ok(Some(default.to_path_buf()));
    }
    let command: Vec<_> = "app "
        .encode_utf16()
        .chain(arguments.iter().copied())
        .collect();
    let args = crate::shell::command_arguments(&command).unwrap_or_default();
    match args.as_slice() {
        [_, flag, value] if flag == "--dock-data-dir" && Path::new(value).is_absolute() => {
            Ok(Some(PathBuf::from(value)))
        }
        _ => Err(Error::InvalidInput(
            "a BroScribe Start menu shortcut has an unknown profile".into(),
        )),
    }
}

fn shortcuts() -> Result<Vec<(PathBuf, PathBuf)>> {
    let _com = crate::Com::new()?;
    let root = crate::native::known_folder(&FOLDERID_Programs)?;
    let default = crate::native::WinPaths::try_new()?.data_dir();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&root, e)),
    };
    let mut shortcuts = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(shortcut_name) else {
            continue;
        };
        if let Some(profile) = shortcut_profile(&entry.path(), &default)? {
            let id =
                crate::shell::profile_app_id((!suffix.is_empty()).then_some(profile.as_path()));
            if id.strip_prefix(SECRET_BASE) == Some(suffix) {
                shortcuts.push((entry.path(), profile));
            }
        }
    }
    Ok(shortcuts)
}

pub fn shortcut_profiles() -> Result<Vec<PathBuf>> {
    Ok(shortcuts()?
        .into_iter()
        .map(|(_, profile)| profile)
        .collect())
}

pub fn remove_shortcuts(profiles: &[PathBuf]) -> Result<usize> {
    let shortcuts = shortcuts()?;
    let _com = crate::Com::new()?;
    let default = crate::native::WinPaths::try_new()?.data_dir();
    let mut removed = 0;
    let mut failed = 0;
    for profile in profiles {
        let identity = crate::profile_identity(profile);
        let is_default = identity == crate::profile_identity(&default);
        // The default AUMID and a legacy explicit override of the default
        // folder may both have been created.
        let mut ids = vec![crate::shell::profile_app_id(Some(profile))];
        if is_default {
            ids.push(SECRET_BASE.to_owned());
        }
        for id in ids {
            let jump = unsafe {
                CoCreateInstance::<_, ICustomDestinationList>(
                    &DestinationList,
                    None,
                    CLSCTX_INPROC_SERVER,
                )
                .and_then(|list| list.DeleteList(&HSTRING::from(&id)))
            };
            if jump.is_err_and(|e| {
                e.code() != ERROR_FILE_NOT_FOUND.to_hresult()
                    && e.code() != ERROR_NOT_FOUND.to_hresult()
            }) {
                failed += 1;
            }
            let key = crate::wide(format!(r"Software\Classes\AppUserModelId\{id}"));
            let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr())) };
            if status.is_err() && status != ERROR_FILE_NOT_FOUND {
                failed += 1;
            }
        }
        for (path, recorded_profile) in &shortcuts {
            if crate::profile_identity(recorded_profile) == identity {
                match remove_path(path) {
                    Ok(done) => removed += usize::from(done),
                    Err(_) => failed += 1,
                }
            }
        }
    }
    if failed != 0 {
        return Err(Error::Internal(format!(
            "removed {removed} Start menu shortcuts; {failed} desktop cleanup steps failed"
        )));
    }
    Ok(removed)
}

pub fn remove_credentials(services: &[String], all: bool) -> Result<usize> {
    let mut count = 0;
    let mut failed = 0;
    for pattern in ["provider/*", "integration/*", "updates/release_key/*"] {
        let filter = HSTRING::from(pattern);
        let mut records: *mut *mut CREDENTIALW = std::ptr::null_mut();
        let mut len = 0;
        let result = unsafe { CredEnumerateW(&filter, None, &mut len, &mut records) };
        match result {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => continue,
            Err(e) => return Err(crate::error("listing BroScribe credentials", e)),
        }
        // SAFETY: CredEnumerate returned len credential pointers owned by a
        // single allocation. Only TargetName and Type are accessed.
        if records.is_null() {
            continue;
        }
        unsafe {
            for record in std::slice::from_raw_parts(records, len as usize) {
                let Some(record) = record.as_ref() else {
                    continue;
                };
                if record.Type != CRED_TYPE_GENERIC {
                    continue;
                }
                let Ok(target) = record.TargetName.to_string() else {
                    continue;
                };
                let in_scope = services.iter().any(|service| {
                    target
                        .strip_suffix(service)
                        .is_some_and(|prefix| prefix.ends_with('.'))
                }) || all
                    && target
                        .rsplit_once(&format!(".{SECRET_BASE}"))
                        .is_some_and(|(_, suffix)| {
                            owns_namespace(SECRET_BASE, &format!("{SECRET_BASE}{suffix}"))
                        });
                if in_scope {
                    match CredDeleteW(PCWSTR(record.TargetName.0), CRED_TYPE_GENERIC, None) {
                        Ok(()) => count += 1,
                        Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => {}
                        Err(_) => failed += 1,
                    }
                }
            }
            CredFree(records.cast());
        }
    }
    if failed != 0 {
        return Err(Error::Internal(format!(
            "removed {count} Credential Manager entries; {failed} failed"
        )));
    }
    Ok(count)
}

// Paths are passed through the child's environment. The fixed script never
// executes path text, follows no proposed user-provided commands, and records
// both success and failure outside the installation.
const AFTER_EXIT: &str = include_str!("uninstall-after-exit.ps1");

pub fn directory_identity(path: &Path) -> Result<Option<DirectoryIdentity>> {
    let file = match std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io_at(path, error)),
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|error| crate::error("reading the uninstall profile directory identity", error))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Ok(None);
    }
    let join = |high, low| (u64::from(high) << 32) | u64::from(low);
    Ok(Some(DirectoryIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: join(info.nFileIndexHigh, info.nFileIndexLow),
        generation: Some(join(
            info.ftCreationTime.dwHighDateTime,
            info.ftCreationTime.dwLowDateTime,
        )),
    }))
}

pub fn validate_installation(root: &Path, has_inventory: bool) -> Result<()> {
    if has_inventory {
        return Ok(());
    }
    broscribe_platform::release_layout::ReleaseLayout::Windows.validate(root, false)
}

pub fn remove_installation(
    executable: &Path,
    root: &Path,
    check: &ReleaseRemovalCheck<'_>,
) -> Result<Option<PathBuf>> {
    remove_after_exit(executable, Some(root), Some(check), None, &mut |receipt| {
        (check.windows_before_launch)(receipt)
    })
    .map(Some)
}

pub fn remove_after_exit(
    executable: &Path,
    root: Option<&Path>,
    check: Option<&ReleaseRemovalCheck<'_>>,
    profile: Option<&DeferredProfile>,
    before_launch: &mut dyn FnMut(&Path) -> Result<PathBuf>,
) -> Result<PathBuf> {
    let installation = release_root(executable);
    if root.is_some()
        && !installation
            .as_deref()
            .zip(root)
            .is_some_and(|(actual, expected)| broscribe_platform::same_path(actual, expected))
    {
        return Err(Error::InvalidInput(
            "BroScribe could not verify the installation folder".into(),
        ));
    }
    if let Some(root) = root {
        let check = check.ok_or_else(|| {
            Error::Internal("Missing release validation for the removal helper.".into())
        })?;
        (check.validate)(root)?;
    }
    if let Some(profile) = profile {
        if directory_identity(&profile.data_dir)? != Some(profile.identity) {
            return Err(Error::InvalidInput(
                "The profile directory changed before after-exit removal.".into(),
            ));
        }
        let target = if profile.remove_data {
            profile.data_dir.clone()
        } else {
            profile.data_dir.join("run")
        };
        if profile.protection.conflicts(&target, true)
            || profile.remove_update_runner
                && profile
                    .protection
                    .conflicts(&profile.data_dir.join("update-runner"), true)
        {
            return Err(Error::InvalidInput(
                "The profile removal path overlaps a protected location".into(),
            ));
        }
    }
    if let Some(cleanup) = profile.and_then(|profile| profile.default_cleanup.as_ref())
        && (directory_identity(&cleanup.data_dir)? != Some(cleanup.identity)
            || cleanup
                .remove_data
                .as_ref()
                .is_some_and(|protection| protection.conflicts(&cleanup.data_dir, true)))
    {
        return Err(Error::InvalidInput(
            "The default profile changed or overlaps a protected location.".into(),
        ));
    }
    let mut ids = vec![std::process::id().to_string()];
    if let Some(installation) = installation {
        let processes = crate::WindowsUpdatePlatform::new()
            .installation_processes(&installation.canonicalize()?)?;
        ids.extend(processes.iter().map(|process| process.pid.to_string()));
    }
    let system_dir = crate::native::system_directory()?;
    let run = create_helper_folder(&std::env::temp_dir())?;
    let result = (|| {
        let receipt = write_helper_files(&run)?;
        let mut command = after_exit_command(
            &system_dir,
            root,
            profile,
            &receipt,
            ids.join(","),
            check.map_or(&[], |check| check.windows_update_cleanup),
        )?;
        let plan = if let Some(bytes) = check.and_then(|check| check.windows_inventory) {
            let plan = run.join("release-inventory.json");
            std::fs::write(&plan, bytes)?;
            plan
        } else {
            PathBuf::new()
        };
        command.env("BROSCRIBE_UNINSTALL_RELEASE_INVENTORY", plan);
        command.env("BROSCRIBE_UNINSTALL_REFERENCE", before_launch(&receipt)?);
        crate::shell::spawn_detached_command(&command)?;
        Ok(receipt)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(run);
    }
    result
}

fn write_helper_files(run: &Path) -> Result<PathBuf> {
    let receipt = run.join("receipt.txt");
    std::fs::write(
        &receipt,
        "Waiting up to 15 minutes for BroScribe processes to exit.\n",
    )?;
    std::fs::write(run.join("uninstall-after-exit.ps1"), AFTER_EXIT)?;
    Ok(receipt)
}

fn create_helper_folder(parent: &Path) -> Result<PathBuf> {
    // Win32 may return a short TEMP path. PowerShell and the receipt use
    // the same canonical, non-verbatim spelling for every file in the run.
    let parent = plain_verbatim(&parent.canonicalize().map_err(|e| Error::io_at(parent, e))?);
    let id = unsafe { CoCreateGuid() }
        .map_err(|e| crate::error("naming the uninstall helper folder", e))?;
    let folder = parent.join(format!("broscribe-uninstall-{:032x}", id.to_u128()));
    let path = crate::wide(&folder);
    let security = crate::ipc::PipeSecurity::private_directory()?;
    // Set the protected, inheritable DACL at creation, before writing files.
    unsafe { CreateDirectoryW(PCWSTR(path.as_ptr()), Some(security.attributes())) }
        .map_err(|e| crate::error("creating the private uninstall helper folder", e))?;
    Ok(folder)
}

fn helper_paths(paths: &[PathBuf]) -> Result<String> {
    paths
        .iter()
        .map(|path| {
            stable_path(path)
                .into_os_string()
                .into_string()
                .ok()
                .filter(|path| !path.contains(['\r', '\n']))
                .ok_or_else(|| {
                    Error::InvalidInput(
                        "Could not pass a protected path to the removal helper".into(),
                    )
                })
        })
        .collect::<Result<std::collections::BTreeSet<_>>>()
        .map(|paths| paths.into_iter().collect::<Vec<_>>().join("\n"))
}

fn helper_identity(id: DirectoryIdentity) -> String {
    format!(
        "{:x}:{:x}:{:x}",
        id.volume,
        id.file,
        id.generation.unwrap_or_default()
    )
}

const HELPER_BOOTSTRAP: &str = include_str!("uninstall-bootstrap.ps1");

fn after_exit_command(
    system_dir: &Path,
    root: Option<&Path>,
    profile: Option<&DeferredProfile>,
    receipt: &Path,
    pids: String,
    update_cleanup: &[broscribe_platform::uninstall::DeferredUpdateCleanup],
) -> Result<Command> {
    let powershell = system_dir.join("WindowsPowerShell/v1.0/powershell.exe");
    let mut command = Command::new(powershell);
    let default = profile.and_then(|profile| profile.default_cleanup.as_ref());
    let default_protection = default.and_then(|cleanup| cleanup.remove_data.as_ref());
    let discard_unsaved = profile.is_some_and(|profile| profile.discard_unsaved)
        || update_cleanup.iter().any(|profile| profile.discard_unsaved);
    let update_cleanup = update_cleanup
        .iter()
        .map(|profile| {
            Ok(format!(
                "{}\t{}\t{}\t{}",
                helper_paths(std::slice::from_ref(&profile.data_dir))?,
                helper_identity(profile.identity),
                profile.marker.as_deref().unwrap_or_default(),
                u8::from(profile.remove_data),
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    command
        .current_dir(system_dir)
        .env_remove(broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            HELPER_BOOTSTRAP.trim(),
        ])
        .env(
            "BROSCRIBE_UNINSTALL_SCRIPT",
            receipt.with_file_name("uninstall-after-exit.ps1"),
        )
        .env(
            "BROSCRIBE_UNINSTALL_DISCARD_UNSAVED",
            if discard_unsaved { "1" } else { "0" },
        )
        .env(
            "BROSCRIBE_UNINSTALL_PROFILE_SHAPES",
            broscribe_platform::profile_cleanup::ALLOWLIST,
        )
        .env(
            "BROSCRIBE_UNINSTALL_BACKUP_FILES",
            broscribe_platform::release_layout::ReleaseLayout::Macos
                .allowed_files()
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
        )
        .env("BROSCRIBE_UNINSTALL_UPDATE_CLEANUP", update_cleanup)
        .env(
            "BROSCRIBE_UNINSTALL_ROOT",
            root.unwrap_or_else(|| Path::new("")),
        )
        .env(
            "BROSCRIBE_UNINSTALL_RELEASE_FILES",
            broscribe_platform::release_layout::ReleaseLayout::Windows
                .common_files()
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
        )
        .env(
            "BROSCRIBE_UNINSTALL_ALLOWED_FILES",
            broscribe_platform::release_layout::ReleaseLayout::Windows
                .allowed_files()
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
        )
        .env(
            "BROSCRIBE_UNINSTALL_OPTIONAL_FILES",
            broscribe_platform::release_layout::WINDOWS_FFMPEG.join(","),
        )
        .env(
            "BROSCRIBE_UNINSTALL_PROFILE",
            profile
                .map(|profile| profile.data_dir.as_path())
                .unwrap_or_else(|| Path::new("")),
        )
        .env(
            "BROSCRIBE_UNINSTALL_PROFILE_ID",
            profile
                .map(|p| helper_identity(p.identity))
                .unwrap_or_default(),
        )
        .env(
            "BROSCRIBE_UNINSTALL_PROFILE_MARKER",
            profile
                .and_then(|p| p.marker.as_deref())
                .unwrap_or_default(),
        )
        .env(
            "BROSCRIBE_UNINSTALL_DEFAULT",
            default
                .map(|p| p.data_dir.as_path())
                .unwrap_or_else(|| Path::new("")),
        )
        .env(
            "BROSCRIBE_UNINSTALL_DEFAULT_ID",
            default
                .map(|p| helper_identity(p.identity))
                .unwrap_or_default(),
        )
        .env(
            "BROSCRIBE_UNINSTALL_DEFAULT_MARKER",
            default
                .and_then(|p| p.marker.as_deref())
                .unwrap_or_default(),
        )
        .env(
            "BROSCRIBE_UNINSTALL_DELETE_DEFAULT",
            if default_protection.is_some() {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "BROSCRIBE_UNINSTALL_CREATED_DEFAULT",
            if default.is_some_and(|p| p.created_for_pending_list) {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "BROSCRIBE_UNINSTALL_DEFAULT_OVERLAP",
            default_protection
                .map(|p| helper_paths(&p.overlap))
                .transpose()?
                .unwrap_or_default(),
        )
        .env(
            "BROSCRIBE_UNINSTALL_DEFAULT_CONTAINS",
            default_protection
                .map(|p| helper_paths(&p.contains))
                .transpose()?
                .unwrap_or_default(),
        )
        .env(
            "BROSCRIBE_UNINSTALL_DELETE_DATA",
            if profile.is_some_and(|p| p.remove_data) {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "BROSCRIBE_UNINSTALL_DELETE_UPDATE_RUNNER",
            if profile.is_some_and(|p| p.remove_update_runner) {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "BROSCRIBE_UNINSTALL_PROTECT_OVERLAP",
            profile
                .map(|p| helper_paths(&p.protection.overlap))
                .transpose()?
                .unwrap_or_default(),
        )
        .env(
            "BROSCRIBE_UNINSTALL_PROTECT_CONTAINS",
            profile
                .map(|p| helper_paths(&p.protection.contains))
                .transpose()?
                .unwrap_or_default(),
        )
        .env("BROSCRIBE_UNINSTALL_RECEIPT", receipt)
        .env("BROSCRIBE_UNINSTALL_PROCESSES", pids)
        .env_remove("BROSCRIBE_UNINSTALL_REFERENCE")
        .env_remove("BROSCRIBE_UNINSTALL_RELEASE_INVENTORY");
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recreated_windows_profile_has_a_different_directory_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        let renamed = temp.path().join("original");
        std::fs::create_dir(&path).unwrap();
        let original = directory_identity(&path).unwrap().unwrap();
        std::fs::rename(&path, &renamed).unwrap();
        assert_eq!(directory_identity(&renamed).unwrap(), Some(original));
        assert!(directory_identity(&path).unwrap().is_none());
        std::fs::create_dir(&path).unwrap();
        assert_ne!(directory_identity(&path).unwrap(), Some(original));
    }

    #[test]
    fn after_exit_helper_uses_the_system_directory_and_a_fifteen_minute_wait() {
        let system = Path::new(r"C:\Windows\System32");
        let root = Path::new(r"C:\Synthetic\BroScribe");
        let receipt = Path::new(r"C:\Synthetic\removal.txt");
        use broscribe_platform::uninstall::RemovalProtection;
        let data = PathBuf::from(r"C:\Synthetic\Profile");
        let default = PathBuf::from(r"C:\Synthetic\Default");
        let temp = tempfile::tempdir().unwrap();
        let command = after_exit_command(
            system,
            Some(root),
            Some(&DeferredProfile {
                data_dir: data.clone(),
                identity: DirectoryIdentity {
                    volume: 1,
                    file: 2,
                    generation: Some(3),
                },
                marker: None,
                remove_data: false,
                discard_unsaved: false,
                remove_update_runner: true,
                protection: RemovalProtection::default(),
                default_cleanup: Some(broscribe_platform::uninstall::DeferredDefault {
                    data_dir: default.clone(),
                    identity: DirectoryIdentity {
                        volume: 4,
                        file: 5,
                        generation: Some(6),
                    },
                    marker: Some("0123456789abcdef0123456789abcdef".into()),
                    remove_data: Some(RemovalProtection {
                        overlap: vec![temp.path().join("KeptLibrary")],
                        contains: Vec::new(),
                    }),
                    created_for_pending_list: false,
                }),
            }),
            receipt,
            "123,456".into(),
            &[],
        )
        .unwrap();
        let env = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            env[std::ffi::OsStr::new(broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE)],
            None
        );
        assert_eq!(
            env[std::ffi::OsStr::new("BROSCRIBE_UNINSTALL_PROFILE")],
            Some(data.as_os_str())
        );
        assert_eq!(
            env[std::ffi::OsStr::new("BROSCRIBE_UNINSTALL_DELETE_UPDATE_RUNNER")],
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("BROSCRIBE_UNINSTALL_PROFILE_ID")],
            Some(std::ffi::OsStr::new("1:2:3"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("BROSCRIBE_UNINSTALL_DEFAULT")],
            Some(default.as_os_str())
        );
        assert_eq!(
            env[std::ffi::OsStr::new("BROSCRIBE_UNINSTALL_DEFAULT_ID")],
            Some(std::ffi::OsStr::new("4:5:6"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("BROSCRIBE_UNINSTALL_DELETE_DEFAULT")],
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(command.get_current_dir(), Some(system));
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                HELPER_BOOTSTRAP.trim(),
            ]
        );
        assert_eq!(
            env[std::ffi::OsStr::new("BROSCRIBE_UNINSTALL_SCRIPT")],
            Some(
                receipt
                    .with_file_name("uninstall-after-exit.ps1")
                    .as_os_str()
            )
        );
        assert!(crate::shell::detached_command_line(&command).unwrap().len() < 512);
        assert!(AFTER_EXIT.contains("UtcNow.AddMinutes(15)"));
        assert!(AFTER_EXIT.contains("Assert-ProfileTarget $runner"));
        assert!(AFTER_EXIT.contains("Remove-Owned $runner 'update-runner'"));
    }

    #[test]
    fn helper_command_line_does_not_include_a_long_script_path() {
        let system = Path::new(r"C:\Windows\System32");
        let receipt = PathBuf::from(format!(r"C:\{}\receipt.txt", "🦀".repeat(16_384)));
        let command = after_exit_command(system, None, None, &receipt, String::new(), &[]).unwrap();
        let short = after_exit_command(
            system,
            None,
            None,
            Path::new(r"C:\Synthetic\receipt.txt"),
            String::new(),
            &[],
        )
        .unwrap();
        let line = crate::shell::detached_command_line(&command).unwrap();
        assert_eq!(line, crate::shell::detached_command_line(&short).unwrap());
        assert!(line.len() < 512);
    }

    #[test]
    fn real_after_exit_helper_runs_an_empty_plan_and_does_not_count_surviving_targets() {
        use broscribe_platform::uninstall::RemovalProtection;
        use windows::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{GetExitCodeProcess, TerminateProcess, WaitForSingleObject},
        };

        let temp = tempfile::tempdir().unwrap();
        let sentinel = temp.path().join("leave-alone");
        std::fs::write(&sentinel, "synthetic sibling").unwrap();
        let system = crate::native::system_directory().unwrap();
        let helper_parent = temp.path().join("helper's café");
        std::fs::create_dir(&helper_parent).unwrap();
        let reference = stable_path(&helper_parent).join("BroScribe.uninstall-helper.json");
        let plain_reference = plain_verbatim(&reference);
        for (with_unknowns, foreign_reference, share_mode) in [
            (false, false, None),
            (true, true, None),
            (true, false, Some((0, "checked"))),
            (true, false, Some((1, "removed"))),
        ] {
            let run = create_helper_folder(&helper_parent).unwrap();
            let receipt = write_helper_files(&run).unwrap();
            let mut referenced_receipt = stable_path(&receipt);
            if foreign_reference {
                referenced_receipt.set_file_name("other-receipt.txt");
            }
            let saved = format!(
                "\"{}\"",
                referenced_receipt.to_str().unwrap().replace('\\', "\\\\")
            );
            std::fs::write(&reference, &saved).unwrap();
            let profile = stable_path(&temp.path().join("profile"));
            let unknowns = [
                profile.join("run/keep.txt"),
                profile.join("update-runner/keep.txt"),
            ];
            let deferred = with_unknowns.then(|| {
                for path in &unknowns {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, "synthetic unknown").unwrap();
                }
                DeferredProfile {
                    identity: directory_identity(&profile).unwrap().unwrap(),
                    data_dir: profile,
                    marker: None,
                    remove_data: false,
                    discard_unsaved: false,
                    remove_update_runner: true,
                    protection: RemovalProtection::default(),
                    default_cleanup: None,
                }
            });
            let inventory = run.join("release-inventory.json");
            std::fs::write(&inventory, "[]").unwrap();
            let mut command = after_exit_command(
                &system,
                None,
                deferred.as_ref(),
                &receipt,
                String::new(),
                &[],
            )
            .unwrap();
            command
                .env("BROSCRIBE_UNINSTALL_RELEASE_INVENTORY", inventory)
                .env("BROSCRIBE_UNINSTALL_REFERENCE", &reference);
            let held_reference = share_mode.map(|(share, _)| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(share)
                    .open(&reference)
                    .unwrap()
            });
            let (child, _) = crate::shell::spawn_detached_command(&command).unwrap();
            unsafe {
                if WaitForSingleObject(child.0, 30_000) != WAIT_OBJECT_0 {
                    let _ = TerminateProcess(child.0, 1);
                    panic!("after-exit PowerShell helper did not finish");
                }
                let mut code = 0;
                GetExitCodeProcess(child.0, &mut code).unwrap();
                assert_eq!(
                    code,
                    0,
                    "{:?}",
                    std::fs::read_to_string(receipt.with_extension("txt.details"))
                );
            }
            drop(held_reference);
            assert_eq!(
                std::fs::read_to_string(&receipt).unwrap().trim(),
                if share_mode.is_some() {
                    "Done. Removed 0 entries. Helper reference kept."
                } else if with_unknowns {
                    "Done. Removed 0 entries."
                } else {
                    "Done. Removed 1 entries."
                }
            );
            let details = receipt.with_extension("txt.details");
            let remaining = std::fs::read_dir(&run)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<std::collections::BTreeSet<_>>();
            let expected = std::collections::BTreeSet::from([receipt, details.clone()]);
            let details = std::fs::read_to_string(details).unwrap();
            if let Some((_, action)) = share_mode {
                assert_eq!(std::fs::read_to_string(&reference).unwrap(), saved);
                assert!(
                    details.contains(&format!(
                        "kept: {} (could not be {action}: ",
                        plain_reference.display()
                    )),
                    "{details}"
                );
            } else if with_unknowns {
                assert_eq!(std::fs::read_to_string(&reference).unwrap(), saved);
                assert!(!details.contains(plain_reference.to_str().unwrap()));
            } else {
                assert!(!reference.exists());
                assert!(details.contains(&format!("removed: {}", plain_reference.display())));
            }
            if with_unknowns {
                for path in unknowns {
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), "synthetic unknown");
                    assert!(details.contains(plain_verbatim(&path).to_str().unwrap()));
                }
            }
            assert_eq!(remaining, expected);
            assert_eq!(
                std::fs::read_to_string(&sentinel).unwrap(),
                "synthetic sibling"
            );
            std::fs::remove_dir_all(run).unwrap();
            if with_unknowns {
                std::fs::remove_file(&reference).unwrap();
            }
            assert_eq!(std::fs::read_dir(&helper_parent).unwrap().count(), 0);
        }
    }

    #[test]
    fn after_exit_reference_retries_reads_and_deletes() {
        let temp = tempfile::tempdir().unwrap();
        let root = plain_verbatim(&stable_path(temp.path()));
        let helper = root.join("uninstall-after-exit.ps1");
        std::fs::write(&helper, AFTER_EXIT).unwrap();
        let script = r#"
            $ErrorActionPreference = 'Stop'
            . $env:BROSCRIBE_TEST_HELPER
            $reference = Join-Path $env:BROSCRIBE_TEST_ROOT 'reference.json'
            $receipt = Join-Path $env:BROSCRIBE_TEST_ROOT 'receipt.txt'
            # Release a real sharing lock on the second pause. FileShare.Read
            # allows verification but blocks deletion; None blocks verification.
            foreach ($releaseLock in @($true, $false)) {
              foreach ($share in @([IO.FileShare]::None, [IO.FileShare]::Read)) {
                [IO.File]::WriteAllText($reference, ($receipt | ConvertTo-Json -Compress))
                $held = [IO.File]::Open($reference, [IO.FileMode]::Open, [IO.FileAccess]::Read, $share)
                $script:referencePauses = 0
                function Start-Sleep([int]$Milliseconds) {
                  if ($Milliseconds -le 0) { throw 'Expected a pause between reference attempts' }
                  if (++$script:referencePauses -eq 2 -and $releaseLock) { $held.Dispose() }
                }
                try {
                  $result = Remove-HelperReference $reference $receipt
                  if ($script:referencePauses -ne 2) { throw 'Expected three attempts' }
                  if ($releaseLock) {
                    if ($result.removed -cne $reference -or [IO.File]::Exists($reference)) {
                      throw ('Matching reference was not removed after a retry with share mode ' + $share)
                    }
                  } else {
                    $action = if ($share -eq [IO.FileShare]::Read) { 'removed' } else { 'checked' }
                    if (!$result.kept.StartsWith("kept: $reference (could not be ${action}: ") -or
                        ![IO.File]::Exists($reference)) {
                      throw ('Persistent sharing failure was not reported with share mode ' + $share)
                    }
                  }
                } finally {
                  Remove-Item Function:Start-Sleep
                  $held.Dispose()
                }
              }
            }
            function Start-Sleep { throw 'A permanent reference failure was retried' }
            [IO.File]::WriteAllText($reference, '{invalid')
            $nonRegular = Join-Path $env:BROSCRIBE_TEST_ROOT 'directory.json'
            [void][IO.Directory]::CreateDirectory($nonRegular)
            $badParent = Join-Path $reference 'child.json'
            foreach ($invalid in @($reference, $nonRegular, $badParent)) {
              $result = Remove-HelperReference $invalid $receipt
              if (!$result.kept.StartsWith("kept: $invalid (could not be checked: ")) {
                throw "Invalid helper reference was not reported: $invalid"
              }
            }
        "#;
        let output = std::process::Command::new(
            crate::native::system_directory()
                .unwrap()
                .join("WindowsPowerShell/v1.0/powershell.exe"),
        )
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("BROSCRIBE_TEST_HELPER", helper)
        .env("BROSCRIBE_TEST_ROOT", root)
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn after_exit_cleanup_functions_keep_unknown_windows_files_and_refuse_library_backups() {
        let temp = tempfile::tempdir().unwrap();
        let data = stable_path(&temp.path().join("Synthetic Profile"));
        std::fs::create_dir_all(data.join("archives/2024")).unwrap();
        std::fs::write(data.join("config.toml"), "synthetic settings").unwrap();
        std::fs::write(data.join("archives/2024/keep.txt"), "synthetic archive").unwrap();

        let backup = data.join("app-backups/BroScribe-1.0.0-42-20261126T010000Z.app");
        std::fs::create_dir_all(backup.join("2024/11/synthetic")).unwrap();
        std::fs::write(backup.join("2024/11/synthetic/meeting.json"), "{}").unwrap();

        // Run the helper functions with temp fixtures, without its process
        // waiting, native profile identity or app-removal entry point.
        let helper = temp.path().join("uninstall-after-exit.ps1");
        std::fs::write(&helper, AFTER_EXIT).unwrap();
        let script = r#"
            $ErrorActionPreference = 'Stop'
            . $env:BROSCRIBE_TEST_HELPER
            $shapes = $env:BROSCRIBE_UNINSTALL_PROFILE_SHAPES | ConvertFrom-Json
            $kept = [Collections.Generic.List[string]]::new()
            $keptParents = [Collections.Generic.List[string]]::new()
            $data = Full-Path $env:BROSCRIBE_TEST_PROFILE
            Remove-Owned $data 'data' @('run', 'cache', 'updates', 'update-runner', 'app-backups')
            try { Remove-Owned (Join-Path $data 'app-backups') 'app-backups' }
            catch { Write-Output $_.Exception.Message; exit 2 }
            exit 1
        "#;
        let system = crate::native::system_directory().unwrap();
        let out = Command::new(system.join("WindowsPowerShell/v1.0/powershell.exe"))
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("BROSCRIBE_TEST_HELPER", &helper)
            .env("BROSCRIBE_TEST_PROFILE", &data)
            .env(
                "BROSCRIBE_UNINSTALL_PROFILE_SHAPES",
                broscribe_platform::profile_cleanup::ALLOWLIST,
            )
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("contains a BroScribe library"));
        assert!(backup.exists() && data.join("archives/2024/keep.txt").exists());
        assert!(!data.join("config.toml").exists());
    }
}

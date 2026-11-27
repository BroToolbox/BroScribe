//! Where the daemon's socket and pid file live, and the private folders
//! holding them.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Folder of the socket and pid file inside the data folder.
pub const RUN_DIR: &str = "run";
pub const SOCKET_NAME: &str = "broscribed.sock";
pub const PID_NAME: &str = "broscribed.pid";
/// Size of `sockaddr_un.sun_path` on macOS (the smallest of the supported
/// systems), terminating NUL included: a socket path must be shorter.
pub const SUN_PATH_BYTES: usize = 104;

/// The socket path and the folder that must be private to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketLocation {
    pub dir: PathBuf,
    pub path: PathBuf,
    /// The OS-specific primary socket location could not be used.
    pub fallback: bool,
}

impl SocketLocation {
    pub fn in_dir(dir: PathBuf, filename: &str, fallback: bool) -> io::Result<Self> {
        let path = dir.join(filename);
        if !fits_sun_path(&path) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Unix socket path {} is too long for sun_path",
                    path.display()
                ),
            ));
        }
        Ok(Self {
            dir,
            path,
            fallback,
        })
    }
}

/// The real user ID of this process.
pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// The account home, independent of the caller's HOME or profile overrides.
pub fn account_home() -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 4096];
    loop {
        // SAFETY: the result points into live storage owned by this call.
        let status = unsafe {
            libc::getpwuid_r(
                current_uid(),
                pwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE {
            buffer.resize(buffer.len().checked_mul(2)?, 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        // SAFETY: a successful lookup supplies a NUL-terminated home directory.
        let home = unsafe { CStr::from_ptr((*result).pw_dir) };
        let path = PathBuf::from(OsStr::from_bytes(home.to_bytes()));
        return path.is_absolute().then_some(path);
    }
}

#[cfg(any(target_os = "linux", test))]
fn uid_runtime_dir(tmp: &Path, uid: u32) -> PathBuf {
    tmp.join(format!("broscribe-{uid}"))
}

#[cfg(any(target_os = "macos", test))]
const DARWIN_LOGIN_REGISTRATION_LOCK: &str = "org.broscribe.login-registration.lock";

/// Opens the same account-wide registration lock for every profile of a uid.
#[cfg(target_os = "macos")]
pub fn open_login_registration_lock() -> io::Result<(PathBuf, File)> {
    let dir = darwin_user_dir()?;
    open_registration_lock_file(dir.join(DARWIN_LOGIN_REGISTRATION_LOCK), current_uid())
}

/// The account's system-managed directory, shared by the lock and fallback sockets.
#[cfg(target_os = "macos")]
pub fn darwin_user_dir() -> io::Result<PathBuf> {
    darwin_user_dir_for(current_uid(), |buffer| {
        // SAFETY: the buffer is writable for the full length passed to confstr.
        let size = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_DIR,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(size)
    })
    .map_err(|error| io::Error::other(format!("resolving the macOS user directory: {error}")))
}

#[cfg(any(target_os = "macos", test))]
fn darwin_user_dir_for(
    uid: u32,
    confstr: impl FnOnce(&mut [u8]) -> io::Result<usize>,
) -> io::Result<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let mut buffer = [0; libc::PATH_MAX as usize];
    let size = confstr(&mut buffer)?;
    let invalid_path = || io::Error::new(ErrorKind::InvalidData, "invalid Darwin user directory");
    let bytes = buffer.get(..size).ok_or_else(invalid_path)?;
    let path = CStr::from_bytes_with_nul(bytes).map_err(|_| invalid_path())?;
    let path = Path::new(OsStr::from_bytes(path.to_bytes()));
    if !path.is_absolute() {
        return Err(invalid_path());
    }
    // confstr returns a trailing slash, which would make lstat follow a symlink.
    let dir: PathBuf = path.components().collect();
    check_system_registration_dir(&dir, uid)?;
    Ok(dir)
}

/// Opens the same account-wide registration lock for every profile of a uid.
#[cfg(target_os = "linux")]
pub fn open_login_registration_lock() -> io::Result<(PathBuf, File)> {
    open_linux_login_registration_lock(Path::new("/run/user"), Path::new("/tmp"), current_uid())
}

#[cfg(any(target_os = "linux", test))]
fn open_linux_login_registration_lock(
    run_user: &Path,
    tmp: &Path,
    uid: u32,
) -> io::Result<(PathBuf, File)> {
    match linux_user_dir_for(run_user, uid) {
        Ok(dir) => open_registration_lock_file(dir.join("broscribe-login-registration.lock"), uid),
        // With no logind runtime directory there is no systemd user manager.
        // Every caller uses the same roots; callers can select different paths
        // if logind creates the runtime directory during a fallback operation.
        Err(error) if error.kind() == ErrorKind::NotFound => {
            open_tmp_login_registration_lock(tmp, uid)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
pub fn linux_socket_location(
    filename: &str,
    xdg_runtime_dir: Option<&Path>,
) -> io::Result<SocketLocation> {
    linux_socket_location_for(
        filename,
        xdg_runtime_dir,
        Path::new("/run/user"),
        Path::new("/tmp"),
        current_uid(),
    )
}

#[cfg(any(target_os = "linux", test))]
fn linux_socket_location_for(
    filename: &str,
    xdg_runtime_dir: Option<&Path>,
    run_user: &Path,
    tmp: &Path,
    uid: u32,
) -> io::Result<SocketLocation> {
    if let Some(dir) =
        xdg_runtime_dir.filter(|dir| dir.is_absolute() && check_private_dir(dir).is_ok())
        && let Ok(location) = SocketLocation::in_dir(dir.join("broscribe"), filename, false)
    {
        return Ok(location);
    }
    match linux_user_dir_for(run_user, uid) {
        Ok(dir) => SocketLocation::in_dir(dir.join("broscribe"), filename, true),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            SocketLocation::in_dir(uid_runtime_dir(tmp, uid), filename, true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_user_dir_for(run_user: &Path, uid: u32) -> io::Result<PathBuf> {
    let dir = run_user.join(uid.to_string());
    check_system_registration_dir(&dir, uid)?;
    Ok(dir)
}

fn check_system_registration_dir(dir: &Path, uid: u32) -> io::Result<()> {
    let meta = owned_dir(dir, uid)?;
    if meta.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is writable by other users", dir.display()),
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn open_tmp_login_registration_lock(tmp: &Path, uid: u32) -> io::Result<(PathBuf, File)> {
    let dir = uid_runtime_dir(tmp, uid);
    ensure_private_dir(&dir)?;
    if owned_dir(&dir, uid)?.mode() & 0o777 != 0o700 {
        fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
    }
    open_registration_lock_file(dir.join("login-registration.lock"), uid)
}

fn open_registration_lock_file(path: PathBuf, uid: u32) -> io::Result<(PathBuf, File)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        // A FIFO must not block the open before we can reject it.
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                io::Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{} is a symlink", path.display()),
                )
            } else {
                error
            }
        })?;
    validate_login_registration_lock(&path, &file, uid)?;
    Ok((path, file))
}

fn validate_login_registration_lock(path: &Path, file: &File, uid: u32) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.uid() != uid {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} belongs to another user", path.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        file.set_permissions(Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Whether `path` fits `sun_path` with its terminating NUL.
pub fn fits_sun_path(path: &Path) -> bool {
    path.as_os_str().len() < SUN_PATH_BYTES
}

/// Creates `dir` (mode 0700; parents with default modes) if needed, then
/// checks that it is a real folder (not a symlink) owned by this user and
/// tightens its mode to 0700 when group or others had access.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = owned_dir(dir, current_uid())?;
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Checks without creating anything: `dir` is a real folder owned by this
/// user that group and others cannot access. `NotFound` passes through.
pub fn check_private_dir(dir: &Path) -> io::Result<()> {
    let meta = owned_dir(dir, current_uid())?;
    if meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is accessible to other users", dir.display()),
        ));
    }
    Ok(())
}

fn owned_dir(dir: &Path, uid: u32) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is not a plain folder", dir.display()),
        ));
    }
    if meta.uid() != uid {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} belongs to another user", dir.display()),
        ));
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_must_leave_space_for_a_terminator() {
        // Exactly at the limit: 103 bytes fit, 104 do not.
        assert!(fits_sun_path(Path::new(&"a".repeat(SUN_PATH_BYTES - 1))));
        assert!(!fits_sun_path(Path::new(&"a".repeat(SUN_PATH_BYTES))));
    }

    #[test]
    fn linux_sockets_prefer_xdg_then_protected_runtime_then_tmp() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let run_user = root.path().join("run/user");
        let tmp = root.path().join("tmp");
        let xdg = root.path().join("xdg");
        let uid = current_uid();
        let runtime_dir = run_user.join(uid.to_string());
        ensure_private_dir(&runtime_dir).unwrap();
        ensure_private_dir(&xdg).unwrap();
        let filename = "broscribed-0123456789abcdef.sock";
        let locate = |xdg| linux_socket_location_for(filename, xdg, &run_user, &tmp, uid).unwrap();

        let primary = locate(Some(&xdg));
        assert_eq!(primary.path, xdg.join("broscribe").join(filename));
        assert!(!primary.fallback);

        let too_long = xdg.join("x".repeat(90));
        ensure_private_dir(&too_long).unwrap();
        let protected = locate(None);
        assert_eq!(protected.path, runtime_dir.join("broscribe").join(filename));
        assert!(protected.fallback);
        assert_eq!(locate(Some(&too_long)), protected);

        fs::set_permissions(&xdg, Permissions::from_mode(0o770)).unwrap();
        assert_eq!(locate(Some(&xdg)), protected);
        fs::set_permissions(&runtime_dir, Permissions::from_mode(0o770)).unwrap();
        let error =
            linux_socket_location_for(filename, Some(&xdg), &run_user, &tmp, uid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        fs::remove_dir(&runtime_dir).unwrap();
        let fallback = locate(None);
        assert_eq!(fallback.path, uid_runtime_dir(&tmp, uid).join(filename));
        assert!(fallback.fallback);
    }

    #[test]
    fn private_dirs_are_created_and_tightened() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        ensure_private_dir(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        assert!(check_private_dir(&dir).is_err());
        ensure_private_dir(&dir).unwrap();
        check_private_dir(&dir).unwrap();
        assert_eq!(
            check_private_dir(&tmp.path().join("absent"))
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn symlinked_dirs_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        ensure_private_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            ensure_private_dir(&link).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
    }

    fn assert_registration_lock_contention(
        expected: &Path,
        open: impl Fn() -> io::Result<(PathBuf, File)>,
    ) {
        let (first_path, first) = open().unwrap();
        assert_eq!(first_path, expected);
        first.try_lock().unwrap();

        let (second_path, second) = open().unwrap();
        assert_eq!(second_path, expected);
        assert!(matches!(
            second.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        // Unlock explicitly, as the guard does: a child forked by a parallel
        // test can briefly hold a copy of this descriptor.
        first.unlock().unwrap();
        drop(first);
        second.try_lock().unwrap();
        second.unlock().unwrap();
    }

    fn open_fake_darwin_lock(dir: &Path, uid: u32) -> io::Result<(PathBuf, File)> {
        use std::os::unix::ffi::OsStrExt;

        // The real confstr includes a trailing slash and the terminating NUL.
        let mut path = dir.as_os_str().as_bytes().to_vec();
        path.extend_from_slice(b"/\0");
        let dir = darwin_user_dir_for(uid, |buffer| {
            buffer[..path.len()].copy_from_slice(&path);
            Ok(path.len())
        })?;
        open_registration_lock_file(dir.join(DARWIN_LOGIN_REGISTRATION_LOCK), uid)
    }

    #[test]
    fn darwin_registration_lock_uses_confstr_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("0");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        assert_registration_lock_contention(
            &dir.join("org.broscribe.login-registration.lock"),
            || open_fake_darwin_lock(&dir, current_uid()),
        );
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn darwin_registration_lock_fails_when_confstr_fails() {
        let error = darwin_user_dir_for(current_uid(), |_| {
            Err(io::Error::from_raw_os_error(libc::EIO))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn darwin_registration_lock_refuses_invalid_directories() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("0");
        ensure_private_dir(&dir).unwrap();
        let symlink = root.path().join("link");
        std::os::unix::fs::symlink(&dir, &symlink).unwrap();

        for (path, uid) in [
            (symlink.as_path(), current_uid()),
            (&dir, current_uid() ^ 1),
        ] {
            assert_eq!(
                open_fake_darwin_lock(path, uid).unwrap_err().kind(),
                ErrorKind::PermissionDenied
            );
        }
        for mode in [0o720, 0o702] {
            fs::set_permissions(&dir, Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                open_fake_darwin_lock(&dir, current_uid())
                    .unwrap_err()
                    .kind(),
                ErrorKind::PermissionDenied
            );
            assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, mode);
        }
        assert!(!dir.join("org.broscribe.login-registration.lock").exists());
    }

    #[test]
    fn linux_registration_lock_uses_valid_runtime_directory() {
        let root = tempfile::tempdir().unwrap();
        let run_user = root.path().join("run/user");
        let tmp = root.path().join("tmp");
        let uid = current_uid();
        let dir = run_user.join(uid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();

        assert_registration_lock_contention(&dir.join("broscribe-login-registration.lock"), || {
            open_linux_login_registration_lock(&run_user, &tmp, uid)
        });
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o755);
        assert!(!tmp.exists());
    }

    #[test]
    fn linux_registration_lock_falls_back_when_runtime_directory_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let run_user = root.path().join("run/user");
        fs::create_dir_all(&run_user).unwrap();
        let tmp = root.path().join("tmp");
        let uid = current_uid();
        let runtime = uid_runtime_dir(&tmp, uid);
        assert_registration_lock_contention(&runtime.join("login-registration.lock"), || {
            open_linux_login_registration_lock(&run_user, &tmp, uid)
        });
        assert_eq!(fs::metadata(&runtime).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&runtime).unwrap().uid(), uid);
        assert!(!run_user.join(uid.to_string()).exists());
    }

    #[test]
    fn linux_registration_lock_never_falls_back_for_an_invalid_runtime_directory() {
        let root = tempfile::tempdir().unwrap();
        let run_user = root.path().join("run/user");
        let tmp = root.path().join("tmp");
        let uid = current_uid();
        let dir = run_user.join(uid.to_string());
        fs::create_dir_all(&run_user).unwrap();

        // A dangling symlink exists and must not be mistaken for an absent directory.
        std::os::unix::fs::symlink(root.path().join("missing"), &dir).unwrap();
        let open = || open_linux_login_registration_lock(&run_user, &tmp, uid);
        assert_eq!(open().unwrap_err().kind(), ErrorKind::PermissionDenied);
        fs::remove_file(&dir).unwrap();
        fs::write(&dir, "synthetic file").unwrap();
        assert_eq!(open().unwrap_err().kind(), ErrorKind::PermissionDenied);
        fs::remove_file(&dir).unwrap();

        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o770)).unwrap();
        assert_eq!(open().unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o770);

        let other_uid = uid ^ 1;
        let other_dir = run_user.join(other_uid.to_string());
        ensure_private_dir(&other_dir).unwrap();
        let error = open_linux_login_registration_lock(&run_user, &tmp, other_uid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("belongs to another user"));
        assert!(!tmp.exists());
    }

    #[test]
    fn registration_lock_refuses_a_leaf_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = uid_runtime_dir(tmp.path(), current_uid());
        ensure_private_dir(&dir).unwrap();
        let target = tmp.path().join("symlink-target");
        std::os::unix::fs::symlink(&target, dir.join("login-registration.lock")).unwrap();
        let error = open_tmp_login_registration_lock(tmp.path(), current_uid()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("is a symlink"), "{error}");
        assert!(!target.exists());
    }

    #[test]
    fn registration_lock_refuses_a_fifo_or_directory() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = uid_runtime_dir(tmp.path(), current_uid());
        ensure_private_dir(&dir).unwrap();
        let path = dir.join("login-registration.lock");
        let fifo = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is NUL-terminated and lives through the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(open_tmp_login_registration_lock(tmp.path(), current_uid()).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(open_tmp_login_registration_lock(tmp.path(), current_uid()).is_err());
    }

    #[test]
    fn registration_lock_refuses_a_foreign_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, file) = open_tmp_login_registration_lock(tmp.path(), current_uid()).unwrap();
        let error = validate_login_registration_lock(&path, &file, current_uid() ^ 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("belongs to another user"),
            "{error}"
        );
    }

    #[test]
    fn registration_lock_tightens_loose_permissions_and_preserves_a_normal_file() {
        let tmp = tempfile::tempdir().unwrap();
        let open = || open_tmp_login_registration_lock(tmp.path(), current_uid());
        let (path, created) = open().unwrap();
        assert_eq!(created.metadata().unwrap().mode() & 0o777, 0o600);
        fs::write(&path, "synthetic mark").unwrap();
        created
            .set_permissions(Permissions::from_mode(0o644))
            .unwrap();

        let (_, tightened) = open().unwrap();
        let private = tightened.metadata().unwrap();
        assert_eq!(private.mode() & 0o777, 0o600);
        let (_, reopened) = open().unwrap();
        let normal = reopened.metadata().unwrap();
        assert_eq!(normal.mode(), private.mode());
        assert_eq!(normal.ino(), private.ino());
        assert_eq!(fs::read_to_string(path).unwrap(), "synthetic mark");
    }

    #[test]
    fn registration_lock_refuses_a_symlinked_uid_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("elsewhere");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, uid_runtime_dir(tmp.path(), current_uid())).unwrap();
        let err = open_tmp_login_registration_lock(tmp.path(), current_uid()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(!real.join("login-registration.lock").exists());
    }
}

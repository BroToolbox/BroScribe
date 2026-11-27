use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use broscribe_platform::about::install_folder;
use broscribe_platform::uninstall::ReleaseRemovalCheck;
use broscribe_platform::uninstall::{Layout, owns_namespace, retire_release_tree};
use broscribe_platform::{AppPaths, Error, Result, SecretStore, stable_path};
use broscribe_platform_unix::dirs::current_uid;

pub const SECRET_BASE: &str = crate::SECRET_SERVICE_BASE;
const APP_ID: &str = "org.broscribe.BroScribe";

fn release_root(executable: &Path) -> Option<PathBuf> {
    let executable = executable.canonicalize().ok()?;
    let folder = executable.parent()?;
    let root = install_folder(&executable)?;
    (folder == root
        && matches!(
            executable.file_name()?.to_str()?,
            "broscribe" | "broscribe-app"
        ))
    .then_some(root)
}

const DESKTOP_ENTRY: &str = "applications/org.broscribe.BroScribe.desktop";
const DESKTOP_TEMPLATE: &str =
    include_str!("../../../packaging/linux/org.broscribe.BroScribe.desktop");
const DESKTOP_MARKER: &str = "X-BroScribe-Managed=true";

/// Each launcher path and its exact generated target relative to the release root.
fn launcher_paths(data: &Path, home: &Path) -> impl Iterator<Item = (PathBuf, PathBuf)> {
    [
        (
            data.join(DESKTOP_ENTRY),
            Path::new("share").join(DESKTOP_ENTRY),
        ),
        (
            home.join(".local/bin/broscribe"),
            PathBuf::from("broscribe"),
        ),
    ]
    .into_iter()
    .chain([22, 32, 64, 128].map(|size| {
        let icon = format!("icons/hicolor/{size}x{size}/apps/{APP_ID}.png");
        (data.join(&icon), Path::new("share").join(icon))
    }))
}

pub fn layout(executable: &Path) -> Result<Layout> {
    let data = crate::LinuxPaths::new().data_dir();
    layout_in(
        executable,
        data.parent().expect("XDG data folder"),
        &crate::machine::home_dir(),
    )
}

fn layout_in(executable: &Path, data: &Path, home: &Path) -> Result<Layout> {
    let installation = release_root(executable);
    let mut desktop_paths = Vec::new();
    if let Some(root) = &installation {
        for (path, target) in launcher_paths(data, home) {
            if desktop_owner(&path, root, &target, &data.join(DESKTOP_ENTRY))? == Some(true) {
                desktop_paths.push(path);
            }
        }
    }
    Ok(Layout {
        installation,
        shell_retry: true,
        desktop_paths,
        ..Layout::default()
    })
}

pub fn remove_desktop_entry(_executable: &Path, path: &Path, root: &Path) -> Result<bool> {
    let data = crate::LinuxPaths::new().data_dir();
    remove_desktop_entry_in(
        path,
        root,
        data.parent().expect("XDG data folder"),
        &crate::machine::home_dir(),
    )
}

fn remove_desktop_entry_in(path: &Path, root: &Path, data: &Path, home: &Path) -> Result<bool> {
    let Some((_, target)) = launcher_paths(data, home).find(|(launcher, _)| launcher == path)
    else {
        return Err(Error::InvalidInput(
            "The launcher path is outside BroScribe's launcher locations.".into(),
        ));
    };
    match desktop_owner(path, root, &target, &data.join(DESKTOP_ENTRY))? {
        Some(true) => match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::io_at(path, e)),
        },
        Some(false) => Err(Error::InvalidInput(
            "The launcher now belongs to another installation.".into(),
        )),
        None => Ok(false),
    }
}

/// Missing, ours, or present but unowned. Check both at discovery and
/// removal so a launcher replaced after the removal plan stays in place.
fn desktop_owner(
    path: &Path,
    root: &Path,
    relative_target: &Path,
    desktop_entry: &Path,
) -> Result<Option<bool>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io_at(path, e)),
    };
    if meta.is_symlink() {
        let target = std::fs::read_link(path).map_err(|e| Error::io_at(path, e))?;
        let target = path.parent().expect("launcher has a parent").join(target);
        let expected = root.join(relative_target);
        // Resolve aliases to the parent (including a symlinked home), but
        // compare the leaf name itself even when its payload is missing.
        let normalized = target
            .parent()
            .zip(target.file_name())
            .map(|(parent, name)| stable_path(parent).join(name));
        return Ok(Some(
            target == expected || normalized.as_deref() == Some(&expected),
        ));
    }
    if !meta.is_file() || path != desktop_entry {
        return Ok(Some(false));
    }
    const MAX_DESKTOP_BYTES: u64 = 64 * 1024;
    let mut bytes = Vec::new();
    broscribe_platform::open_regular_nofollow(std::fs::OpenOptions::new().read(true), path)
        .and_then(|file| file.take(MAX_DESKTOP_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| Error::io_at(path, e))?;
    let launcher = (bytes.len() as u64 <= MAX_DESKTOP_BYTES)
        .then(|| std::str::from_utf8(&bytes).ok())
        .flatten();
    Ok(Some(launcher.is_some_and(|text| {
        owned_desktop_text(text)
            && launcher_executable(text).is_some_and(|target| {
                target.is_absolute() && stable_path(&target) == root.join("broscribe-app")
            })
    })))
}

fn owned_desktop_text(text: &str) -> bool {
    let marked = text
        .lines()
        .skip_while(|line| *line != "[Desktop Entry]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .any(|line| line == DESKTOP_MARKER);
    marked
        || text
            .lines()
            .map(|line| {
                if line.starts_with("Exec=") {
                    "Exec=broscribe-app"
                } else {
                    line
                }
            })
            .eq(DESKTOP_TEMPLATE
                .lines()
                .filter(|line| *line != DESKTOP_MARKER))
}

fn launcher_executable(launcher: &str) -> Option<PathBuf> {
    let mut lines = launcher
        .lines()
        .skip_while(|line| *line != "[Desktop Entry]");
    lines.next()?;
    let mut exec = lines
        .take_while(|line| !line.starts_with('['))
        .filter_map(|line| line.strip_prefix("Exec="));
    let value = exec.next()?.strip_prefix('"')?.strip_suffix('"')?;
    if exec.next().is_some() {
        return None;
    }
    // Invert install.sh's desktop value escaping first, then its escaping
    // inside the quoted Exec token (including literal percent field codes).
    let mut value_chars = value.chars();
    let mut token = String::new();
    while let Some(c) = value_chars.next() {
        if c == '\\' && value_chars.next()? != '\\' {
            return None;
        }
        token.push(c);
    }
    let mut chars = token.chars();
    let mut executable = String::new();
    while let Some(c) = chars.next() {
        executable.push(match c {
            '\\' => match chars.next()? {
                c @ ('\\' | '"' | '$' | '`') => c,
                _ => return None,
            },
            '%' if chars.next()? == '%' => '%',
            '%' | '"' | '$' | '`' => return None,
            c if c.is_ascii_control() => return None,
            c => c,
        });
    }
    Some(PathBuf::from(executable))
}

pub fn validate_installation(root: &Path, has_inventory: bool) -> Result<()> {
    if has_inventory {
        return Ok(());
    }
    broscribe_platform::release_layout::ReleaseLayout::Linux.validate(root, false)
}

pub fn remove_installation(
    executable: &Path,
    root: &Path,
    check: &ReleaseRemovalCheck<'_>,
) -> Result<Option<PathBuf>> {
    if release_root(executable).as_deref() != Some(root) {
        return Err(Error::InvalidInput(
            "BroScribe could not verify the installation folder".into(),
        ));
    }
    let platform = crate::LinuxUpdatePlatform::for_executable(executable.to_path_buf());
    retire_release_tree(root, &platform, check.validate)?;
    Ok(None)
}

pub fn runtime_paths(data_dir: &Path) -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::FileTypeExt as _;
    let location = crate::socket_location(data_dir)?;
    let filename = location.path.file_name().expect("agent socket");
    let app = filename.to_string_lossy().replacen("broscribed", "app", 1);
    let roots = runtime_roots(location.dir);
    let mut paths = Vec::new();
    for root in roots {
        if broscribe_platform_unix::check_private_dir(&root).is_ok() {
            for name in [filename, app.as_ref()] {
                let path = root.join(name);
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.file_type().is_socket() => paths.push(path),
                    // Removing a socket also considers its parent for empty
                    // directory cleanup, including after a clean agent exit.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => paths.push(path),
                    _ => {}
                }
            }
        }
    }
    Ok(paths)
}

fn runtime_roots(current: PathBuf) -> Vec<PathBuf> {
    let mut roots = vec![
        current,
        PathBuf::from(format!("/run/user/{}/broscribe", current_uid())),
        PathBuf::from(format!("/tmp/broscribe-{}", current_uid())),
    ];
    roots.sort();
    roots.dedup();
    roots
}

pub fn remove_runtime_files(
    data_dir: &Path,
) -> Result<broscribe_platform::profile_cleanup::Cleanup> {
    let location = crate::socket_location(data_dir)?;
    let endpoint_lock = location.path.with_extension("lock");
    let name = endpoint_lock.file_name().expect("agent endpoint lock");
    let roots = runtime_roots(location.dir);
    // Tray assets use XDG_RUNTIME_DIR even when its socket path is too long.
    let tray_root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|runtime| {
            runtime.is_absolute() && broscribe_platform_unix::check_private_dir(runtime).is_ok()
        })
        .map(|runtime| runtime.join("broscribe"));
    remove_runtime_files_in(name, &roots, tray_root.as_deref())
}

fn remove_runtime_files_in(
    endpoint_lock: &std::ffi::OsStr,
    roots: &[PathBuf],
    tray_root: Option<&Path>,
) -> Result<broscribe_platform::profile_cleanup::Cleanup> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut cleanup = broscribe_platform::profile_cleanup::Cleanup::default();
    for root in roots {
        if broscribe_platform_unix::check_private_dir(root).is_err() {
            continue;
        }
        let lock_path = root.join(endpoint_lock);
        if regular_runtime_file(&lock_path, &mut cleanup)? {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&lock_path)
                .map_err(|error| Error::io_at(&lock_path, error))?;
            match file.try_lock() {
                Ok(()) => remove_runtime_file(&lock_path, &mut cleanup)?,
                Err(std::fs::TryLockError::WouldBlock) => cleanup.kept.push(lock_path),
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(Error::io_at(&lock_path, error));
                }
            }
        }
        if root.file_name().is_some_and(|name| {
            name == "broscribe" || name == format!("broscribe-{}", current_uid()).as_str()
        }) {
            remove_empty(root, &mut cleanup)?;
        }
    }
    if let Some(root) = tray_root
        && broscribe_platform_unix::check_private_dir(root).is_ok()
    {
        let icons = root.join("icons");
        if broscribe_platform_unix::check_private_dir(&icons).is_ok() {
            for name in crate::status::symbolic_icon_names() {
                remove_runtime_file(&icons.join(format!("{name}.svg")), &mut cleanup)?;
            }
            remove_empty(&icons, &mut cleanup)?;
        }
        remove_empty(root, &mut cleanup)?;
    }
    Ok(cleanup)
}

fn regular_runtime_file(
    path: &Path,
    cleanup: &mut broscribe_platform::profile_cleanup::Cleanup,
) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => {
            cleanup.kept.push(path.to_path_buf());
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io_at(path, error)),
    }
}

fn remove_runtime_file(
    path: &Path,
    cleanup: &mut broscribe_platform::profile_cleanup::Cleanup,
) -> Result<()> {
    if regular_runtime_file(path, cleanup)? {
        match std::fs::remove_file(path) {
            Ok(()) => cleanup.changed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io_at(path, error)),
        }
    }
    Ok(())
}

fn remove_empty(
    path: &Path,
    cleanup: &mut broscribe_platform::profile_cleanup::Cleanup,
) -> Result<()> {
    use std::io::ErrorKind::{DirectoryNotEmpty, NotFound};
    match std::fs::remove_dir(path) {
        Ok(()) => cleanup.changed = true,
        Err(error) if matches!(error.kind(), NotFound | DirectoryNotEmpty) => {}
        Err(error) => return Err(Error::io_at(path, error)),
    }
    Ok(())
}

pub fn registration_lock_paths() -> Result<Vec<PathBuf>> {
    let uid = current_uid();
    Ok(vec![
        PathBuf::from(format!("/run/user/{uid}/broscribe-login-registration.lock")),
        PathBuf::from(format!("/tmp/broscribe-{uid}/login-registration.lock")),
    ])
}

pub fn remove_credentials(services: &[String], all: bool) -> Result<usize> {
    let Some(entries) = list_credentials(services, all)? else {
        return Ok(0);
    };
    let mut count = 0;
    let mut failed = 0;
    for (service, key) in entries {
        if crate::LinuxSecretStore::with_service(&service)
            .delete(&key)
            .is_ok()
        {
            count += 1;
        } else {
            failed += 1;
        }
    }
    if failed != 0 {
        return Err(Error::Internal(format!(
            "removed {count} Secret Service entries; {failed} failed"
        )));
    }
    Ok(count)
}

pub(crate) fn secret_service_missing(service: &str) -> bool {
    matches!(list_credentials(&[service.to_owned()], false), Ok(None))
}

fn session_bus_unreachable(_: zbus::Error) -> Error {
    Error::Service {
        code: "uninstall_session_bus_unreachable",
        message: "Could not reach the Secret Service session bus. Run broscribe uninstall from your desktop session, or add --skip-credentials if this account never stored any.".into(),
    }
}

fn no_secret_service(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
        ),
        zbus::Error::FDO(error) => matches!(
            error.as_ref(),
            zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
        ),
        _ => false,
    }
}

/// Secret Service SearchItems takes exact attributes. The explicit bulk
/// cleanup can enumerate service attributes without unlocking any items.
fn list_credentials(
    services: &[String],
    all: bool,
) -> Result<Option<std::collections::BTreeSet<(String, String)>>> {
    use std::collections::{BTreeSet, HashMap};
    use std::time::Duration;
    use zbus::{
        blocking::{Proxy, connection::Builder},
        zvariant::OwnedObjectPath,
    };
    const BUS: &str = "org.freedesktop.secrets";
    let error = |_| Error::Internal("could not list BroScribe Secret Service entries".into());
    let connection = Builder::session()
        .map_err(session_bus_unreachable)?
        .method_timeout(Duration::from_secs(3))
        .build()
        .map_err(session_bus_unreachable)?;
    let proxy = match Proxy::new(
        &connection,
        BUS,
        "/org/freedesktop/secrets",
        "org.freedesktop.Secret.Service",
    ) {
        Ok(proxy) => proxy,
        Err(error) if no_secret_service(&error) => return Ok(None),
        Err(e) => return Err(error(e)),
    };
    let filters = if all {
        vec![HashMap::new()]
    } else {
        services
            .iter()
            .map(|service| HashMap::from([("service", service.as_str())]))
            .collect()
    };
    let mut entries = BTreeSet::new();
    for filter in filters {
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            match proxy.call("SearchItems", &(filter,)) {
                Ok(items) => items,
                Err(error) if no_secret_service(&error) => return Ok(None),
                Err(e) => return Err(error(e)),
            };
        for path in unlocked.into_iter().chain(locked) {
            let item =
                Proxy::new(&connection, BUS, path, "org.freedesktop.Secret.Item").map_err(error)?;
            let attrs: HashMap<String, String> = item.get_property("Attributes").map_err(error)?;
            if let (Some(service), Some(key)) = (attrs.get("service"), attrs.get("username"))
                && (services.contains(service) || all && owns_namespace(SECRET_BASE, service))
            {
                entries.insert((service.clone(), key.clone()));
            }
        }
    }
    Ok(Some(entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    const HELPER: &str = "BROSCRIBE_TEST_LINUX_INSTALLER_HELPER";

    #[test]
    fn uninstall_distinguishes_an_unreachable_bus_from_an_absent_service() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::ConnectionRefused,
            ErrorKind::PermissionDenied,
        ] {
            let io = zbus::Error::InputOutput(std::io::Error::from(kind).into());
            assert!(!no_secret_service(&io));
            let error = session_bus_unreachable(io);
            assert_eq!(error.code(), "uninstall_session_bus_unreachable");
            assert!(error.to_string().contains("desktop session"));
            assert!(error.to_string().contains("--skip-credentials"));
        }
        let bus = |error| zbus::Error::FDO(Box::new(error));
        assert!(no_secret_service(&bus(zbus::fdo::Error::ServiceUnknown(
            "synthetic".into()
        ))));
        assert!(no_secret_service(&bus(zbus::fdo::Error::NameHasNoOwner(
            "synthetic".into()
        ))));
        assert!(!no_secret_service(&bus(zbus::fdo::Error::AccessDenied(
            "synthetic".into()
        ))));
        assert!(!no_secret_service(&bus(zbus::fdo::Error::Failed(
            "collection is locked".into()
        ))));
    }

    #[test]
    #[ignore = "synthetic CLI invoked explicitly by the installer round trip"]
    fn installer_uninstall_helper() {
        let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap());
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let cli = PathBuf::from(std::env::var_os(HELPER).unwrap());
        assert!(cli.starts_with(home.join(".local/lib/broscribe")));
        let plan = layout_in(&cli, &data, &home).unwrap();
        let root = plan.installation.unwrap();
        assert_eq!(plan.desktop_paths.len(), 6);
        assert_eq!(
            home.join(".local/bin/broscribe").canonicalize().unwrap(),
            cli.canonicalize().unwrap()
        );
        let template = root.join("share").join(DESKTOP_ENTRY);
        let unexpected = root.join("keep.txt");
        let refused = unexpected.exists() || !template.exists();
        if refused {
            let error = validate_installation(&root, false).unwrap_err();
            assert_eq!(
                error.code(),
                broscribe_platform::release_layout::RELEASE_TREE_UNSAFE
            );
            let missing_or_extra = if unexpected.exists() {
                "keep.txt"
            } else {
                DESKTOP_ENTRY
            };
            assert!(error.to_string().contains(missing_or_extra));
        }
        for entry in plan.desktop_paths {
            assert!(remove_desktop_entry_in(&entry, &root, &data, &home).unwrap());
        }
        if refused {
            assert!(root.join("broscribe").is_file());
            return;
        }
        let check = ReleaseRemovalCheck {
            validate: &|tree| validate_installation(tree, false),
            windows_inventory: None,
            windows_update_cleanup: &[],
            windows_before_launch: &|_| unreachable!(),
        };
        remove_installation(&cli, &root, &check).unwrap();
    }

    #[test]
    fn linux_installer_uninstalls_and_reinstalls_across_a_symlinked_home() {
        let tmp = tempfile::tempdir().unwrap();
        let actual = tmp.path().join("actual-home");
        let home = tmp.path().join(r#"home with "$cash`and\%f"#);
        std::fs::create_dir(&actual).unwrap();
        symlink(actual, &home).unwrap();
        let data = home.join("xdg-data");
        let source = tmp.path().join("package");
        let tools = tmp.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::create_dir_all(source.join("share/applications")).unwrap();
        let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
        for name in ["install.sh", "uninstall.sh"] {
            std::fs::copy(scripts.join(name), source.join(name)).unwrap();
        }
        for name in [
            "broscribe-app",
            "broscribed",
            "broscribe-worker",
            "broscribe-update-runner",
            "LICENSE",
            "NOTICE",
            "THIRD_PARTY_NOTICES.txt",
            "third-party-inventory.json",
            "build-info.json",
        ] {
            std::fs::write(source.join(name), b"synthetic fixture").unwrap();
        }
        std::fs::write(
            source
                .join("share/applications")
                .join(format!("{APP_ID}.desktop")),
            DESKTOP_TEMPLATE,
        )
        .unwrap();
        for size in [22, 32, 64, 128] {
            let dir = source.join(format!("share/icons/hicolor/{size}x{size}/apps"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{APP_ID}.png")), b"fixture icon").unwrap();
        }
        let cli = source.join("broscribe");
        std::fs::write(&cli, format!(
            "#!/bin/sh\nset -eu\n[ \"$*\" = 'uninstall --yes' ]\nexport {HELPER}=\"$0\"\nexec \"$BROSCRIBE_TEST_BINARY\" --ignored --exact uninstall::tests::installer_uninstall_helper\n"
        )).unwrap();
        std::fs::set_permissions(cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let id = tools.join("id");
        std::fs::write(&id, b"#!/bin/sh\necho 1234\n").unwrap();
        std::fs::set_permissions(id, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |script: &str, args: &[&str]| {
            let output = Command::new("/bin/sh")
                .arg(source.join(script))
                .args(args)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", &data)
                .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
                .env("BROSCRIBE_TEST_BINARY", std::env::current_exe().unwrap())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let prefix = home.join(".local/lib/broscribe");
        let desktop = data.join(DESKTOP_ENTRY);
        for mode in [
            "marked",
            "symlink",
            "unmarked-refusal",
            "missing-template",
            "reinstalled",
        ] {
            run("install.sh", &[]);
            match mode {
                "symlink" => {
                    std::fs::remove_file(&desktop).unwrap();
                    symlink(prefix.join("share").join(DESKTOP_ENTRY), &desktop).unwrap();
                }
                "unmarked-refusal" => {
                    let entry = std::fs::read_to_string(&desktop).unwrap();
                    std::fs::write(&desktop, entry.replace(&format!("{DESKTOP_MARKER}\n"), ""))
                        .unwrap();
                    std::fs::write(prefix.join("keep.txt"), "synthetic user file").unwrap();
                    // A removed icon leaves an owned but dangling launcher link.
                    std::fs::remove_file(
                        prefix.join(format!("share/icons/hicolor/22x22/apps/{APP_ID}.png")),
                    )
                    .unwrap();
                }
                "missing-template" => {
                    std::fs::remove_file(&desktop).unwrap();
                    let template = prefix.join("share").join(DESKTOP_ENTRY);
                    symlink(&template, &desktop).unwrap();
                    std::fs::remove_file(template).unwrap();
                }
                _ => {}
            }
            run("uninstall.sh", &["--yes"]);
            for (path, _) in launcher_paths(&data, &home) {
                assert_eq!(
                    std::fs::symlink_metadata(&path).unwrap_err().kind(),
                    ErrorKind::NotFound,
                    "stale launcher: {}",
                    path.display()
                );
            }
            if mode == "unmarked-refusal" {
                let recovered = tmp.path().join("recovered-user-file");
                std::fs::rename(prefix.join("keep.txt"), &recovered).unwrap();
                std::fs::remove_dir_all(&prefix).unwrap();
                assert_eq!(
                    std::fs::read_to_string(recovered).unwrap(),
                    "synthetic user file"
                );
            }
            if mode == "missing-template" {
                assert!(prefix.join("broscribe").is_file());
                std::fs::remove_dir_all(&prefix).unwrap();
            }
            assert!(!prefix.exists());
        }
    }

    #[test]
    fn a_personal_desktop_entry_with_the_same_exec_is_not_claimed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("installation");
        let desktop = temp.path().join(DESKTOP_ENTRY);
        std::fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        let launcher = format!(
            "[Desktop Entry]\nName=Personal menu\nExec=\"{}\"\n",
            root.join("broscribe-app").display()
        );
        std::fs::write(&desktop, launcher).unwrap();
        assert_eq!(
            desktop_owner(
                &desktop,
                &root,
                &Path::new("share").join(DESKTOP_ENTRY),
                &desktop
            )
            .unwrap(),
            Some(false)
        );
    }

    #[test]
    fn launchers_pointing_to_other_leaves_under_the_release_are_kept() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let data = home.join("data");
        let root = home.join("release");
        std::fs::create_dir_all(&root).unwrap();
        let cli = root.join("broscribe");
        std::fs::write(&cli, b"synthetic executable").unwrap();
        std::fs::write(root.join("build-info.json"), b"{}").unwrap();
        for (launcher, target) in launcher_paths(&data, &home) {
            let personal = root.join("personal");
            std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
            std::fs::create_dir_all(root.join(&target).parent().unwrap()).unwrap();
            std::fs::write(root.join(&target), b"synthetic release file").unwrap();
            // Even a second link that ultimately resolves to the generated target is foreign.
            symlink(root.join(target), &personal).unwrap();
            symlink(&personal, &launcher).unwrap();
            assert!(
                layout_in(&cli, &data, &home)
                    .unwrap()
                    .desktop_paths
                    .is_empty()
            );
            assert!(remove_desktop_entry_in(&launcher, &root, &data, &home).is_err());
            assert_eq!(std::fs::read_link(&launcher).unwrap(), personal);
            std::fs::remove_file(personal).unwrap();
            std::fs::remove_file(launcher).unwrap();
        }
    }

    #[test]
    fn launcher_exec_parser_rejects_arguments_field_codes_and_duplicates() {
        let entry = |line| format!("[Desktop Entry]\n{line}\n");
        assert_eq!(
            launcher_executable(&entry(r#"Exec="/app/%%f/broscribe-app""#)),
            Some(PathBuf::from("/app/%f/broscribe-app"))
        );
        for line in [
            r#"Exec="/app/broscribe-app" %f"#,
            r#"Exec="/app/%f/broscribe-app""#,
            "Exec=\"/app/broscribe-app\"\nExec=\"/other/app\"",
        ] {
            assert!(launcher_executable(&entry(line)).is_none(), "{line}");
        }
    }
}

#[cfg(test)]
#[test]
fn runtime_cleanup_removes_owned_lock_and_tray_files_and_keeps_links() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("xdg");
    broscribe_platform_unix::ensure_private_dir(&runtime).unwrap();
    crate::status::install_symbolic_icons_in(Some(runtime.clone())).unwrap();
    let root = runtime.join("broscribe");
    let icons = root.join("icons");
    let lock_name = std::ffi::OsStr::new("broscribed-1234abcdef123456.lock");
    let endpoint = root.join(lock_name);
    std::fs::write(&endpoint, b"42").unwrap();
    let link = icons.join("broscribe-idle-symbolic.svg");
    let outside = temp.path().join("outside.svg");
    std::fs::write(&outside, b"synthetic outside file").unwrap();
    std::fs::remove_file(&link).unwrap();
    symlink(&outside, &link).unwrap();
    let cleanup =
        remove_runtime_files_in(lock_name, std::slice::from_ref(&root), Some(&root)).unwrap();
    assert!(cleanup.changed && cleanup.kept == std::slice::from_ref(&link));
    assert!(!endpoint.exists() && !icons.join("broscribe-recording-symbolic.svg").exists());
    assert_eq!(std::fs::read(&outside).unwrap(), b"synthetic outside file");
    std::fs::remove_file(link).unwrap();
    remove_runtime_files_in(lock_name, std::slice::from_ref(&root), Some(&root)).unwrap();
    assert!(!root.exists());
}

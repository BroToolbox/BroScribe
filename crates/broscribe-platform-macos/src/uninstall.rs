use std::path::{Path, PathBuf};

use broscribe_platform::uninstall::ReleaseRemovalCheck;
use broscribe_platform::uninstall::{Layout, owns_namespace};
use broscribe_platform::{Error, Result, SecretStore};
use objc2_foundation::{NSBundle, NSFileManager, NSString, NSURL};
use security_framework::item::{ItemClass, ItemSearchOptions, Limit};

pub const SECRET_BASE: &str = crate::keychain::KEYCHAIN_SERVICE;

pub fn layout(executable: &Path) -> Result<Layout> {
    let installation = executable.canonicalize().ok().and_then(|exe| {
        exe.ancestors()
            .find(|root| {
                root.extension().is_some_and(|ext| ext == "app")
                    && exe.parent() == Some(root.join("Contents/MacOS").as_path())
                    && root
                        .to_str()
                        .and_then(|path| {
                            NSBundle::bundleWithPath(&NSString::from_str(path))
                                .and_then(|bundle| bundle.bundleIdentifier())
                        })
                        .is_some_and(|id| id.to_string() == "org.broscribe.BroScribe")
            })
            .map(Path::to_path_buf)
    });
    Ok(Layout {
        installation,
        manual_steps: vec![
            "Review BroScribe’s permissions in System Settings › Privacy & Security.",
            "Review BroScribe’s entry in System Settings › General › Login Items & Extensions.",
        ],
        ..Layout::default()
    })
}

pub fn validate_installation(root: &Path, _: bool) -> Result<()> {
    crate::update::validate_bundle_tree(root, None)
}

pub fn remove_installation(
    executable: &Path,
    root: &Path,
    check: &ReleaseRemovalCheck<'_>,
) -> Result<Option<PathBuf>> {
    if layout(executable)?.installation.as_deref() != Some(root) {
        return Err(Error::InvalidInput(
            "BroScribe could not verify the app bundle".into(),
        ));
    }
    (check.validate)(root)?;
    let path = root
        .to_str()
        .ok_or_else(|| Error::InvalidInput("the app path is not UTF-8".into()))?;
    let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true);
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|error| {
            Error::Internal(format!(
                "moving BroScribe to the Trash: {}",
                error.localizedDescription()
            ))
        })?;
    Ok(None)
}

pub fn runtime_paths(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let location = crate::socket_location(data_dir)?;
    let app = location
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .replacen("broscribed", "app", 1);
    Ok(vec![location.path, location.dir.join(app)])
}

pub fn registration_lock_paths() -> Result<Vec<PathBuf>> {
    Ok(vec![
        broscribe_platform_unix::dirs::darwin_user_dir()?
            .join("org.broscribe.login-registration.lock"),
    ])
}

pub fn remove_credentials(services: &[String], all: bool) -> Result<usize> {
    let mut entries = std::collections::BTreeSet::new();
    let filters: Vec<_> = if all {
        vec![None]
    } else {
        services.iter().map(|s| Some(s.as_str())).collect()
    };
    for service in filters {
        let mut query = ItemSearchOptions::new();
        query
            .class(ItemClass::generic_password())
            .load_attributes(true)
            .limit(Limit::All);
        if let Some(service) = service {
            query.service(service);
        }
        let found = match query.search() {
            Ok(items) => items,
            Err(error) if error.code() == -25300 => continue,
            Err(error) => {
                return Err(Error::Internal(format!(
                    "listing BroScribe Keychain items failed (OSStatus {})",
                    error.code()
                )));
            }
        };
        for item in found {
            let Some(attributes) = item.simplify_dict() else {
                continue;
            };
            if let (Some(service), Some(key)) = (attributes.get("svce"), attributes.get("acct"))
                && (services.contains(service) || all && owns_namespace(SECRET_BASE, service))
            {
                entries.insert((service.clone(), key.clone()));
            }
        }
    }
    let mut count = 0;
    let mut failed = 0;
    for (service, key) in entries {
        if crate::keychain::MacKeychain::with_service(&service)
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
            "removed {count} Keychain items; {failed} failed"
        )));
    }
    Ok(count)
}

//! A data folder's identity: one daemon endpoint and one secret namespace
//! per profile (`BROSCRIBE_DATA_DIR` / `--data-dir`).

use std::path::{Component, Path, PathBuf};

/// The canonical path, so symlink aliases, `.`/`..` and trailing separators
/// name one folder. A folder that does not exist yet (daemon first start, a
/// client racing it) resolves its nearest existing ancestor and appends the
/// rest, so the result is the same before and after the folder is created.
pub fn stable_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    let base = loop {
        if let Ok(canonical) = existing.canonicalize() {
            break canonical;
        }
        match (existing.parent(), existing.components().next_back()) {
            (Some(parent), Some(last)) => {
                missing.push(last);
                existing = parent;
            }
            _ => return absolute,
        }
    };
    missing
        .into_iter()
        .rev()
        .fold(base, |mut resolved, component| {
            match component {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => resolved.push(name),
                _ => {}
            }
            resolved
        })
}

/// Equality for paths recorded by different platform entry points.
pub fn same_path(a: &Path, b: &Path) -> bool {
    let plain = crate::plain_verbatim;
    plain(a) == plain(b) || stable_path(a) == stable_path(b)
}

/// Whether either folder contains the other, including equality and aliases.
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    let a = stable_path(a);
    let b = stable_path(b);
    a.starts_with(&b) || b.starts_with(&a)
}

/// The secret-store service of a data folder: `base` for the native default
/// folder (`identity == default_identity`), so its entries keep their names;
/// `<base>.profile-<16 hex>` for any other, so profiles never read, replace
/// or delete each other's secrets. The suffix hashes the platform's identity
/// bytes and reveals no path.
pub fn secret_namespace(base: &str, identity: &[u8], default_identity: &[u8]) -> String {
    if identity == default_identity {
        base.to_owned()
    } else {
        format!("{base}.profile-{}", profile_key(identity))
    }
}

/// Stable filename-safe suffix for a platform's profile identity bytes.
pub fn profile_key(identity: &[u8]) -> String {
    broscribe_core::schema::sha256_hex(identity)[..16].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_windows_paths_accept_both_drive_spellings() {
        for path in [
            r"C:\Users\Synthetic\Programs\BroScribe",
            r"C:\Users\Synthetic\Programs\.BroScribe.update",
        ] {
            let verbatim = format!(r"\\?\{path}");
            assert!(same_path(Path::new(path), Path::new(&verbatim)));
        }
    }

    const BASE: &str = "org.broscribe.BroScribe.test";

    #[test]
    fn the_default_folder_keeps_the_base_namespace() {
        assert_eq!(secret_namespace(BASE, b"/data", b"/data"), BASE);
    }

    #[test]
    fn other_folders_get_a_stable_distinct_profile_namespace() {
        let one = secret_namespace(BASE, b"/profiles/one", b"/data");
        let two = secret_namespace(BASE, b"/profiles/two", b"/data");
        let suffix = one
            .strip_prefix("org.broscribe.BroScribe.test.profile-")
            .unwrap();
        assert_eq!(suffix.len(), 16);
        assert!(
            suffix
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert!(!one.contains("profiles"));
        assert_ne!(one, two);
        assert_eq!(
            one,
            secret_namespace(BASE, b"/profiles/one", b"/other-default")
        );
        assert_eq!(
            one,
            format!(
                "{BASE}.profile-{}",
                &broscribe_core::schema::sha256_hex(b"/profiles/one")[..16]
            )
        );
    }

    #[test]
    fn spellings_of_one_folder_share_a_stable_path() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let canonical = data.canonicalize().unwrap();
        assert_eq!(stable_path(&data), canonical);
        assert_eq!(stable_path(&tmp.path().join("data/")), canonical);
        assert_eq!(stable_path(&data.join(".")), canonical);
        assert_eq!(stable_path(&data.join("../data")), canonical);
        assert_ne!(stable_path(&tmp.path().join("other")), canonical);
    }

    #[test]
    fn a_missing_folder_resolves_the_same_before_and_after_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("not-yet").join("data");
        let before = stable_path(&data);
        assert_eq!(before, stable_path(&tmp.path().join("not-yet/./x/../data")));
        std::fs::create_dir_all(&data).unwrap();
        assert_eq!(before, stable_path(&data));
        assert_eq!(before, data.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_alias_names_the_real_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert_eq!(stable_path(&alias), stable_path(&real));
        // Not created yet under the alias: still the real folder's path.
        let later = alias.join("not-yet").join("data");
        assert_eq!(stable_path(&later), stable_path(&real.join("not-yet/data")));
        std::fs::create_dir_all(&later).unwrap();
        assert_eq!(
            stable_path(&later),
            real.canonicalize().unwrap().join("not-yet/data")
        );
    }
}

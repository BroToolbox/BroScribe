//! Application folders, with the `BROSCRIBE_DATA_DIR` / `BROSCRIBE_LIBRARY`
//! overrides applied here (never in platform implementations).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result};

/// The original directory for relative config paths when the desktop app
/// starts a Windows maintenance command outside its installation.
pub const ENV_MAINTENANCE_CONFIG_BASE: &str = "BROSCRIBE_MAINTENANCE_CONFIG_BASE";

pub const ENV_DATA_DIR: &str = "BROSCRIBE_DATA_DIR";
pub const ENV_LIBRARY: &str = "BROSCRIBE_LIBRARY";

/// Namespace and session ownership of helper-produced setup samples.
pub const HELPER_SAMPLE_PREFIX: &str = "sample-";
const HELPER_SAMPLE_KEY_LEN: usize = 64;

/// Bounded, filename-safe ownership key for every valid request ID.
pub fn helper_sample_session_key(session: &str) -> String {
    broscribe_core::schema::sha256_hex(session.as_bytes())
}

/// The complete ownership suffix in a helper sample folder, never a legacy name.
pub fn helper_sample_owner_key(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(HELPER_SAMPLE_PREFIX)?;
    let split = rest.len().checked_sub(HELPER_SAMPLE_KEY_LEN)?;
    if split == 0 {
        return None;
    }
    let key = rest.get(split..)?;
    key.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        .then_some(key)
}

pub trait AppPaths: Send + Sync {
    /// Settings, models (`~/Library/Application Support/BroScribe` on macOS).
    fn data_dir(&self) -> PathBuf;
    /// Source to name when the native default data directory is unusable.
    fn default_data_dir_source(&self) -> &'static str {
        "the platform's data directory settings"
    }
    fn cache_dir(&self) -> PathBuf;
    /// Library used when neither flag, environment nor config chooses one.
    fn default_library_dir(&self) -> PathBuf;
    /// The current user's platform Documents folder.
    fn documents_dir(&self) -> PathBuf;
    /// Native default profile and library roots to preserve in a shell retry.
    fn uninstall_retry_env(&self) -> Vec<(&'static str, &Path)> {
        Vec::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// An explicit data override must reach children without moving native-default caches.
    pub data_dir_overridden: bool,
    /// The original environment selector, before resolving it against the current folder.
    pub data_dir_from_env: Option<PathBuf>,
    pub library_from_env: Option<PathBuf>,
    pub default_library_dir: PathBuf,
}

impl ResolvedPaths {
    /// Library folder by precedence: `--library` flag, `BROSCRIBE_LIBRARY`,
    /// config `library`, platform default.
    pub fn library(&self, flag: Option<&Path>, config: Option<&Path>) -> PathBuf {
        flag.or(self.library_from_env.as_deref())
            .or(config)
            .unwrap_or(self.default_library_dir.as_path())
            .to_path_buf()
    }
}

/// Platform folders with environment overrides applied.
pub fn resolve_paths(paths: &dyn AppPaths) -> Result<ResolvedPaths> {
    resolve_paths_from(paths, |k| std::env::var_os(k))
}

/// [`resolve_paths`] with an explicit environment. Explicit data folders are
/// absolute before child cwd can differ; their cache remains `<data_dir>/cache`.
pub fn resolve_paths_from(
    paths: &dyn AppPaths,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<ResolvedPaths> {
    let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let data_dir_from_env = var(ENV_DATA_DIR);
    let (data_dir, cache_dir) = match data_dir_from_env.as_deref() {
        Some(d) => {
            let data_dir = if d.is_absolute() {
                d.to_path_buf()
            } else {
                std::path::absolute(d)
                    .map_err(|e| Error::io("resolving the data-directory override", e))?
            };
            let cache_dir = data_dir.join("cache");
            (data_dir, cache_dir)
        }
        None => (paths.data_dir(), paths.cache_dir()),
    };
    Ok(ResolvedPaths {
        data_dir,
        cache_dir,
        data_dir_overridden: data_dir_from_env.is_some(),
        data_dir_from_env,
        library_from_env: var(ENV_LIBRARY),
        default_library_dir: paths.default_library_dir(),
    })
}

/// File dialogs and file-manager integration run on the owning UI loop.
pub type SavePath = std::pin::Pin<
    Box<dyn std::future::Future<Output = std::result::Result<Option<PathBuf>, String>>>,
>;

pub trait FileActions<Ui> {
    fn choose_save_path(&self, ui: &Ui, directory: &Path, name: &str) -> SavePath;
    fn reveal(&self, ui: &Ui, path: &Path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::FakePaths;

    #[test]
    fn env_overrides_and_precedence() {
        let root = tempfile::tempdir().unwrap();
        let p = FakePaths::new(root.path());
        let config_library = root.path().join("configured-library");
        let flag_library = root.path().join("flag-library");
        let env_data = root.path().join("override-data");
        let env_library = root.path().join("override-library");
        let r = resolve_paths_from(&p, |_| None).unwrap();
        assert_eq!(r.data_dir, root.path().join("data"));
        assert_eq!(r.cache_dir, root.path().join("cache"));
        assert_eq!(r.library(None, None), root.path().join("library"));
        assert_eq!(r.library(None, Some(&config_library)), config_library);

        let r = resolve_paths_from(&p, |k| match k {
            ENV_DATA_DIR => Some(env_data.clone().into_os_string()),
            ENV_LIBRARY => Some(env_library.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(r.data_dir, env_data);
        assert_eq!(r.cache_dir, env_data.join("cache"));
        assert_eq!(r.library(None, Some(&config_library)), env_library);
        assert_eq!(r.library(Some(&flag_library), None), flag_library);
    }
}

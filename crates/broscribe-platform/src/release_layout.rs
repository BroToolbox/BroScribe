//! Installed archive layouts shared by the updater and uninstaller.
//! New releases append entries to release_layouts.json; historical entries stay.

use std::collections::BTreeSet;
use std::path::Path;

use crate::{Error, Result};

/// The installed tree cannot be moved or removed as a BroScribe release.
pub const RELEASE_TREE_UNSAFE: &str = "update_installation_contents";

pub const WINDOWS_FFMPEG: &[&str] = &["ffmpeg.exe", "ffplay.exe", "ffprobe.exe"];

#[derive(Clone, Copy)]
pub enum ReleaseLayout {
    Macos,
    Linux,
    Windows,
}

impl ReleaseLayout {
    pub fn for_target(target: &str) -> Option<Self> {
        match target {
            "aarch64-apple-darwin" => Some(Self::Macos),
            "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu" => Some(Self::Linux),
            "x86_64-pc-windows-msvc" => Some(Self::Windows),
            _ => None,
        }
    }

    pub fn archive_root(self) -> &'static str {
        match self {
            Self::Macos => "BroScribe.app",
            Self::Linux | Self::Windows => "BroScribe",
        }
    }

    pub fn validate_partial_archive(self, root: &Path) -> Result<()> {
        let mut refusal = None;
        for layout in self.history() {
            let files = layout.iter().map(|name| self.key(name)).collect();
            match self.validate_entries(root, files, BTreeSet::new()) {
                Ok(()) => return Ok(()),
                Err(error) => refusal = Some(error),
            }
        }
        Err(refusal.expect("every platform has a release layout"))
    }

    fn history(self) -> &'static [Vec<String>] {
        #[derive(serde::Deserialize)]
        struct History {
            macos: Vec<Vec<String>>,
            linux: Vec<Vec<String>>,
            windows: Vec<Vec<String>>,
        }
        static HISTORY: std::sync::LazyLock<History> = std::sync::LazyLock::new(|| {
            serde_json::from_str(include_str!("release_layouts.json"))
                .expect("compiled release layouts are valid")
        });
        match self {
            Self::Macos => &HISTORY.macos,
            Self::Linux => &HISTORY.linux,
            Self::Windows => &HISTORY.windows,
        }
    }

    pub fn required_files(self) -> Vec<String> {
        self.history()
            .last()
            .expect("at least one release layout")
            .clone()
    }

    /// All names ever shipped for this platform, plus the Windows user tools.
    pub fn allowed_files(self) -> BTreeSet<String> {
        self.history()
            .iter()
            .flatten()
            .map(|name| self.key(name))
            .chain(
                match self {
                    Self::Windows => WINDOWS_FFMPEG,
                    Self::Linux | Self::Macos => &[],
                }
                .iter()
                .map(|name| self.key(name)),
            )
            .collect()
    }

    /// Paths present in every historical layout for this platform.
    pub fn common_files(self) -> BTreeSet<String> {
        self.history()[0]
            .iter()
            .map(|name| self.key(name))
            .filter(|name| {
                self.history()
                    .iter()
                    .all(|layout| layout.iter().any(|file| self.key(file) == *name))
            })
            .collect()
    }

    fn key(self, path: &str) -> String {
        match self {
            Self::Linux | Self::Macos => path.to_owned(),
            Self::Windows => path.to_ascii_lowercase(),
        }
    }

    /// A cleanup can resume after some release files were already deleted.
    /// A move, or a first removal of the installation, needs the full layout.
    pub fn validate(self, root: &Path, allow_partial: bool) -> Result<()> {
        let required = if allow_partial {
            BTreeSet::new()
        } else {
            self.common_files()
        };
        self.validate_entries(root, self.allowed_files(), required)
    }

    fn validate_entries(
        &self,
        root: &Path,
        files: BTreeSet<String>,
        required: BTreeSet<String>,
    ) -> Result<()> {
        let dirs: BTreeSet<_> = files
            .iter()
            .flat_map(|name| name.match_indices('/').map(|(end, _)| &name[..end]))
            .collect();
        let mut pending = vec![(root.to_path_buf(), String::new())];
        let mut seen = BTreeSet::new();
        let mut unexpected = Vec::new();
        while let Some((folder, prefix)) = pending.pop() {
            let meta = std::fs::symlink_metadata(&folder).map_err(|e| Error::io_at(&folder, e))?;
            if !meta.is_dir() || is_reparse(&meta) {
                unexpected.push(folder);
                break;
            }
            for entry in std::fs::read_dir(&folder).map_err(|e| Error::io_at(&folder, e))? {
                let entry = entry.map_err(|e| Error::io_at(&folder, e))?;
                let path = entry.path();
                let name = entry.file_name();
                let key = name
                    .to_str()
                    .map(|name| self.key(&format!("{prefix}{name}")));
                let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
                if let Some(key) = key.filter(|key| !is_reparse(&meta) && seen.insert(key.clone()))
                {
                    if meta.is_file() && files.contains(&key) {
                        continue;
                    }
                    if meta.is_dir() && dirs.contains(key.as_str()) {
                        pending.push((path, format!("{key}/")));
                        continue;
                    }
                }
                unexpected.push(path);
                if unexpected.len() == 4 {
                    break;
                }
            }
            if unexpected.len() == 4 {
                break;
            }
        }
        if !unexpected.is_empty() {
            let paths = unexpected
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            return Err(layout_error(format!(
                "Unexpected installation entries: {}. Move them out of {} before retrying.",
                paths.join(", "),
                root.display()
            )));
        }
        if !required.is_subset(&seen) {
            let missing = required
                .difference(&seen)
                .take(4)
                .cloned()
                .collect::<Vec<_>>();
            return Err(layout_error(format!(
                "The installation at {} is missing release files: {}.",
                root.display(),
                missing.join(", ")
            )));
        }
        Ok(())
    }
}

fn layout_error(message: String) -> Error {
    Error::Service {
        code: RELEASE_TREE_UNSAFE,
        message,
    }
}

pub fn is_reparse(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}

#[cfg(test)]
pub fn write_release_files(root: &Path, files: impl IntoIterator<Item = String>) {
    for name in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"synthetic release resource").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_layout_refuses_extra_entries_and_permits_only_named_optional_files() {
        for layout in [ReleaseLayout::Linux, ReleaseLayout::Windows] {
            let tmp = tempfile::tempdir().unwrap();
            write_release_files(tmp.path(), layout.required_files());
            layout.validate(tmp.path(), false).unwrap();
            let extra = tmp.path().join(match layout {
                ReleaseLayout::Linux => "share/icons/hicolor/64x64/apps/meeting-library",
                ReleaseLayout::Windows => "meeting-library",
                ReleaseLayout::Macos => unreachable!(),
            });
            std::fs::create_dir(&extra).unwrap();
            std::fs::write(extra.join("keep.md"), "synthetic meeting").unwrap();
            for partial in [true, false] {
                let error = layout
                    .validate(tmp.path(), partial)
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("meeting-library") && error.contains("Move them out"),
                    "{error}"
                );
            }
            assert!(extra.join("keep.md").is_file());
            std::fs::remove_dir_all(extra).unwrap();
            let optional = match layout {
                ReleaseLayout::Linux => "uninstall.sh",
                ReleaseLayout::Windows => "FFmpeg.EXE",
                ReleaseLayout::Macos => unreachable!(),
            };
            std::fs::write(tmp.path().join(optional), "synthetic optional tool").unwrap();
            layout.validate(tmp.path(), false).unwrap();
            std::fs::remove_file(tmp.path().join("LICENSE")).unwrap();
            assert!(layout.validate(tmp.path(), false).is_err());
            layout.validate(tmp.path(), true).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_allowed_name_cannot_be_a_symbolic_link() {
        let tmp = tempfile::tempdir().unwrap();
        write_release_files(tmp.path(), ReleaseLayout::Windows.required_files());
        std::os::unix::fs::symlink(tmp.path(), tmp.path().join("FFPLAY.exe")).unwrap();
        assert!(ReleaseLayout::Windows.validate(tmp.path(), false).is_err());
    }
}

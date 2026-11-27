//! Strict extraction of a release archive into a fresh staging folder.
//!
//! Every entry lies under the artifact's single root folder; names are
//! ASCII, relative, without `.`/`..`, backslashes, drive or reserved
//! characters, AppleDouble (`._*`, `__MACOSX`) files, case-insensitive
//! duplicates or a file or link as an ancestor. Entries are regular files,
//! folders or (where the platform has them) relative links whose target
//! stays inside the root: leading `..` steps that never climb above the
//! root, then plain names. Links are created only after every file is
//! written, so no write can follow one. Encrypted entries and compression
//! other than stored/deflate are refused. Entry count and the sum of file
//! and link sizes must equal the signed values exactly; every entry yields
//! exactly its declared size (and its CRC, checked by the reader).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result};
use broscribe_platform::update::UpdatePlatform;
use serde::{Deserialize, Serialize};
use zip::CompressionMethod;

use crate::{ArchiveFormat, Artifact, Cancel, Limits, codes, fail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dir,
    File { executable: bool },
    Link,
}

/// Entry types in the signed download, including implicit directories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryType {
    Directory,
    File,
    Link,
}

pub(crate) type Entries = BTreeMap<String, EntryType>;

impl From<Kind> for EntryType {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Dir => Self::Directory,
            Kind::File { .. } => Self::File,
            Kind::Link => Self::Link,
        }
    }
}

struct Plan {
    parts: Vec<String>,
    kind: Kind,
    size: u64,
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

fn unsafe_entry(why: impl Into<String>) -> Error {
    fail(codes::ARCHIVE_UNSAFE, why)
}

/// Extracts `archive` into `dest` (which must not exist) and returns the
/// payload root, `dest/<artifact.root>`.
pub(crate) fn extract(
    archive: &Path,
    dest: &Path,
    artifact: &Artifact,
    limits: &Limits,
    platform: &dyn UpdatePlatform,
    cancel: &Cancel,
    remember_entries: impl FnOnce(Entries) -> Result<()>,
) -> Result<PathBuf> {
    let file = File::open(archive).map_err(|e| Error::io_at(archive, e))?;
    let before_writing = |entries| {
        cancel.check()?;
        remember_entries(entries)?;
        create_fresh_directory(dest)?;
        create_fresh_directory(&dest.join(&artifact.root))
    };
    let links = match artifact.archive {
        ArchiveFormat::Zip => extract_zip(
            file,
            dest,
            artifact,
            limits,
            platform,
            cancel,
            before_writing,
        )?,
        ArchiveFormat::TarGz => {
            // Tar streams its headers alongside the data. Complete the
            // checked header scan before publishing an extraction directory.
            let layout = walk_tar(&file, artifact, limits, cancel, |_, _, _| Ok(()))?;
            before_writing(layout.entries())?;
            let mut file = file;
            use std::io::Seek as _;
            file.rewind().map_err(|e| Error::io_at(archive, e))?;
            let mut links = Links::new();
            walk_tar(file, artifact, limits, cancel, |entry, plan, target| {
                let path = dest.join(plan.parts.join("/"));
                match plan.kind {
                    Kind::Dir => create_directory(&path),
                    Kind::File { executable } => {
                        write_file(&path, entry, plan.size, executable, platform, cancel)
                    }
                    Kind::Link => {
                        links.push((path, target.expect("validated tar link")));
                        Ok(())
                    }
                }
            })?;
            links
        }
    };
    for (link, target) in links {
        cancel.check()?;
        create_parent(&link)?;
        platform.create_symlink(&link, &target)?;
    }
    Ok(dest.join(&artifact.root))
}

fn create_parent(path: &Path) -> Result<()> {
    let parent = path.parent().expect("entry paths have a parent");
    create_directory(parent)
}

type Links = Vec<(PathBuf, PathBuf)>;

fn extract_zip(
    file: File,
    dest: &Path,
    artifact: &Artifact,
    limits: &Limits,
    platform: &dyn UpdatePlatform,
    cancel: &Cancel,
    before_writing: impl FnOnce(Entries) -> Result<()>,
) -> Result<Links> {
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| unsafe_entry(format!("unreadable archive: {e}")))?;
    let mut layout = Layout::new(artifact, limits);
    layout.check_count(zip.len() as u64)?;
    let mut plans = Vec::with_capacity(zip.len());
    for index in 0..zip.len() {
        cancel.check()?;
        let entry = zip
            .by_index_raw(index)
            .map_err(|e| unsafe_entry(format!("unreadable entry {index}: {e}")))?;
        let (parts, slash) = entry_parts(entry.name_raw(), &artifact.root, limits)?;
        let shown = parts.join("/");
        if entry.encrypted() {
            return Err(unsafe_entry(format!("{shown} is encrypted")));
        }
        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(unsafe_entry(format!(
                "{shown} uses an unsupported compression method"
            )));
        }
        let mode_type = entry.unix_mode().map(|mode| mode & S_IFMT).unwrap_or(0);
        let kind = match (slash, mode_type) {
            (true, 0 | S_IFDIR) => Kind::Dir,
            (false, S_IFLNK) => Kind::Link,
            (false, 0 | S_IFREG) => Kind::File {
                executable: entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0),
            },
            _ => {
                return Err(unsafe_entry(format!(
                    "{shown} is a special file or its type disagrees with its name"
                )));
            }
        };
        let plan = Plan {
            parts,
            kind,
            size: entry.size(),
        };
        layout.record(&plan)?;
        plans.push(plan);
    }
    layout.finish()?;
    before_writing(layout.entries())?;
    let mut links = Links::new();
    for (index, plan) in plans.iter().enumerate() {
        cancel.check()?;
        let path = dest.join(plan.parts.join("/"));
        if plan.kind == Kind::Dir {
            create_directory(&path)?;
            continue;
        }
        let entry = zip
            .by_index(index)
            .map_err(|e| unsafe_entry(format!("unreadable entry: {e}")))?;
        match plan.kind {
            Kind::File { executable } => {
                write_file(&path, entry, plan.size, executable, platform, cancel)?
            }
            Kind::Link => {
                let mut target = Vec::new();
                copy_exact(entry, &mut target, plan.size, cancel)?;
                links.push((path, link_target(&target, &plan.parts)?));
            }
            Kind::Dir => unreachable!(),
        }
    }
    Ok(links)
}

fn walk_tar<'a, R: Read>(
    file: R,
    artifact: &'a Artifact,
    limits: &'a Limits,
    cancel: &Cancel,
    mut entry_action: impl FnMut(
        tar::Entry<'_, flate2::bufread::GzDecoder<BufReader<R>>>,
        &Plan,
        Option<PathBuf>,
    ) -> Result<()>,
) -> Result<Layout<'a>> {
    let gzip = flate2::bufread::GzDecoder::new(BufReader::new(file));
    let mut tar = tar::Archive::new(gzip);
    let unreadable = |e| unsafe_entry(format!("unreadable tar.gz archive: {e}"));
    let mut layout = Layout::new(artifact, limits);
    // Raw iteration keeps PAX, GNU extension and sparse headers visible.
    for entry in tar.entries().map_err(unreadable)?.raw(true) {
        cancel.check()?;
        let entry = entry.map_err(unreadable)?;
        let (parts, slash) = entry_parts(&entry.path_bytes(), &artifact.root, limits)?;
        let raw_size = entry.size();
        let (kind, target) = match entry.header().entry_type() {
            tar::EntryType::Directory => (Kind::Dir, None),
            tar::EntryType::Regular if !slash => (
                Kind::File {
                    executable: entry.header().mode().map_err(unreadable)? & 0o111 != 0,
                },
                None,
            ),
            tar::EntryType::Symlink if !slash && raw_size == 0 => {
                (Kind::Link, entry.link_name_bytes())
            }
            _ => return Err(unsafe_entry("unsupported tar entry type, path or size")),
        };
        let size = match kind {
            Kind::Link => target.as_ref().map_or(0, |target| target.len() as u64),
            _ => raw_size,
        };
        let plan = Plan { parts, kind, size };
        layout.record(&plan)?;
        let target = if kind == Kind::Link {
            let target = target.ok_or_else(|| unsafe_entry("tar link is missing a target"))?;
            Some(link_target(&target, &plan.parts)?)
        } else {
            None
        };
        entry_action(entry, &plan, target)?;
    }
    layout.finish()?;
    cancel.check()?;
    // The tar reader consumes the first end block. Check the second and
    // bounded record padding, and drive gzip to EOF to verify its trailer.
    let mut gzip = tar.into_inner();
    const MAX_PADDING: u64 = 10_240;
    let mut tail = Vec::new();
    (&mut gzip)
        .take(MAX_PADDING + 1)
        .read_to_end(&mut tail)
        .map_err(unreadable)?;
    if tail.len() < 512
        || tail.len() as u64 > MAX_PADDING
        || !tail.len().is_multiple_of(512)
        || tail.iter().any(|&byte| byte != 0)
        || !gzip.into_inner().fill_buf().map_err(unreadable)?.is_empty()
    {
        return Err(unsafe_entry("invalid or oversized tar end padding"));
    }
    Ok(layout)
}

fn create_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(unsafe_entry(format!("{} is not a folder", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_parent(path)?;
            create_fresh_directory(path)
        }
        Err(e) => Err(Error::io_at(path, e)),
    }
}

fn create_fresh_directory(path: &Path) -> Result<()> {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(path).map_err(|e| Error::io_at(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| Error::io_at(path, e))?;
    }
    Ok(())
}

fn write_file(
    path: &Path,
    data: impl Read,
    size: u64,
    executable: bool,
    platform: &dyn UpdatePlatform,
    cancel: &Cancel,
) -> Result<()> {
    create_parent(path)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| Error::io_at(path, e))?;
    copy_exact(data, &mut out, size, cancel)?;
    out.sync_all().map_err(|e| Error::io_at(path, e))?;
    drop(out);
    platform.set_executable(path, executable)
}

struct Layout<'a> {
    artifact: &'a Artifact,
    limits: &'a Limits,
    seen: BTreeMap<String, (String, Kind)>,
    declared: u64,
}

impl<'a> Layout<'a> {
    fn new(artifact: &'a Artifact, limits: &'a Limits) -> Self {
        Self {
            artifact,
            limits,
            seen: BTreeMap::new(),
            declared: 0,
        }
    }

    fn check_count(&self, count: u64) -> Result<()> {
        if count != self.artifact.entries || count > self.limits.max_entries {
            return Err(fail(
                codes::ARCHIVE_TOO_LARGE,
                format!(
                    "the archive has {count} entries; the release declares {}",
                    self.artifact.entries
                ),
            ));
        }
        Ok(())
    }

    fn record(&mut self, plan: &Plan) -> Result<()> {
        let shown = plan.parts.join("/");
        match plan.kind {
            Kind::Dir if plan.size != 0 => {
                return Err(unsafe_entry(format!("folder {shown} carries data")));
            }
            Kind::Link if plan.size == 0 || plan.size > self.limits.max_link_bytes => {
                return Err(unsafe_entry(format!(
                    "link {shown} has a bad target length"
                )));
            }
            kind if kind != Kind::Dir && plan.parts.len() == 1 => {
                return Err(unsafe_entry("the archive root is not a folder"));
            }
            _ => {}
        }
        let name = shown.to_ascii_lowercase();
        if self.seen.contains_key(&name) {
            return Err(unsafe_entry(format!("{shown} appears twice")));
        }
        let count = self.seen.len() as u64 + 1;
        if count > self.artifact.entries || count > self.limits.max_entries {
            return self.check_count(count);
        }
        if plan.kind != Kind::Dir {
            let prefix = format!("{name}/");
            if self
                .seen
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(next, _)| next.starts_with(&prefix))
            {
                return Err(unsafe_entry(format!(
                    "{shown} is a file or link used as a folder"
                )));
            }
        }
        for (end, _) in name.match_indices('/') {
            if self
                .seen
                .get(&name[..end])
                .is_some_and(|(_, kind)| *kind != Kind::Dir)
            {
                return Err(unsafe_entry(format!("{shown} lies under a file or link")));
            }
        }
        self.seen.insert(name, (shown, plan.kind));
        self.declared = self
            .declared
            .checked_add(plan.size)
            .filter(|total| {
                *total <= self.artifact.installed_size && *total <= self.limits.max_installed_bytes
            })
            .ok_or_else(|| {
                fail(
                    codes::ARCHIVE_TOO_LARGE,
                    "the archive declares more bytes than allowed",
                )
            })?;
        Ok(())
    }

    fn entries(self) -> Entries {
        let mut entries = Entries::from([(self.artifact.root.clone(), EntryType::Directory)]);
        for (name, kind) in self.seen.into_values() {
            for (end, _) in name.match_indices('/') {
                entries
                    .entry(name[..end].to_owned())
                    .or_insert(EntryType::Directory);
            }
            entries.insert(name, kind.into());
        }
        entries
    }

    fn finish(&self) -> Result<()> {
        self.check_count(self.seen.len() as u64)?;
        if self.declared != self.artifact.installed_size {
            return Err(fail(
                codes::ARCHIVE_TOO_LARGE,
                format!(
                    "the archive expands to {} bytes; the release declares {}",
                    self.declared, self.artifact.installed_size
                ),
            ));
        }
        Ok(())
    }
}

/// Splits an entry name into checked components; the bool says whether the
/// name ends with `/` (a folder).
fn entry_parts(raw: &[u8], root: &str, limits: &Limits) -> Result<(Vec<String>, bool)> {
    let shown = String::from_utf8_lossy(raw);
    let name = std::str::from_utf8(raw)
        .ok()
        .filter(|name| !name.is_empty() && name.len() <= limits.max_path_bytes && name.is_ascii())
        .ok_or_else(|| {
            unsafe_entry(format!(
                "entry name {shown:?} is empty, too long or not ASCII"
            ))
        })?;
    let (body, slash) = match name.strip_suffix('/') {
        Some(body) => (body, true),
        None => (name, false),
    };
    let parts: Vec<String> = body.split('/').map(str::to_owned).collect();
    if parts.len() > limits.max_depth {
        return Err(unsafe_entry(format!("{name} is nested too deeply")));
    }
    if parts.first().map(String::as_str) != Some(root) {
        return Err(unsafe_entry(format!(
            "{name} is outside the archive root {root}"
        )));
    }
    for part in &parts {
        if !valid_component(part) {
            return Err(unsafe_entry(format!("{name} has an unsafe path component")));
        }
    }
    Ok((parts, slash))
}

fn valid_component(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.starts_with("._")
        && part != "__MACOSX"
        && !part.ends_with('.')
        && !part.ends_with(' ')
        && !part.bytes().any(|b| {
            b.is_ascii_control()
                || matches!(b, b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|')
        })
}

/// A link target that stays inside the root: leading `..` steps no higher
/// than the root folder, then plain names.
fn link_target(raw: &[u8], link: &[String]) -> Result<PathBuf> {
    let shown = link.join("/");
    let target = std::str::from_utf8(raw)
        .ok()
        .filter(|t| t.is_ascii() && !t.starts_with('/'))
        .ok_or_else(|| unsafe_entry(format!("link {shown} has an absolute or non-ASCII target")))?;
    let mut climbing = true;
    let mut ups = 0usize;
    let mut names = 0usize;
    for part in target.split('/') {
        if climbing && part == ".." {
            ups += 1;
            continue;
        }
        climbing = false;
        if !valid_component(part) {
            return Err(unsafe_entry(format!("link {shown} has an unsafe target")));
        }
        names += 1;
    }
    // The link's folder is `link[..len-1]`; `link[0]` is the root itself.
    let folder_depth = link.len() - 2;
    if ups > folder_depth || (names == 0 && ups == 0) {
        return Err(unsafe_entry(format!(
            "link {shown} points outside the payload"
        )));
    }
    Ok(PathBuf::from(target))
}

/// Copies exactly `size` bytes of `reader`; more or fewer is unsafe.
fn copy_exact(reader: impl Read, out: &mut dyn Write, size: u64, cancel: &Cancel) -> Result<u64> {
    let mut reader = reader.take(size + 1);
    let mut buf = vec![0; 1 << 20];
    let mut total = 0u64;
    loop {
        cancel.check()?;
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(unsafe_entry(format!("corrupt entry data: {e}"))),
        };
        total += n as u64;
        if total > size {
            return Err(fail(
                codes::ARCHIVE_TOO_LARGE,
                "an entry expands beyond its declared size",
            ));
        }
        out.write_all(&buf[..n])
            .map_err(|e| Error::io("writing an extracted file", e))?;
    }
    if total != size {
        return Err(unsafe_entry("an entry is shorter than its declared size"));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(path: &str) -> Vec<String> {
        path.split('/').map(str::to_owned).collect()
    }

    #[test]
    fn link_targets_stay_under_the_root() {
        let link = parts("App.app/Contents/Frameworks/X.framework/Headers");
        assert!(link_target(b"Versions/Current/Headers", &link).is_ok());
        assert!(link_target(b"../../MacOS/tool", &link).is_ok());
        assert!(
            link_target(b"../../../x", &link).is_ok(),
            "to the root's child"
        );
        assert!(
            link_target(b"../../../../x", &link).is_err(),
            "above the root"
        );
        assert!(link_target(b"/etc/passwd", &link).is_err());
        assert!(
            link_target(b"a/../../../../..", &link).is_err(),
            "`..` after a name"
        );
        assert!(link_target(b"a/./b", &link).is_err());
        assert!(link_target(b"", &link).is_err());
        assert!(link_target(b"a//b", &link).is_err());
        let top = parts("App.app/link");
        assert!(link_target(b"..", &top).is_err(), "the root's parent");
    }

    #[test]
    fn entry_names_are_plain_and_under_the_root() {
        let limits = Limits::default();
        assert!(entry_parts(b"App.app/Contents/Info.plist", "App.app", &limits).is_ok());
        assert!(
            entry_parts(b"App.app/Contents/", "App.app", &limits)
                .unwrap()
                .1
        );
        for bad in [
            &b"Other.app/x"[..],
            b"App.app/../x",
            b"App.app/./x",
            b"/App.app/x",
            b"App.app//x",
            b"App.app/a\\b",
            b"App.app/C:x",
            b"App.app/._Info.plist",
            b"App.app/__MACOSX/x",
            b"App.app/caf\xc3\xa9",
            b"App.app/trailing.",
        ] {
            assert!(
                entry_parts(bad, "App.app", &limits).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }
    #[test]
    fn tar_stream_rejects_unsafe_entries_and_unbounded_layouts() {
        let archive = |items: &[(&str, tar::EntryType, &[u8])], tail: &[u8]| {
            let mut buffer = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut buffer);
                for &(path, kind, data) in items {
                    let mut header = tar::Header::new_ustar();
                    header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
                    header.set_entry_type(kind);
                    header.set_mode(0o755);
                    if kind == tar::EntryType::Symlink {
                        header.set_link_name_literal(data).unwrap();
                        header.set_size(0);
                    } else {
                        header.set_size(data.len() as u64);
                    }
                    header.set_cksum();
                    builder
                        .append(
                            &header,
                            if kind == tar::EntryType::Symlink {
                                &[][..]
                            } else {
                                data
                            },
                        )
                        .unwrap();
                }
                builder.finish().unwrap();
            }
            let mut compressor =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            compressor.write_all(&buffer).unwrap();
            compressor.write_all(tail).unwrap();
            compressor.finish().unwrap()
        };
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("fixture.tar.gz");
        let dest = tmp.path().join("payload");
        let platform = broscribe_platform::update::FakeUpdatePlatform::new(
            tmp.path().join("install"),
            "aarch64-unknown-linux-gnu",
        );
        let mut artifact = Artifact {
            target: "aarch64-unknown-linux-gnu".into(),
            app_id: "BroScribe".into(),
            url: "https://example.org/fixture.tar.gz".into(),
            size: 1,
            sha256: "0".repeat(64),
            archive: ArchiveFormat::TarGz,
            root: "BroScribe".into(),
            entries: 1,
            installed_size: 1,
        };
        let regular = ("BroScribe/bin/tool", tar::EntryType::Regular, &b"x"[..]);
        let extract_fixture = |items: &[(&str, tar::EntryType, &[u8])],
                               tail,
                               artifact: &Artifact,
                               limits: &Limits| {
            if dest.exists() {
                std::fs::remove_dir_all(&dest).unwrap();
            }
            std::fs::write(&file, archive(items, tail)).unwrap();
            extract(
                &file,
                &dest,
                artifact,
                limits,
                &platform,
                &Cancel::new(),
                |entries| {
                    assert!(!dest.exists(), "entry list must precede extraction");
                    assert_eq!(entries.get("BroScribe"), Some(&EntryType::Directory));
                    if items.iter().any(|(name, _, _)| *name == regular.0) {
                        assert_eq!(entries.get("BroScribe/bin"), Some(&EntryType::Directory));
                        assert_eq!(entries.get(regular.0), Some(&EntryType::File));
                    }
                    Ok(())
                },
            )
        };
        for bad in [
            ("BroScribe/../outside", tar::EntryType::Regular, &b"x"[..]),
            ("BroScribe/fifo", tar::EntryType::Fifo, &[][..]),
            ("BroScribe/link", tar::EntryType::Link, &[][..]),
            (
                "BroScribe/link",
                tar::EntryType::Symlink,
                &b"../outside"[..],
            ),
            ("BroScribe/ext", tar::EntryType::XHeader, &[][..]),
            ("BroScribe/folder", tar::EntryType::Directory, &b"x"[..]),
        ] {
            let declared = Artifact {
                installed_size: bad.2.len().max(1) as u64,
                ..artifact.clone()
            };
            assert_eq!(
                extract_fixture(&[bad], &[], &declared, &Limits::default())
                    .unwrap_err()
                    .code(),
                codes::ARCHIVE_UNSAFE,
                "{bad:?}"
            );
        }
        for pad in [&[1][..], &[0; 10_240][..]] {
            assert_eq!(
                extract_fixture(&[regular], pad, &artifact, &Limits::default())
                    .unwrap_err()
                    .code(),
                codes::ARCHIVE_UNSAFE
            );
        }
        let limits = Limits {
            max_depth: 2,
            ..Limits::default()
        };
        assert_eq!(
            extract_fixture(&[regular], &[], &artifact, &limits)
                .unwrap_err()
                .code(),
            codes::ARCHIVE_UNSAFE
        );
        artifact.installed_size = 2;
        assert_eq!(
            extract_fixture(&[regular], &[], &artifact, &Limits::default())
                .unwrap_err()
                .code(),
            codes::ARCHIVE_TOO_LARGE
        );
        artifact.entries = 2;
        let upper = ("BroScribe/BIN/tool", tar::EntryType::Regular, &b"y"[..]);
        assert_eq!(
            extract_fixture(&[regular, upper], &[], &artifact, &Limits::default())
                .unwrap_err()
                .code(),
            codes::ARCHIVE_UNSAFE
        );
        let parent = ("BroScribe/bin", tar::EntryType::Symlink, &b"elsewhere"[..]);
        artifact.installed_size = 10;
        for entries in [[regular, parent], [parent, regular]] {
            assert_eq!(
                extract_fixture(&entries, &[], &artifact, &Limits::default())
                    .unwrap_err()
                    .code(),
                codes::ARCHIVE_UNSAFE
            );
        }
        #[cfg(unix)]
        {
            let link = ("BroScribe/tool", tar::EntryType::Symlink, &b"bin/tool"[..]);
            artifact.installed_size = 9;
            let root =
                extract_fixture(&[link, regular], &[], &artifact, &Limits::default()).unwrap();
            assert_eq!(std::fs::read(root.join("bin/tool")).unwrap(), b"x");
            assert_eq!(
                std::fs::read_link(root.join("tool")).unwrap(),
                Path::new("bin/tool")
            );
        }
    }
}

#!/usr/bin/env python3
"""Check a BroScribe.app bundle, its distributable ZIP or its disk image without credentials.

  scripts/verify-macos-artifact.py <BroScribe.app | artifact.zip>
      [--provenance FILE] [--require-complete-licences]
      [--forbid TEXT ...] [--codesign] [--require-stapled]
  scripts/verify-macos-artifact.py --dmg <artifact.dmg> --compare-zip <artifact.zip>
      [--provenance FILE] [--require-complete-licences]
      [--forbid TEXT ...] [--codesign] [--require-stapled]
  scripts/verify-macos-artifact.py <symbols-folder | symbols.zip> --symbols
      [--forbid TEXT ...]

Structural checks (any platform): the bundle layout (the capture daemon lives
only in the login-item helper), the shipped LICENSE/NOTICE/third-party
notices and inventory, the icon, Info.plist versions agreeing with
build-info.json and with the helper, and that no executable or resource
contains a builder path or any extra --forbid string. With a ZIP, a sibling
<zip>.sha256 or --provenance must match it.

--codesign and --require-stapled run `codesign --verify` and
`xcrun stapler validate` on an extracted copy (macOS only); they report what
those tools say and never contact a signing service with credentials.

--dmg (macOS: hdiutil) checks the disk image in two stages. Before anything
is attached: its <dmg>.sha256, the --provenance `disk_image` (this image's
name, size and SHA-256, the --compare-zip archive, and the same signer,
notarization and format rules as when it was recorded), an unencrypted ULFO
image with no licence agreement, and with --codesign a timestamped Developer
ID Application signature of team MACOS_TEAM whose authorities and team equal
the provenance's; with --require-stapled its stapled ticket and Gatekeeper
(`spctl -t open`). Any failure there ends the check unmounted. Then it is
attached read-only and without browsing at a private mount point and always
detached again: an APFS volume whose root holds exactly BroScribe.app and an
`Applications` link to /Applications (DMG_HIDDEN_ALLOWED lists the hidden
entries the filesystem itself may add: none for this layout). The mounted app
must equal the app in --compare-zip: the same paths, entry types, link
targets, permission bits and file contents, and it gets the same bundle and
builder-path checks. Links are never followed. That comparison ignores
extended attributes, ACLs and BSD file flags (neither a ZIP entry nor lstat
records them), and nothing else compares them either. Separately, under
--codesign the mounted app's code signature must verify (`codesign --verify
--deep --strict`, same team), which covers the signed code and sealed
resources, not arbitrary metadata; --require-stapled validates its ticket.

Exit status: 0 pass, 1 check failed, 2 usage or unreadable input.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import re
import shutil
import subprocess
import stat
import sys
import tempfile
import time
import zipfile
from pathlib import Path

from desktop_artifacts import build_roots, builder_paths, forbidden_paths, upstream_paths

# One wrapper for every native tool (hdiutil, codesign, xcrun, spctl, ditto),
# so the offline tests can replace them all. Bounded: a stuck tool fails the check.
TOOL_TIMEOUT = 600


def run_tool(command: list[str], *, timeout: int = TOOL_TIMEOUT) -> subprocess.CompletedProcess:
    return subprocess.run(command, capture_output=True, timeout=timeout, check=False)


def tool_output(result: subprocess.CompletedProcess) -> str:
    return b"\n".join(part for part in (result.stdout, result.stderr) if part).decode(errors="replace").strip()

HELPER = "Contents/Library/LoginItems/BroScribe Agent.app"
REQUIRED = (
    "Contents/Info.plist",
    "Contents/MacOS/broscribe-app",
    "Contents/MacOS/broscribe",
    "Contents/MacOS/broscribe-worker",
    "Contents/MacOS/broscribe-update-runner",
    f"{HELPER}/Contents/Info.plist",
    f"{HELPER}/Contents/MacOS/broscribed",
    "Contents/Library/LaunchAgents/org.broscribe.broscribed.plist",
    "Contents/Resources/LICENSE",
    "Contents/Resources/NOTICE",
    "Contents/Resources/THIRD_PARTY_NOTICES.txt",
    "Contents/Resources/third-party-inventory.json",
    "Contents/Resources/build-info.json",
    "Contents/Resources/BroScribe.icns",
)
FORBIDDEN = ("Contents/MacOS/broscribed",)
EXECUTABLES = {f"BroScribe.app/{name}" for name in REQUIRED if "/MacOS/" in name}
MACOS_SHERPA = "osx-arm64"


class Bundle:
    """Read-only view of BroScribe.app, from a folder or inside a ZIP."""

    def __init__(self, path: Path):
        self.path = path
        self.zip = None
        if path.is_dir():
            self.files = {p.relative_to(path).as_posix(): p for p in path.rglob("*") if p.is_file()}
        elif zipfile.is_zipfile(path):
            self.zip = zipfile.ZipFile(path)
            prefix = "BroScribe.app/"
            self.files = {name[len(prefix):]: name for name in self.zip.namelist()
                          if name.startswith(prefix) and not name.endswith("/")}
            if not self.files:
                raise ValueError(f"{path} has no BroScribe.app at its top level")
        else:
            raise ValueError(f"{path} is neither an app folder nor a ZIP")

    def read(self, relative: str) -> bytes:
        source = self.files[relative]
        return self.zip.read(source) if self.zip else source.read_bytes()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def check_bundle(bundle: Bundle, forbid: list[str], require_complete: bool) -> list[str]:
    problems = [f"missing {name}" for name in REQUIRED if name not in bundle.files]
    problems += [f"{name} must not ship: the daemon runs only from the login-item helper"
                 for name in FORBIDDEN if name in bundle.files]
    if problems:
        return problems

    info = plistlib.loads(bundle.read("Contents/Info.plist"))
    helper = plistlib.loads(bundle.read(f"{HELPER}/Contents/Info.plist"))
    build = json.loads(bundle.read("Contents/Resources/build-info.json"))
    inventory = json.loads(bundle.read("Contents/Resources/third-party-inventory.json"))
    version, number = info.get("CFBundleShortVersionString"), info.get("CFBundleVersion")
    if version != build.get("version") or number != str(build.get("build")):
        problems.append(f"Info.plist {version} ({number}) disagrees with build-info.json "
                        f"{build.get('version')} ({build.get('build')})")
    if not (isinstance(number, str) and number.isdigit() and int(number) > 0):
        problems.append(f"CFBundleVersion {number!r} is not a positive build number")
    if (helper.get("CFBundleShortVersionString"), helper.get("CFBundleVersion")) != (version, number):
        problems.append("the helper's version or build differs from the app's")
    if not str(info.get("NSHumanReadableCopyright", "")).startswith("Copyright "):
        problems.append("Info.plist has no NSHumanReadableCopyright")
    if info.get("CFBundleIconFile") != "BroScribe":
        problems.append("Info.plist does not name the BroScribe icon")
    if inventory.get("target") != build.get("target"):
        problems.append("the licence inventory was generated for another target")
    if require_complete and not inventory.get("complete"):
        problems.append("the third-party licence inventory is incomplete: "
                        + "; ".join(inventory.get("unresolved", [])[:5]))
    if b"Apache License" not in bundle.read("Contents/Resources/LICENSE"):
        problems.append("Contents/Resources/LICENSE is not BroScribe's licence")

    scan = forbidden_paths(forbid)
    scan_binary = forbidden_paths(forbid, upstream_paths(MACOS_SHERPA))
    for name in sorted(bundle.files):
        check_data = scan_binary if name in REQUIRED and "/MacOS/" in name else scan
        if scan(name.encode()) or check_data(bundle.read(name)):
            problems.append(f"{name} embeds a forbidden builder or home path")
    return problems


def symbol_files(path: Path):
    """Every staged/archive file, refusing links instead of following them."""
    if path.is_dir():
        for entry in sorted(path.rglob("*")):
            name = entry.relative_to(path).as_posix()
            if entry.is_symlink():
                raise ValueError(f"symbols contain a symlink: {name}")
            if entry.is_file():
                yield name, entry.read_bytes()
    elif zipfile.is_zipfile(path):
        with zipfile.ZipFile(path) as archive:
            for entry in archive.infolist():
                if stat.S_IFMT(entry.external_attr >> 16) == stat.S_IFLNK:
                    raise ValueError(f"symbols contain a symlink: {entry.filename}")
                if not entry.is_dir():
                    yield entry.filename, archive.read(entry)
    else:
        raise ValueError(f"{path} is neither a symbols folder nor a ZIP")


def check_symbols(path: Path, forbid: list[str]) -> list[str]:
    """Check all symbol files, not only DWARF: dsymutil's maps embed input paths."""
    problems = []
    scan = forbidden_paths(forbid)
    scan_dwarf = forbidden_paths(forbid, upstream_paths(MACOS_SHERPA))
    dwarfs = 0
    uuids = False
    for name, data in symbol_files(path):
        parts = name.split("/")
        if any(part.startswith("._") or part == "__MACOSX" for part in parts):
            problems.append(f"symbols contain AppleDouble metadata: {name}")
        if "Relocations" in parts:
            problems.append(f"symbols contain build-only relocation maps: {name}")
        dwarf = "/Contents/Resources/DWARF/" in f"/{name}"
        if dwarf and data:
            dwarfs += 1
        if parts[-1] == "UUIDS.txt" and data.strip():
            uuids = True
        check_data = scan_dwarf if dwarf else scan
        if scan(name.encode()) or check_data(data):
            problems.append(f"{name} embeds a forbidden builder or home path")
    if not dwarfs:
        problems.append("symbols have no nonempty DWARF files")
    if not uuids:
        problems.append("symbols have no nonempty UUIDS.txt")
    return problems


# The limits and rules of crates/broscribe-updates (settings.rs `Limits` defaults
# and archive.rs `plan`/`entry_parts`/`valid_component`/`link_target`), so no
# artifact passes here that the updater would refuse to install.
ARCHIVE_ROOT = "BroScribe.app"
MAX_ENTRIES = 200_000
MAX_PATH_BYTES = 1024
MAX_DEPTH = 64
MAX_LINK_BYTES = 1024
S_IFMT, S_IFREG, S_IFDIR, S_IFLNK = 0o170000, 0o100000, 0o040000, 0o120000
FORBIDDEN_NAME_BYTES = set(b'\\:*?"<>|')


def valid_component(part: str) -> bool:
    """archive.rs `valid_component` for an ASCII path component."""
    return (part not in ("", ".", "..", "__MACOSX")
            and not part.startswith("._")
            and not part.endswith((".", " "))
            and not any(b < 0x20 or b == 0x7F or b in FORBIDDEN_NAME_BYTES for b in part.encode("ascii")))


def unix_mode(info: zipfile.ZipInfo) -> int | None:
    """The zip crate's `unix_mode()`: Unix mode bits by creating system, DOS
    attributes mapped to a folder or file mode, nothing when attributes are 0."""
    if info.external_attr == 0:
        return None
    high = info.external_attr >> 16
    if info.create_system == 3:  # Unix
        return high & ~0o7000
    if info.create_system == 0:  # MS-DOS
        dos = info.external_attr & 0xFF
        mode = (S_IFDIR | 0o775) if dos & 0x10 else (S_IFREG | 0o664)
        return mode & ~0o222 if dos & 0x01 else mode
    return high & ~0o7000 if high else None


def entry_parts(raw_name: str) -> tuple[list[str] | None, bool, str | None]:
    """archive.rs `entry_parts`: checked components, whether the name ends in
    `/`, or the reason the name is refused."""
    if not raw_name or not raw_name.isascii() or len(raw_name) > MAX_PATH_BYTES:
        return None, False, "is empty, too long or not ASCII"
    slash = raw_name.endswith("/")
    parts = (raw_name[:-1] if slash else raw_name).split("/")
    if len(parts) > MAX_DEPTH:
        return None, slash, "is nested too deeply"
    if parts[0] != ARCHIVE_ROOT:
        return None, slash, f"is outside the archive root {ARCHIVE_ROOT}"
    if not all(valid_component(part) for part in parts):
        return None, slash, "has an unsafe path component (AppleDouble, '.', '..', reserved character or trailing dot/space)"
    return parts, slash, None


def link_target_problem(raw: bytes, link: list[str]) -> str | None:
    """archive.rs `link_target`: leading `..` steps that never climb above the
    root, then plain names."""
    try:
        target = raw.decode("utf-8")
    except UnicodeDecodeError:
        target = None
    if target is None or not target.isascii() or target.startswith("/"):
        return "has an absolute or non-ASCII target"
    climbing, ups, names = True, 0, 0
    for part in target.split("/"):
        if climbing and part == "..":
            ups += 1
            continue
        climbing = False
        if not valid_component(part):
            return f"has an unsafe target {target!r}"
        names += 1
    if ups > len(link) - 2 or (names == 0 and ups == 0):
        return f"points outside the payload ({target!r})"
    return None


def archive_layout(archive: zipfile.ZipFile) -> tuple[list[str], int, int]:
    """Check updater layout and distribution modes, and count entries and
    installed bytes (file sizes plus link target lengths)."""
    infos = archive.infolist()
    problems = []
    if len(infos) > MAX_ENTRIES:
        problems.append(f"the archive has {len(infos)} entries; the updater accepts at most {MAX_ENTRIES}")
    seen: set[str] = set()
    leaves: set[str] = set()
    entries: list[list[str]] = []
    installed = 0
    for info in infos:
        name = info.orig_filename  # before zipfile strips anything after a NUL
        parts, slash, why = entry_parts(name)
        if why:
            problems.append(f"entry {name!r} {why}")
            continue
        shown = "/".join(parts)
        readable = True
        if info.flag_bits & 0x1:
            problems.append(f"{shown} is encrypted")
            readable = False
        if info.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
            problems.append(f"{shown} uses an unsupported compression method ({info.compress_type})")
            readable = False
        mode = unix_mode(info)
        mode_type = mode & S_IFMT if mode is not None else 0
        if slash and mode_type in (0, S_IFDIR):
            kind = "dir"
        elif not slash and mode_type == S_IFLNK:
            kind = "link"
        elif not slash and mode_type in (0, S_IFREG):
            kind = "file"
        else:
            problems.append(f"{shown} is a special file or its type disagrees with its name")
            continue
        # Symlink mode bits are conventionally 0777 and do not control access
        # on macOS. The entries they point to are checked independently.
        if kind != "link":
            permissions = mode & 0o777 if mode is not None else 0
            needed = 0o555 if kind == "dir" or shown in EXECUTABLES or permissions & 0o111 else 0o444
            if permissions & needed != needed or permissions & 0o022:
                problems.append(f"{shown} has unsafe distribution permissions {permissions:04o} "
                                f"(requires {needed:04o} and no group or other write access)")
        size = info.file_size
        if kind == "dir" and size != 0:
            problems.append(f"folder {shown} carries data")
        elif kind == "link" and not 0 < size <= MAX_LINK_BYTES:
            problems.append(f"link {shown} has a bad target length")
        elif kind != "dir" and len(parts) == 1:
            problems.append("the archive root is not a folder")
        elif kind == "link" and readable:
            # Read at most the declared length, as the updater does.
            with archive.open(info) as handle:
                raw = handle.read(size + 1)
            why = "is shorter or longer than its declared size" if len(raw) != size else \
                link_target_problem(raw, parts)
            if why:
                problems.append(f"link {shown} {why}")
        installed += size
        folded = shown.lower()
        if folded in seen:
            problems.append(f"{shown} appears twice (names differ only by case or kind)")
        seen.add(folded)
        if kind != "dir":
            leaves.add(folded)
        entries.append(parts)
    for parts in entries:
        for depth in range(1, len(parts)):
            if "/".join(parts[:depth]).lower() in leaves:
                problems.append(f"{'/'.join(parts)} lies under a file or link")
                break
    return problems, len(infos), installed


def check_checksum(artifact: Path, provenance: Path | None) -> list[str]:
    problems = []
    actual = sha256_file(artifact)
    sidecar = artifact.with_name(artifact.name + ".sha256")
    if sidecar.is_file():
        expected = sidecar.read_text().split()
        if expected != [actual, artifact.name]:
            problems.append(f"{sidecar.name} does not match {artifact.name}")
    elif provenance is None:
        problems.append(f"no {sidecar.name} or --provenance to check {artifact.name} against")
    if provenance is not None:
        record = json.loads(provenance.read_text())
        if record.get("artifact", {}).get("sha256") != actual:
            problems.append("provenance artifact SHA-256 does not match")
        if record.get("artifact", {}).get("size") != artifact.stat().st_size:
            problems.append("provenance artifact size does not match")
        with zipfile.ZipFile(artifact) as archive:
            _, entries, installed = archive_layout(archive)
        if (record.get("artifact", {}).get("entries"), record.get("artifact", {}).get("installed_size")) \
                != (entries, installed):
            problems.append("provenance entry count or installed size does not match the archive")
    return problems


def check_native(path: Path, codesign: bool, stapled: bool) -> list[str]:
    if not (codesign or stapled):
        return []
    if sys.platform != "darwin":
        return ["--codesign/--require-stapled need macOS"]
    work = None
    try:
        app = path
        if not path.is_dir():
            work = Path(tempfile.mkdtemp(prefix="broscribe-verify-"))
            command = ["ditto", "-x", "-k", str(path), str(work)]
            result = run_tool(command)
            if result.returncode != 0:
                raise subprocess.CalledProcessError(result.returncode, command)
            app = work / "BroScribe.app"
        problems = []
        if codesign:
            result = run_tool(["codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app)])
            if result.returncode != 0:
                problems.append(f"codesign --verify failed: {tool_output(result)}")
        if stapled:
            result = run_tool(["xcrun", "stapler", "validate", str(app)])
            if result.returncode != 0:
                problems.append(f"stapler validate failed: {tool_output(result)}")
        return problems
    finally:
        if work is not None:
            shutil.rmtree(work, ignore_errors=True)


# ------------------------------------------------------------- disk image

DMG_APP = "BroScribe.app"
DMG_LINK = "Applications"
DMG_LINK_TARGET = "/Applications"
DMG_FORMAT = "ULFO"
DMG_FILESYSTEM = "apfs"  # hdiutil's `volume-kind` for the mounted volume
# Hidden root entries the filesystem itself may create, by name and kind. An
# APFS ULFO image made by `hdiutil create -fs APFS -srcfolder` and attached
# read-only with -nobrowse has none (observed on macOS 27.2, 2026-11-27); an
# HFS+ image would add .fseventsd, which this layout never uses. The release
# workflow builds and checks an unsigned image on every macOS run, so a
# runner that differs shows up before a tag.
DMG_HIDDEN_ALLOWED: dict[str, str] = {}
# Named in the error so a Finder layout or a resource fork is easy to spot.
DMG_FINDER_METADATA = (".DS_Store", ".Trashes", ".background", ".VolumeIcon.icns", "__MACOSX")
# Created by the filesystem or macOS itself, not by Finder; still refused.
DMG_SYSTEM_ENTRIES = {
    ".fseventsd": "a file system event log, which this APFS image should not have",
    ".Spotlight-V100": "a Spotlight index, which a read-only -nobrowse attach should not create",
    ".DocumentRevisions-V100": "a document revision store",
    ".TemporaryItems": "a temporary-items folder",
}
ATTACH_TIMEOUT = 300
DETACH_TIMEOUT = 120
DETACH_ATTEMPTS = 3
DETACH_PAUSE = 2.0
MAX_DIFFERENCES = 10
# Apple's Developer ID Application certificate chain, pinned to one team.
DEVELOPER_ID_REQUIREMENT = (
    "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists"
    " and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
    ' and certificate leaf[subject.OU] = "{team}"'
)


def release_metadata():
    """scripts/release-metadata.py, for MACOS_TEAM and its record rules."""
    module = sys.modules.get("release_metadata")
    if module is None:
        from desktop_artifacts import sibling
        module = sibling("release-metadata")
    return module


def macos_team() -> str:
    """MACOS_TEAM from release-metadata.py, the team the updater pins."""
    return release_metadata().MACOS_TEAM


def kind_of(mode: int) -> str:
    return {S_IFDIR: "dir", S_IFLNK: "link", S_IFREG: "file"}.get(mode & S_IFMT, "special")


def tree_entries(app: Path) -> dict[str, tuple[str, int | None, str | bytes | None]]:
    """`BroScribe.app/...` → (kind, permission bits, SHA-256 or link target)
    for the folder and everything in it, from lstat. Links are never followed."""
    entries = {}
    pending = [(app, DMG_APP)]
    while pending:
        path, name = pending.pop()
        info = os.lstat(path)
        kind = kind_of(info.st_mode)
        if kind == "link":
            entries[name] = (kind, None, os.fsencode(os.readlink(path)))
        elif kind == "dir":
            entries[name] = (kind, stat.S_IMODE(info.st_mode), None)
            pending += [(path / child, f"{name}/{child}") for child in sorted(os.listdir(path))]
        elif kind == "file":
            entries[name] = (kind, stat.S_IMODE(info.st_mode), sha256_file(path))
        else:
            entries[name] = (kind, stat.S_IMODE(info.st_mode), None)
    return entries


def zip_entries(archive: zipfile.ZipFile) -> dict[str, tuple[str, int | None, str | bytes | None]]:
    """The same view of a ZIP's entries, with the folders it only implies."""
    entries = {}
    for info in archive.infolist():
        name = info.orig_filename.rstrip("/")
        mode = unix_mode(info)
        if info.orig_filename.endswith("/"):
            kind = "dir"
        elif mode is not None and mode & S_IFMT == S_IFLNK:
            kind = "link"
        elif mode is None or mode & S_IFMT == S_IFREG:
            kind = "file"
        else:
            kind = "special"
        permissions = mode & 0o777 if mode is not None and kind != "link" else None
        if kind == "file":
            digest = hashlib.sha256()
            with archive.open(info) as handle:
                for block in iter(lambda: handle.read(1 << 20), b""):
                    digest.update(block)
            entries[name] = (kind, permissions, digest.hexdigest())
        elif kind == "link":
            entries[name] = (kind, None, archive.read(info))
        else:
            entries[name] = (kind, permissions, None)
    for name in list(entries):
        parts = name.split("/")
        for depth in range(1, len(parts)):
            entries.setdefault("/".join(parts[:depth]), ("dir", None, None))
    return entries


def compare_trees(mounted: dict, archived: dict) -> list[str]:
    """Differences between the mounted app and the ZIP's app."""
    differences = [f"{name} is only in the disk image" for name in sorted(mounted.keys() - archived.keys())]
    differences += [f"{name} is missing from the disk image" for name in sorted(archived.keys() - mounted.keys())]
    for name in sorted(mounted.keys() & archived.keys()):
        (kind, mode, data), (zip_kind, zip_mode, zip_data) = mounted[name], archived[name]
        if kind != zip_kind:
            differences.append(f"{name} is a {kind} in the disk image but a {zip_kind} in the ZIP")
        elif kind == "special":
            differences.append(f"{name} is a special file")
        elif mode is not None and mode & 0o7000:
            differences.append(f"{name} has setuid, setgid or sticky bits")
        elif zip_mode is not None and mode is not None and mode & 0o777 != zip_mode:
            differences.append(f"{name} has mode {mode & 0o777:04o} in the disk image, {zip_mode:04o} in the ZIP")
        elif data != zip_data:
            differences.append(f"{name} has a different {'target' if kind == 'link' else 'content'}")
    if len(differences) > MAX_DIFFERENCES:
        differences[MAX_DIFFERENCES:] = [f"and {len(differences) - MAX_DIFFERENCES} more differences"]
    return [f"the disk image's app differs from the ZIP: {item}" for item in differences]


class TreeBundle(Bundle):
    """A mounted app as a Bundle; links read as their target, like ZIP entries."""

    def __init__(self, app: Path, entries: dict):
        self.path, self.zip = app, None
        prefix = DMG_APP + "/"
        self.files = {name[len(prefix):]: app / name[len(prefix):] for name, (kind, _, _) in entries.items()
                      if name.startswith(prefix) and kind in ("file", "link")}

    def read(self, relative: str) -> bytes:
        path = self.files[relative]
        return os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()


def check_dmg_root(mount: Path) -> list[str]:
    """Exactly BroScribe.app and the Applications link (lstat only), plus only
    allowlisted filesystem entries (none)."""
    problems = []
    names = sorted(os.listdir(mount))
    for name in names:
        if not name.startswith((".", "__MACOSX")):
            continue
        info = os.lstat(mount / name)
        if DMG_HIDDEN_ALLOWED.get(name) == kind_of(info.st_mode):
            continue
        if name in DMG_FINDER_METADATA:
            what = " (Finder metadata)"
        elif name.startswith("._"):
            what = " (AppleDouble metadata)"
        elif name in DMG_SYSTEM_ENTRIES:
            what = f" ({DMG_SYSTEM_ENTRIES[name]}; DMG_HIDDEN_ALLOWED allows no hidden entry)"
        else:
            what = " (a hidden entry; DMG_HIDDEN_ALLOWED allows none)"
        problems.append(f"the disk image root has an unexpected entry {name!r}{what}")
    visible = [name for name in names if not name.startswith((".", "__MACOSX"))]
    if visible != sorted((DMG_APP, DMG_LINK)):
        problems.append(f"the disk image root must hold exactly {DMG_APP} and {DMG_LINK}, not {visible}")
    if DMG_APP in names and kind_of(os.lstat(mount / DMG_APP).st_mode) != "dir":
        problems.append(f"{DMG_APP} on the disk image is not a folder")
    if DMG_LINK in names:
        if kind_of(os.lstat(mount / DMG_LINK).st_mode) != "link":
            problems.append(f"{DMG_LINK} on the disk image is not a symbolic link")
        elif os.readlink(mount / DMG_LINK) != DMG_LINK_TARGET:
            problems.append(f"{DMG_LINK} on the disk image does not point to {DMG_LINK_TARGET}")
    return problems


def image_info_problems(dmg: Path) -> list[str]:
    result = run_tool(["hdiutil", "imageinfo", "-plist", str(dmg)], timeout=ATTACH_TIMEOUT)
    if result.returncode != 0:
        return [f"hdiutil imageinfo failed: {tool_output(result)}"]
    info = plistlib.loads(result.stdout)
    properties = info.get("Properties") or {}
    problems = []
    if info.get("Format") != DMG_FORMAT:
        problems.append(f"the disk image format is {info.get('Format')!r}, not {DMG_FORMAT}")
    if properties.get("Encrypted") is not False:
        problems.append("the disk image is encrypted or does not say it is not")
    if properties.get("Software License Agreement") is not False:
        problems.append("the disk image carries a licence agreement resource")
    return problems


def image_device(entities: list) -> str | None:
    """The disk image's own whole disk, which detaching takes down with every
    partition and synthesized APFS container: the whole disk carrying the
    partition scheme (`content-hint` GUID_partition_scheme and the like), else
    the first whole disk in hdiutil's `system-entities` order. Device numbers
    are not compared: the synthesized container may have the lower one."""
    disks = [entity for entity in entities if isinstance(entity, dict)
             and re.fullmatch(r"/dev/disk[0-9]+", str(entity.get("dev-entry", "")))]
    for entity in disks:
        if str(entity.get("content-hint", "")).endswith("_partition_scheme"):
            return entity["dev-entry"]
    return disks[0]["dev-entry"] if disks else None


def attach(dmg: Path, mountpoint: Path) -> tuple[str | None, list[str]]:
    """Attach read-only at `mountpoint`. Returns the image's device (for
    detaching) and problems; the device may be known even when attaching
    failed. The one mounted volume must be APFS."""
    result = run_tool(["hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen",
                       "-mountpoint", str(mountpoint), "-plist", str(dmg)], timeout=ATTACH_TIMEOUT)
    try:
        entities = plistlib.loads(result.stdout).get("system-entities") or [] if result.stdout else []
    except (plistlib.InvalidFileException, ValueError, AttributeError):
        entities = []
    device = image_device(entities)
    if result.returncode != 0:
        return device, [f"hdiutil attach failed: {tool_output(result)}"]
    volumes = [entity for entity in entities if isinstance(entity, dict) and entity.get("mount-point")]
    mounted = [entity["mount-point"] for entity in volumes]
    if device is None or len(mounted) != 1 or os.path.realpath(mounted[0]) != os.path.realpath(mountpoint):
        return device, [f"hdiutil attached the disk image at {mounted} instead of only the private mount point"]
    kind = volumes[0].get("volume-kind")
    if kind != DMG_FILESYSTEM:
        return device, [f"the mounted volume is {kind!r}, not the {DMG_FILESYSTEM} volume the image must hold"]
    return device, []


def detach(target: str) -> bool:
    """Detach, retrying while the volume is busy, then force it."""
    for attempt in range(DETACH_ATTEMPTS + 1):
        command = ["hdiutil", "detach", *(["-force"] if attempt == DETACH_ATTEMPTS else []), target]
        try:
            if run_tool(command, timeout=DETACH_TIMEOUT).returncode == 0:
                return True
        except subprocess.TimeoutExpired:
            pass
        if attempt < DETACH_ATTEMPTS:
            time.sleep(DETACH_PAUSE)
    return False


def developer_id_requirement() -> str:
    return "=" + DEVELOPER_ID_REQUIREMENT.format(team=macos_team())


def outer_native_problems(dmg: Path, image: dict | None, codesign: bool, stapled: bool) -> list[str]:
    """The disk image file's own signature, timestamp, ticket and Gatekeeper
    verdict, checked before it is attached. With a provenance `image`, the
    signer codesign reports must be the one recorded."""
    problems = []
    if codesign:
        result = run_tool(["codesign", "--verify", "--strict", "--verbose=2", "-R", developer_id_requirement(),
                           str(dmg)])
        if result.returncode != 0:
            problems.append(f"the disk image is not signed by a Developer ID Application of team "
                            f"{macos_team()}: {tool_output(result)}")
        display = run_tool(["codesign", "--display", "--verbose=4", str(dmg)])
        if display.returncode != 0:
            problems.append(f"codesign --display failed on the disk image: {tool_output(display)}")
        lines = tool_output(display).splitlines()
        if not any(line.startswith("Timestamp=") for line in lines):
            problems.append("the disk image signature has no secure timestamp")
        if image is not None:
            signer = {"authorities": [line.split("=", 1)[1] for line in lines if line.startswith("Authority=")],
                      "team_identifier": next((line.split("=", 1)[1] for line in lines
                                               if line.startswith("TeamIdentifier=")), None)}
            signing = image.get("signing") if isinstance(image.get("signing"), dict) else {}
            recorded = {key: signing.get(key) for key in signer}
            if recorded != signer:
                problems.append(f"the disk image's signer {signer} is not the one in the provenance {recorded}")
    if stapled:
        result = run_tool(["xcrun", "stapler", "validate", str(dmg)])
        if result.returncode != 0:
            problems.append(f"stapler validate failed on the disk image: {tool_output(result)}")
        result = run_tool(["spctl", "-a", "-t", "open", "--context", "context:primary-signature", "-v", str(dmg)])
        if result.returncode != 0:
            problems.append(f"Gatekeeper refuses the disk image: {tool_output(result)}")
    return problems


def mounted_app_native_problems(app: Path, codesign: bool, stapled: bool) -> list[str]:
    problems = []
    if codesign:
        result = run_tool(["codesign", "--verify", "--deep", "--strict", "--verbose=2", "-R",
                           developer_id_requirement(), str(app)])
        if result.returncode != 0:
            problems.append(f"codesign --verify failed on the mounted app: {tool_output(result)}")
    if stapled:
        result = run_tool(["xcrun", "stapler", "validate", str(app)])
        if result.returncode != 0:
            problems.append(f"stapler validate failed on the mounted app: {tool_output(result)}")
    return problems


def dmg_checksum_problems(dmg: Path, compare_zip: Path, record: dict | None, raw: bytes | None,
                          forbid: list[str], codesign: bool) -> list[str]:
    """The sidecar, and the provenance `disk_image` read back with the rules it
    was recorded under (release-metadata.py disk_image_problems), its team also
    pinned to MACOS_TEAM under --codesign."""
    problems = []
    actual = sha256_file(dmg)
    sidecar = dmg.with_name(dmg.name + ".sha256")
    if not sidecar.is_file():
        problems.append(f"no {sidecar.name} beside the disk image")
    elif sidecar.read_text().split() != [actual, dmg.name]:
        problems.append(f"{sidecar.name} does not match {dmg.name}")
    if record is None:
        return problems
    if forbidden_paths(forbid)(raw):
        problems.append("provenance embeds a forbidden builder or home path")
    image = record.get("disk_image")
    if not isinstance(image, dict):
        return problems + ["the provenance has no disk_image"]
    if (image.get("name"), image.get("sha256"), image.get("size")) != (dmg.name, actual, dmg.stat().st_size):
        problems.append("provenance disk_image name, SHA-256 or size does not match the disk image")
    problems += [f"provenance disk_image: {problem}" for problem in release_metadata().disk_image_problems(
        image, record.get("signing"), macos_team() if codesign else None)]
    artifact = record.get("artifact") or {}
    if (artifact.get("name"), artifact.get("sha256")) != (compare_zip.name, sha256_file(compare_zip)):
        problems.append("the provenance does not describe the --compare-zip archive")
    return problems


def check_disk_image(dmg: Path, compare_zip: Path, provenance: Path | None, forbid: list[str],
                     require_complete: bool, codesign: bool, stapled: bool) -> list[str]:
    if not dmg.is_file() or not zipfile.is_zipfile(compare_zip):
        raise ValueError("--dmg needs a disk image file and --compare-zip a ZIP")
    if dmg.name.removesuffix(".dmg") != compare_zip.name.removesuffix(".zip") or dmg.suffix != ".dmg":
        return [f"{dmg.name} is not the disk image of {compare_zip.name}"]
    raw = provenance.read_bytes() if provenance is not None else None
    record = json.loads(raw) if raw is not None else None
    if provenance is not None and not isinstance(record, dict):
        raise ValueError(f"{provenance} is not a JSON object")
    # Everything about the file itself first: nothing is attached unless the
    # checksum, provenance, image format and (as asked) signature, ticket and
    # Gatekeeper verdict all pass.
    problems = dmg_checksum_problems(dmg, compare_zip, record, raw, forbid, codesign)
    with zipfile.ZipFile(compare_zip) as archive:
        problems += [f"--compare-zip: {problem}" for problem in archive_layout(archive)[0]]
        archived = zip_entries(archive)
    problems += image_info_problems(dmg)
    if not problems:
        image = record.get("disk_image") if record is not None else None
        problems += outer_native_problems(dmg, image, codesign, stapled)
    if problems:
        return problems + ["the disk image was not attached: its outer checks failed"]
    work = Path(tempfile.mkdtemp(prefix="broscribe-dmg-"))
    mountpoint = work / "mount"
    mountpoint.mkdir()
    device = None
    try:
        device, attach_problems = attach(dmg, mountpoint)
        if attach_problems:
            problems += attach_problems
            return problems
        problems += check_dmg_root(mountpoint)
        app = mountpoint / DMG_APP
        if kind_of(os.lstat(app).st_mode if os.path.lexists(app) else 0) != "dir":
            return problems
        mounted = tree_entries(app)
        problems += compare_trees(mounted, archived)
        bundle = TreeBundle(app, mounted)
        problems += check_bundle(bundle, forbid, require_complete)
        if record is not None and "Contents/Info.plist" in bundle.files:
            info = plistlib.loads(bundle.read("Contents/Info.plist"))
            if (record.get("version"), str(record.get("build"))) != (
                    info.get("CFBundleShortVersionString"), info.get("CFBundleVersion")):
                problems.append("provenance version/build does not match the disk image's Info.plist")
        problems += mounted_app_native_problems(app, codesign, stapled)
        return problems
    finally:
        target = device or (str(mountpoint) if os.path.ismount(mountpoint) else None)
        if target is not None and not detach(target):
            problems.append(f"could not detach the disk image ({target})")
        if not os.path.ismount(mountpoint):
            shutil.rmtree(work, ignore_errors=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("artifact", type=Path, nargs="?")
    parser.add_argument("--dmg", type=Path, help="check this disk image (requires --compare-zip)")
    parser.add_argument("--compare-zip", type=Path,
                        help="the ZIP whose app the disk image must contain exactly")
    parser.add_argument("--provenance", type=Path)
    parser.add_argument("--require-complete-licences", action="store_true")
    parser.add_argument("--forbid", action="append", default=[])
    parser.add_argument("--codesign", action="store_true")
    parser.add_argument("--require-stapled", action="store_true")
    parser.add_argument("--symbols", action="store_true",
                        help="check a symbols folder/ZIP, including non-DWARF files")
    args = parser.parse_args(argv)
    if (args.artifact is None) == (args.dmg is None):
        parser.error("name either an app, ZIP or symbols, or --dmg")
    if (args.dmg is None) != (args.compare_zip is None):
        parser.error("--dmg and --compare-zip go together")
    if args.dmg and args.symbols:
        parser.error("--dmg and --symbols are different checks")
    if args.symbols and (args.provenance or args.require_complete_licences
                         or args.codesign or args.require_stapled):
        parser.error("--symbols accepts only --forbid checks")
    if args.dmg is not None:
        args.artifact = args.dmg
    try:
        forbid = [*builder_paths(MACOS_SHERPA, build_roots()), *args.forbid]
        if args.dmg is not None:
            problems = check_disk_image(args.dmg, args.compare_zip, args.provenance, forbid,
                                        args.require_complete_licences, args.codesign, args.require_stapled)
        elif args.symbols:
            problems = check_symbols(args.artifact, forbid)
        else:
            bundle = Bundle(args.artifact)
            problems = check_bundle(bundle, forbid, args.require_complete_licences)
            if bundle.zip is not None:
                problems += archive_layout(bundle.zip)[0]
                problems += check_checksum(args.artifact, args.provenance)
                if args.provenance is not None:
                    raw = args.provenance.read_bytes()
                    if forbidden_paths(forbid)(raw):
                        problems.append("provenance embeds a forbidden builder or home path")
                    record = json.loads(raw)
                    info = plistlib.loads(bundle.read("Contents/Info.plist"))
                    if (record.get("version"), str(record.get("build"))) != (
                            info.get("CFBundleShortVersionString"), info.get("CFBundleVersion")):
                        problems.append("provenance version/build does not match Info.plist")
            problems += check_native(args.artifact, args.codesign, args.require_stapled)
    except (ValueError, OSError, KeyError, json.JSONDecodeError, plistlib.InvalidFileException,
            zipfile.BadZipFile, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(f"verify-macos-artifact: {error}", file=sys.stderr)
        return 2
    for problem in problems:
        print(f"verify-macos-artifact: {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"ok: {args.artifact}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

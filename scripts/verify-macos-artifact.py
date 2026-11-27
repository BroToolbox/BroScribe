#!/usr/bin/env python3
"""Check a BroScribe.app bundle or its distributable ZIP without credentials.

  scripts/verify-macos-artifact.py <BroScribe.app | artifact.zip>
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
Exit status: 0 pass, 1 check failed, 2 usage or unreadable input.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import plistlib
import shutil
import subprocess
import stat
import sys
import tempfile
import zipfile
from pathlib import Path

from desktop_artifacts import build_roots, builder_paths, forbidden_paths, upstream_paths

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
            subprocess.run(["ditto", "-x", "-k", str(path), str(work)], check=True)
            app = work / "BroScribe.app"
        problems = []
        if codesign:
            result = subprocess.run(["codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app)],
                                    capture_output=True, text=True)
            if result.returncode != 0:
                problems.append(f"codesign --verify failed: {result.stderr.strip()}")
        if stapled:
            result = subprocess.run(["xcrun", "stapler", "validate", str(app)], capture_output=True, text=True)
            if result.returncode != 0:
                problems.append(f"stapler validate failed: {(result.stdout + result.stderr).strip()}")
        return problems
    finally:
        if work is not None:
            shutil.rmtree(work, ignore_errors=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("artifact", type=Path)
    parser.add_argument("--provenance", type=Path)
    parser.add_argument("--require-complete-licences", action="store_true")
    parser.add_argument("--forbid", action="append", default=[])
    parser.add_argument("--codesign", action="store_true")
    parser.add_argument("--require-stapled", action="store_true")
    parser.add_argument("--symbols", action="store_true",
                        help="check a symbols folder/ZIP, including non-DWARF files")
    args = parser.parse_args(argv)
    if args.symbols and (args.provenance or args.require_complete_licences
                         or args.codesign or args.require_stapled):
        parser.error("--symbols accepts only --forbid checks")
    try:
        forbid = [*builder_paths(MACOS_SHERPA, build_roots()), *args.forbid]
        if args.symbols:
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
            zipfile.BadZipFile, subprocess.CalledProcessError) as error:
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

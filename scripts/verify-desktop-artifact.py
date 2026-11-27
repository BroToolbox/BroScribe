#!/usr/bin/env python3
"""Verify a BroScribe Linux tar.gz or Windows ZIP without extracting or running it."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import struct
import sys
import tarfile
import zipfile

from desktop_artifacts import (
    APP_ID, BINARIES, DESKTOP, ICONS, TARGETS, artifact_record, release_files,
    build_roots, builder_paths, forbidden_paths, json_object, sha256_file, upstream_paths,
)

ROOT = "BroScribe/"
NATIVE_IMPORT = re.compile(rb"(?:sherpa[-_]onnx|onnxruntime)", re.IGNORECASE)


def elf_runpath(data: bytes, machine: int) -> None:
    # Inspect the ELF64 program headers so stripped executables work, without
    # running the target program or requiring Linux tools on the verifying host.
    if (data[:6] != b"\x7fELF\x02\x01"
            or struct.unpack_from("<HH", data, 16)[0] not in (2, 3)
            or struct.unpack_from("<H", data, 18)[0] != machine):
        raise ValueError("wrong ELF format or machine")
    offset = struct.unpack_from("<Q", data, 32)[0]
    stride, count = struct.unpack_from("<HH", data, 54)
    if stride != 56 or offset + stride * count > len(data):
        raise ValueError("invalid ELF program headers")
    loads = []
    dynamic = None
    for index in range(count):
        kind, _, start, address, _, length = struct.unpack_from("<IIQQQQ", data, offset + stride * index)
        if kind == 1:
            loads.append((start, address, length))
        elif kind == 2:
            dynamic = (start, length)
    if dynamic is None:
        raise ValueError("missing ELF dynamic segment")
    start, length = dynamic
    if length % 16 or start + length > len(data):
        raise ValueError("invalid ELF dynamic segment")
    values: dict[int, list[int]] = {}
    for tag, value in struct.iter_unpack("<qQ", memoryview(data)[start:start + length]):
        if tag == 0:
            break
        values.setdefault(tag, []).append(value)
    (address,) = values.get(5, [])
    (size,) = values.get(10, [])
    for start, base, length in loads:
        if base <= address and address + size <= base + length:
            start += address - base
            if start + size <= len(data):
                strings = data[start:start + size]
                break
    else:
        raise ValueError("invalid ELF dynamic string table")

    def string_at(index: int) -> bytes:
        end = strings.find(b"\0", index)
        if end < 0:
            raise ValueError("invalid ELF dynamic string")
        return strings[index:end]

    if [string_at(index) for index in values.get(15, []) + values.get(29, [])] != [b"$ORIGIN"]:
        raise ValueError('ELF RPATH/RUNPATH must be exactly $ORIGIN')
    for index in values.get(1, []):
        name = string_at(index)
        if b"/" in name or NATIVE_IMPORT.search(name):
            raise ValueError("ELF links a path or an unshipped sherpa/ONNX shared library")


def check_pe(data: bytes) -> None:
    if data[:2] != b"MZ":
        raise ValueError("missing PE header")
    header = struct.unpack_from("<I", data, 60)[0]
    signature, machine, _, _, _, _, size, flags = struct.unpack_from("<4sHHIIIHH", data, header)
    if (signature != b"PE\0\0" or machine != 0x8664 or size < 2 or not flags & 2 or flags & 0x2000
            or struct.unpack_from("<H", data, header + 24)[0] != 0x20B):
        raise ValueError("expected a Windows x64 PE32+ executable")


def archive_members(archive):
    if isinstance(archive, zipfile.ZipFile):
        for entry in archive.infolist():
            mode = entry.external_attr >> 16
            directory = entry.is_dir()
            if (entry.flag_bits & 1 or entry.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED)
                    or stat.S_IFMT(mode) not in (0, stat.S_IFDIR if directory else stat.S_IFREG)
                    or entry.external_attr & 0x400):  # Windows reparse point
                raise ValueError(f"unsupported ZIP entry {entry.orig_filename!r}")
            yield entry.orig_filename, directory, mode, entry.file_size, entry
    else:
        for entry in archive:
            if entry.type not in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE) or entry.pax_headers:
                raise ValueError(f"unsupported tar entry {entry.name!r}")
            yield entry.name, entry.isdir(), entry.mode, entry.size, entry


def inspect(artifact: Path, suffix: str, target: str, machine: int, forbid: list[str], *,
            allow_legacy_layout: bool = False):
    linux = suffix.startswith("linux-")
    binaries = {name if linux else name + ".exe" for name in BINARIES}
    scripts = {"install.sh", "uninstall.sh"} if linux else set()
    current = release_files(linux)
    layouts = [current]
    if allow_legacy_layout:
        history = json.loads((Path(__file__).resolve().parents[1] /
                              "crates/broscribe-platform/src/release_layouts.json").read_text())
        layouts = [set(layout) for layout in history["linux" if linux else "windows"]]
    allowed = set.union(*layouts)
    directories = {ROOT.rstrip("/")}
    for name in allowed:
        directories.update(parent.as_posix() for parent in PurePosixPath(ROOT + name).parents
                           if parent.parts)
    seen, files, metadata = set(), set(), {}
    archived_dirs, executable_files = set(), set()
    entries = installed = 0
    scan = forbidden_paths(forbid)
    scan_binary = forbidden_paths(forbid, upstream_paths(TARGETS[target][0]))
    if linux:
        handle = tarfile.open(artifact, "r:gz")
    else:
        handle = zipfile.ZipFile(artifact)
    with handle as archive:
        if isinstance(archive, zipfile.ZipFile) and archive.comment:
            raise ValueError("ZIP contains an archive comment")
        for raw, is_dir, mode, size, entry in archive_members(archive):
            name = raw.removesuffix("/") if is_dir else raw
            entries += 1
            if name.casefold() in seen or ".private." in name.casefold():
                raise ValueError(f"duplicate or private archive entry {name!r}")
            seen.add(name.casefold())
            if mode & 0o7022:
                raise ValueError(f"unsafe permissions on {name!r}")
            if is_dir:
                if name not in directories or size:
                    raise ValueError(f"unexpected archive directory {name!r}")
                archived_dirs.add(name)
                continue
            relative = name.removeprefix(ROOT)
            if name == relative or relative not in allowed:
                raise ValueError(f"unexpected archive file {name!r}")
            if not size:
                raise ValueError(f"empty archive file {name!r}")
            if linux and relative in binaries | scripts and mode & 0o111 != 0o111:
                raise ValueError(f"{name} is missing executable bits")
            installed += size
            files.add(relative)
            if mode & 0o111:
                executable_files.add(relative)
            data = archive.extractfile(entry).read() if linux else archive.read(entry)
            check_data = scan_binary if relative in binaries else scan
            if found := check_data(data) or scan(raw.encode("utf-8")):
                # Name the match: the build log is the only place to find it.
                shown = found.replace(b"\0", b"").decode("utf-8", "backslashreplace")
                raise ValueError(f"{relative} contains a forbidden builder or home path: {shown}")
            if relative in binaries:
                try:
                    elf_runpath(data, machine) if linux else check_pe(data)
                except (ValueError, struct.error) as error:
                    raise ValueError(f"{relative}: {error}") from None
            elif relative == "LICENSE" and b"Apache License" not in data:
                raise ValueError("invalid BroScribe licence")
            elif relative == "NOTICE" and not any(line.startswith(b"Copyright ") for line in data.splitlines()):
                raise ValueError("invalid BroScribe NOTICE")
            elif relative == "THIRD_PARTY_NOTICES.txt":
                if not data.startswith(f"BroScribe third-party notices\nTarget: {target}\n".encode()) \
                        or b"\nINCOMPLETE:" in data or b"\nLicence and notice texts\n" not in data:
                    raise ValueError("invalid or incomplete third-party notices")
            elif relative in ("build-info.json", "third-party-inventory.json"):
                metadata[relative] = (json_object(data), data)
            elif relative == DESKTOP:
                if not all(line in data.splitlines() for line in (
                    b"[Desktop Entry]", b"Type=Application", b"Exec=broscribe-app", f"Icon={APP_ID}".encode()
                )):
                    raise ValueError("invalid BroScribe desktop file")
            elif relative in ICONS:
                dimensions = (ICONS[relative],) * 2
                if (len(data) < 24 or data[:16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR"
                        or struct.unpack_from(">II", data, 16) != dimensions):
                    raise ValueError(f"{relative} is not a PNG of the declared size")
    if files not in layouts:
        if not allow_legacy_layout:
            raise ValueError(f"missing archive files: {', '.join(sorted(current - files))}")
        raise ValueError("archive file set does not match a known release layout")
    if linux and (executable_resources := executable_files & (files - binaries - scripts)):
        raise ValueError(f"{ROOT}{min(executable_resources)} must be a non-executable resource")
    # Directory members must also belong to the layout selected by the files.
    selected_dirs = {parent.as_posix() for name in files
                     for parent in PurePosixPath(ROOT + name).parents if parent.parts}
    if extra := archived_dirs - selected_dirs:
        raise ValueError(f"unexpected archive directories: {', '.join(sorted(extra))}")
    build = metadata["build-info.json"][0]
    inventory, inventory_bytes = metadata["third-party-inventory.json"]
    if (inventory.get("schema") != 1 or inventory.get("target") != target
            or inventory.get("complete") is not True or inventory.get("unresolved") != []):
        raise ValueError("third-party licence inventory is incomplete or for another target")
    for section in ("packages", "native", "embedded"):
        if not inventory.get(section) or any(item.get("status") != "ok" for item in inventory[section]):
            raise ValueError(f"third-party inventory has incomplete {section}")
    return build, inventory_bytes, entries, installed


def verify(artifact: Path, provenance: Path | None = None, forbid: list[str] = (),
           target_dir: Path | None = None, *, allow_legacy_layout: bool = False) -> None:
    match = re.fullmatch(r"BroScribe-(\d+\.\d+\.\d+)-([1-9]\d{0,8})-(linux-(?:x86_64|aarch64)\.tar\.gz|windows-x64\.zip)",
                         artifact.name, re.ASCII)
    if not match:
        raise ValueError("invalid release archive name")
    version, number, filename_suffix = match.groups()
    suffix = filename_suffix.split(".")[0]
    target, machine = next((target, machine) for target, (_, label, machine) in TARGETS.items() if label == suffix)
    forbid = [*builder_paths(TARGETS[target][0], build_roots(target_dir)), *forbid]
    checksum = sha256_file(artifact)
    if artifact.with_name(artifact.name + ".sha256").read_text().split() != [checksum, artifact.name]:
        raise ValueError("SHA-256 sidecar does not match archive")
    build, inventory, entries, installed = inspect(
        artifact, suffix, target, machine, forbid, allow_legacy_layout=allow_legacy_layout)
    if (build.get("schema"), build.get("product"), build.get("version"), build.get("build"),
            build.get("target"), build.get("profile")) != (1, "BroScribe", version, int(number), target, "release"):
        raise ValueError("build-info.json disagrees with release name")
    expected = artifact_record(artifact, build, inventory, entries, installed)
    if provenance is None:
        provenance = artifact.with_name(artifact.name.removesuffix(".zip").removesuffix(".tar.gz") + ".provenance.json")
    raw = provenance.read_bytes()
    if forbidden_paths(forbid)(raw):
        raise ValueError("provenance contains a forbidden builder or home path")
    if json_object(raw) != expected:
        raise ValueError("provenance disagrees with archive or build information")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifact", type=Path)
    parser.add_argument("--provenance", type=Path)
    parser.add_argument("--forbid", action="append", default=[])
    args = parser.parse_args()
    try:
        verify(args.artifact, args.provenance, args.forbid)
    except (OSError, EOFError, ValueError, KeyError, AttributeError, TypeError, struct.error,
            zipfile.BadZipFile, tarfile.TarError) as error:
        print(f"verify-desktop-artifact: {error}", file=sys.stderr)
        return 1
    print(f"ok: {args.artifact}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

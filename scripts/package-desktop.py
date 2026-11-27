#!/usr/bin/env python3
"""Build and package a native BroScribe Linux or Windows release."""

from __future__ import annotations

import argparse
import gzip
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from types import SimpleNamespace
import zipfile

from desktop_artifacts import (
    BINARIES, DESKTOP, ICONS, LICENCES, REPO, TARGETS, artifact_record, build_root_spellings,
    build_roots, builder_paths, json_bytes, json_object, sibling,
)

metadata = sibling("release-metadata")
verifier = sibling("verify-desktop-artifact")
SOURCE = REPO / "packaging/linux"
BRAND = REPO / "crates/broscribe-app/assets/brand/linux"
PINS = REPO / "third_party/sherpa-onnx"


def run(*command, **kwargs):
    return subprocess.run(command, check=True, cwd=REPO, **kwargs)


def output(*command, **kwargs) -> str:
    return run(*command, stdout=subprocess.PIPE, encoding="utf-8", **kwargs).stdout.strip()


def clean_commit() -> str:
    env = {**os.environ, "GIT_OPTIONAL_LOCKS": "0"}
    if output("git", "status", "--porcelain=v1", "--untracked-files=normal", env=env):
        raise ValueError("release needs a clean checkout, including untracked source files")
    return output("git", "rev-parse", "--verify", "HEAD", env=env)


def find_bash(windows: bool) -> str:
    bash = shutil.which("bash")
    if windows:
        if git := shutil.which("git"):
            for root in Path(git).parents[1:3]:
                git_bash = root / "bin" / "bash.exe"
                if git_bash.is_file():
                    return str(git_bash.resolve())
        # CreateProcess and PATH can both find the system WSL launcher.
        if bash and Path(bash).parent.name.lower() in {"system32", "syswow64", "sysnative", "windowsapps"}:
            bash = None
    if not bash:
        raise ValueError("Git Bash is required for Windows packaging" if windows else "bash is required for packaging")
    return str(Path(bash).resolve())


def build_native(target: str, target_dir: Path, remaps: dict[str, Path], bash: str) -> None:
    windows = "windows" in target
    env = {**os.environ, "CARGO_TARGET_DIR": str(target_dir), "CARGO_PROFILE_RELEASE_STRIP": "debuginfo"}
    target_key = target.replace("-", "_")
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", f"CARGO_TARGET_{target_key.upper()}_RUSTFLAGS"):
        if name in env:
            raise ValueError(f"unset {name} before a release build")
    spellings = build_root_spellings(remaps)
    flags = [f"--remap-path-prefix={source}=/{label}" for label, source in spellings]
    linker = None
    if windows:
        env["MSYS2_ARG_CONV_EXCL"] = "*"
        # Git Bash puts its usr/bin first, where coreutils `link` shadows the
        # MSVC linker rustc would find on PATH. Name MSVC's linker explicitly.
        linker = shutil.which("link.exe")
        if not linker or not (Path(linker).parent / "cl.exe").is_file():
            raise ValueError("put the MSVC tools (link.exe beside cl.exe) first on PATH")
        # rustc and CMake can spell a Windows path with either separator.
        flags = list(dict.fromkeys(
            f"--remap-path-prefix={path}=/{label}" for label, source in spellings
            for path in (str(source), source.as_posix())))
        flags += ["-Clink-arg=/PDBALTPATH:%_PDB%"]
        if any(re.search(r"""[\s'"%;]""", str(path)) for _, path in spellings):
            raise ValueError("Windows C/C++ path trimming requires build paths without whitespace, quotes, percent signs or semicolons")
        trim_flags = [f"/d1trimfile:{path}" for _, source in spellings
                      for path in (str(source).replace("/", "\\"), source.as_posix())]
        # cc can parse shell quoting, but the cmake crate rejoins the resulting
        # compiler arguments when it populates CMAKE_C_FLAGS / CMAKE_CXX_FLAGS.
        shell_escaped = env.get("CC_SHELL_ESCAPED_FLAGS") not in (None, "", "0", "false", "no")
        maps = shlex.join(trim_flags) if shell_escaped else " ".join(trim_flags)
    else:
        flags += ["-Clink-arg=-Wl,-rpath,$ORIGIN"]
        if any(not re.fullmatch(r"/[A-Za-z0-9._/+@,-]+", str(path)) for _, path in spellings):
            raise ValueError("Linux build paths must allow unquoted C/C++ prefix-map flags")
        maps = " ".join(f"-ffile-prefix-map={source}=/{label}" for label, source in spellings)
    for language in ("CFLAGS", "CXXFLAGS"):
        name = f"{language}_{target_key}"
        env[name] = " ".join(value for value in (env.get(name), maps) if value)
    # --config appends these to the repository's MSVC +crt-static rustflags.
    # RUSTFLAGS would discard that required target policy. The flags go in a
    # file: Git Bash's argument parser turns the escaped backslashes of an
    # inline TOML value into invalid escapes. JSON strings are TOML strings.
    packages = ["broscribe-cli" if name == "broscribe" else name for name in BINARIES]
    with tempfile.TemporaryDirectory(prefix="broscribe-rustflags-") as scratch:
        config = Path(scratch) / "rustflags.toml"
        table = f"[target.{target}]\nrustflags = {json.dumps(flags)}\n"
        if linker:
            table += f"linker = {json.dumps(linker)}\n"
        config.write_text(table, encoding="utf-8")
        run(bash, "scripts/build-target.sh", target, "build", "--locked", "--release",
            "--config", config.as_posix(),
            *(arg for package in packages for arg in ("-p", package)), env=env)


def check_windows_imports(release_dir: Path) -> None:
    unshipped = re.compile(
        r"^\s+(?:sherpa[-_]onnx|onnxruntime|vcruntime|msvcp|msvcr|ucrtbase|api-ms-win-crt)"
        r"[^\s]*\.dll\s*$", re.IGNORECASE | re.MULTILINE)
    for name in BINARIES:
        imports = output("dumpbin.exe", "/DEPENDENTS", str(release_dir / (name + ".exe")))
        if match := unshipped.search(imports):
            raise ValueError(f"{name}.exe links an unshipped runtime: {match.group().strip()}")
        print(imports)


def pinned_library(target: str, bash: str) -> tuple[Path, str, str]:
    # Delegate pin and .verified interpretation to the same code as the linker wrapper.
    code = """
source third_party/sherpa-onnx/pins.sh
sherpa_check_lock Cargo.lock
target=$(sherpa_target_for_triple "$1")
sherpa_lib_verified "third_party/sherpa-onnx/lib-$target" "$target" || {
    echo "Missing pinned sherpa-onnx inputs for $target: run fetch.sh --target $target first." >&2
    exit 1
}
archive=$(sherpa_archive "$target")
printf '%s\n' "$archive"
sherpa_sha256 "$archive"
"""
    archive, checksum = output(bash, "-ec", code, "package-desktop", target).splitlines()
    return PINS / f"lib-{TARGETS[target][0]}", archive, checksum


def payload(release_dir: Path, notices_dir: Path, build: dict, linux: bool):
    files = {}
    for name in BINARIES:
        name = name if linux else name + ".exe"
        source = release_dir / name
        if not stat.S_ISREG(source.lstat().st_mode):
            raise ValueError(f"{name} must be a regular executable")
        files[name] = (source, 0o755)
    for name in ("LICENSE", "NOTICE"):
        files[name] = (REPO / name, 0o644)
    for name in LICENCES - {"LICENSE", "NOTICE", "build-info.json"}:
        contents = (notices_dir / name).read_text(encoding="utf-8")
        files[name] = (contents.encode(), 0o644)
    files["build-info.json"] = (json_bytes(build), 0o644)
    if linux:
        files[DESKTOP] = (SOURCE / Path(DESKTOP).name, 0o644)
        for script in ("install.sh", "uninstall.sh"):
            files[script] = (SOURCE / script, 0o755)
        for name, size in ICONS.items():
            image = "window-icon.png" if size == 128 else f"tray-idle-{size}.png"
            files[name] = (BRAND / image, 0o644)
    return files


def write_archive(artifact: Path, files: dict, linux: bool) -> tuple[int, int]:
    """Preserve content and intended modes with synthetic, stable archive metadata."""
    installed = 0
    with artifact.open("wb") as raw:
        if linux:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed, \
                    tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name, (source, mode) in sorted(files.items()):
                    info = tarfile.TarInfo(verifier.ROOT + name)
                    info.mode = mode
                    info.size = len(source) if isinstance(source, bytes) else source.stat().st_size
                    with (io.BytesIO(source) if isinstance(source, bytes) else source.open("rb")) as contents:
                        archive.addfile(info, contents)
                    installed += info.size
        else:
            with zipfile.ZipFile(raw, mode="w", compression=zipfile.ZIP_DEFLATED) as archive:
                for name, (source, mode) in sorted(files.items()):
                    info = zipfile.ZipInfo(verifier.ROOT + name)
                    info.create_system = 3
                    info.external_attr = (stat.S_IFREG | mode) << 16
                    info.compress_type = zipfile.ZIP_DEFLATED
                    with (io.BytesIO(source) if isinstance(source, bytes) else source.open("rb")) as contents:
                        with archive.open(info, "w") as member:
                            shutil.copyfileobj(contents, member, length=1 << 20)
                    installed += info.file_size
    return len(files), installed


def write_release(dist: Path, work: Path, build: dict, files: dict, forbid: list[str]) -> Path:
    suffix = TARGETS[build["target"]][1]
    linux = suffix.startswith("linux-")
    stem = f"BroScribe-{build['version']}-{build['build']}-{suffix}"
    artifact = work / (stem + (".tar.gz" if linux else ".zip"))
    provenance = work / (stem + ".provenance.json")
    sidecar = work / (artifact.name + ".sha256")
    for previous in sorted(dist.glob("*.provenance.json")):
        record = json_object(previous.read_bytes())
        if record["git"]["commit"] != build["git"]["commit"] and record["build"] >= build["build"]:
            raise ValueError(f"build number must increase over {previous.name}")
    entries, installed = write_archive(artifact, files, linux)
    record = artifact_record(artifact, build, files["third-party-inventory.json"][0], entries, installed)
    provenance.write_bytes(json_bytes(record))
    sidecar.write_bytes(f"{record['artifact']['sha256']}  {artifact.name}\n".encode())
    verifier.verify(artifact, provenance, forbid, target_dir=dist.parent)
    for path in (sidecar, provenance, artifact):
        path.replace(dist / path.name)
    return dist / artifact.name


def package(args) -> None:
    if args.target != next((line[6:] for line in output("rustc", "-vV").splitlines() if line.startswith("host: ")), ""):
        raise ValueError(f"{args.target} requires its native Rust host")
    if not output("cargo", "--version").startswith("cargo 1.100.0 "):
        raise ValueError("use the repository's pinned Cargo 1.100.0")
    commit = clean_commit()
    remaps = build_roots(args.target_dir)
    target_dir = remaps["__build_target__"].resolve()
    dist = target_dir / "dist"
    cwd_lock = REPO / "Cargo.lock"
    bash = find_bash(args.target.endswith("-windows-msvc"))
    lib_dir, archive, sha256 = pinned_library(args.target, bash)
    build = metadata.build_info(SimpleNamespace(
        version=tomllib.loads((REPO / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"],
        build=os.environ.get("BROSCRIBE_BUILD_NUMBER") or output("git", "rev-list", "--count", "HEAD"),
        target=args.target, profile="release", git_commit=commit, git_dirty="false",
        rustc=output("rustc", "--version"), cargo=output("cargo", "--version"), cargo_lock=cwd_lock,
        sherpa_archive=archive, sherpa_sha256=sha256,
        copyright=next(line for line in (REPO / "NOTICE").read_text().splitlines() if line.startswith("Copyright ")),
    ))
    release_dir = target_dir / args.target / "release"
    receipt = release_dir / ".broscribe-release-build.json"
    if not args.package_only:
        receipt.unlink(missing_ok=True)
        build_native(args.target, target_dir, remaps, bash)
    if clean_commit() != commit:
        raise ValueError("checkout changed during the release build")
    if args.package_only:
        if json_object(receipt.read_bytes()) != build:
            raise ValueError("native build receipt disagrees with this release")
    else:
        receipt.write_bytes(json_bytes(build))
    if args.build_only:
        return
    if args.target.endswith("-windows-msvc"):
        check_windows_imports(release_dir)
    dist.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".broscribe-package-", dir=dist) as tmp:
        work = Path(tmp)
        run(sys.executable, "scripts/third-party-notices.py", "--target", args.target, "--out-dir", str(work),
            "--native-lib-dir", str(lib_dir), env={**os.environ, "PYTHONUTF8": "1"})
        files = payload(release_dir, work, build, args.target.endswith("-linux-gnu"))
        if clean_commit() != commit:
            raise ValueError("checkout changed during licence generation")
        print(write_release(dist, work, build, files,
                            [*builder_paths(TARGETS[args.target][0], remaps), *args.forbid]))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=TARGETS)
    parser.add_argument("--target-dir", type=Path)
    parser.add_argument("--forbid", action="append", default=[])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--build-only", action="store_true", help="build first for optional Authenticode signing")
    mode.add_argument("--package-only", action="store_true", help="package outputs from --build-only")
    try:
        package(parser.parse_args())
    except (OSError, EOFError, ValueError, KeyError, TypeError, StopIteration,
            metadata.MetadataError, subprocess.CalledProcessError) as error:
        print(f"package-desktop: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

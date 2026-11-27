"""Shared layout and provenance for Linux and Windows release archives."""

from __future__ import annotations

from functools import cache
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import tomllib

SCRIPT = Path(__file__).absolute()
REPO = SCRIPT.resolve().parent.parent
APP_ID = "org.broscribe.BroScribe"
DESKTOP = f"share/applications/{APP_ID}.desktop"
ICONS = {f"share/icons/hicolor/{size}x{size}/apps/{APP_ID}.png": size for size in (22, 32, 64, 128)}
BINARIES = ("broscribe-app", "broscribe", "broscribed", "broscribe-worker", "broscribe-update-runner")
LICENCES = {"LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.txt", "third-party-inventory.json", "build-info.json"}
TARGETS = {
    "x86_64-unknown-linux-gnu": ("linux-x64", "linux-x86_64", 62),
    "aarch64-unknown-linux-gnu": ("linux-aarch64", "linux-aarch64", 183),
    "x86_64-pc-windows-msvc": ("win-x64", "windows-x64", 0x8664),
}


def release_files(linux: bool) -> set[str]:
    binaries = {name if linux else name + ".exe" for name in BINARIES}
    return binaries | LICENCES | ({"install.sh", "uninstall.sh", DESKTOP, *ICONS} if linux else set())


def sibling(name: str):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), REPO / "scripts" / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


sha256_file = sibling("release-metadata").sha256_file


def json_object(data: bytes) -> dict:
    value = json.loads(data)
    if not isinstance(value, dict):
        raise ValueError("expected a JSON object")
    return value


def json_bytes(value: dict) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def artifact_record(artifact: Path, build: dict, inventory: bytes, entries: int, installed: int) -> dict:
    git = build["git"]
    if not re.fullmatch("[0-9a-f]{40}", git["commit"]) or git.get("dirty") is not False:
        raise ValueError("release needs a clean git commit")
    if any(not re.fullmatch("[0-9a-f]{64}", digest)
           for digest in (build["cargo_lock_sha256"], build["sherpa_onnx"]["sha256"])):
        raise ValueError("invalid release input digest")
    return {
        "schema": 1,
        "artifact": {"name": artifact.name, "sha256": sha256_file(artifact),
                     "size": artifact.stat().st_size, "entries": entries, "installed_size": installed},
        **{key: build[key] for key in ("product", "version", "build", "target", "git", "toolchain",
                                       "cargo_lock_sha256", "sherpa_onnx")},
        "bundle_identifier": APP_ID,
        "third_party_inventory_sha256": hashlib.sha256(inventory).hexdigest(),
    }


@cache
def upstream_paths(native_target: str) -> tuple[str, ...]:
    with (REPO / "third_party/sherpa-onnx/UPSTREAM_PATHS.toml").open("rb") as source:
        return tuple(tomllib.load(source)[native_target])


def build_roots(target_dir: Path | None = None, repo_dir: Path | None = None) -> dict[str, Path]:
    cwd = Path.cwd()
    logical_cwd = Path(os.environ.get("PWD", cwd))
    if not logical_cwd.is_absolute() or logical_cwd.resolve() != cwd:
        logical_cwd = cwd
    if repo_dir is None:
        repo_dir = SCRIPT.parent.parent
        if repo_dir.resolve() != REPO:
            repo_dir = REPO
        # Python may expand a relative script argument against the physical
        # cwd. Recover the logical checkout spelling when the shell supplied it.
        if logical_cwd != cwd and cwd.is_relative_to(REPO):
            logical_repo = logical_cwd
            for _ in cwd.relative_to(REPO).parts:
                logical_repo = logical_repo.parent
            if logical_repo.resolve() == REPO:
                repo_dir = logical_repo
    repo_dir = logical_cwd / repo_dir
    home = logical_cwd / Path.home()
    roots = {
        "__build_home__": home,
        "__build_cargo__": logical_cwd / os.environ.get("CARGO_HOME", home / ".cargo"),
        "__build_repo__": repo_dir,
        "__build_target__": logical_cwd / (target_dir or os.environ.get("CARGO_TARGET_DIR", repo_dir / "target")),
    }
    if runner_temp := os.environ.get("RUNNER_TEMP"):
        roots["__build_temp__"] = logical_cwd / runner_temp
    return roots


def build_root_spellings(roots: dict[str, Path]) -> list[tuple[str, Path]]:
    """Return logical and physical prefixes, with more specific maps last."""
    entries = ((label, path) for label, root in roots.items()
               for path in dict.fromkeys((root, root.resolve() if isinstance(root, Path) else root)))
    return sorted(entries, key=lambda entry: len(str(entry[1])))


def builder_paths(native_target: str, roots: dict[str, Path]) -> list[str]:
    upstream = [comparable_path(prefix) for prefix in upstream_paths(native_target)]
    paths = []
    for label, path in build_root_spellings(roots):
        if label == "__build_home__" and any(
            prefix.startswith(comparable_path(str(path)).rstrip("/") + "/") for prefix in upstream
        ):
            continue
        paths.append(str(path))
    return paths


def windows_path(path: str) -> bool:
    return path.startswith("\\") or re.match(r"^[a-zA-Z]:[/\\]", path) is not None


def comparable_path(path: str) -> str:
    return path.replace("\\", "/").lower() if windows_path(path) else path


ENCODINGS = ("utf-8", "utf-16le")


def path_needles(paths):
    posix, windows = set(), set()
    for path in paths:
        if len(path) < 3:
            continue
        if windows_path(path):
            # PE and JSON can spell Windows paths with either separator or
            # escaped backslashes. PE resources can also be UTF-16LE.
            slash = comparable_path(path)
            backslash = slash.replace("/", "\\")
            windows.update((slash, backslash, backslash.replace("\\", "\\\\")))
        else:
            posix.add(path)
    return tuple(tuple(path.encode(encoding) for path in sorted(group) for encoding in ENCODINGS)
                 for group in (posix, windows))


def common_paths(encoding: str) -> re.Pattern[bytes]:
    def literal(value: str) -> bytes:
        return re.escape(value.encode(encoding))

    def either(*parts: bytes) -> bytes:
        return b"(?:" + b"|".join(parts) + b")"

    def component(excluded: bytes) -> bytes:
        unit = b"[^\x00-\x20\x7f" + re.escape(excluded + b"\"'") + b"]"
        if encoding == "utf-16le":
            unit = either(unit + b"\x00", b"[\\s\\S][^\x00]")
        return unit + b"{1,255}"

    # Every byte a component stops at ends it: control bytes, space, DEL
    # and quotes. A name cut short by any of them is still a whole name.
    stops = [chr(code) for code in (*range(0x21), 0x7f)] + ["\"", "'"]
    boundary = either(*(literal(char) for char in stops), rb"\Z")
    # POSIX is case-sensitive, so lowercase /users/ API routes pass.
    posix = either(*(literal(prefix) for prefix in (
        "/Users/", "/home/", "/private/var/folders/", "/__w/", "/root/",
        "/private/tmp/", "/private/var/tmp/",
    ))) + component(b"/") + either(literal("/"), boundary)
    backslash = either(literal("\\\\"), literal("\\"))
    separator = either(literal("/"), backslash)
    drive = b"[a-z]" + (b"\x00" if encoding == "utf-16le" else b"") + literal(":")
    windows = either(
        either(drive + separator, backslash) + literal("users") + separator,
        literal("d:") + separator + literal("a") + separator,
    ) + component(b"/\\") + either(separator, boundary)
    return re.compile(either(posix, b"(?i:" + windows + b")"))


COMMON_PATHS = tuple(common_paths(encoding) for encoding in ENCODINGS)


def forbidden_paths(paths, upstream=()):
    explicit_posix, explicit_windows = path_needles(paths)
    upstream_posix, upstream_windows = path_needles(upstream)

    def contains(data: bytes) -> bytes | None:
        """Return the first forbidden path found, or None."""
        # An exception can never override a specifically forbidden build root.
        for needle in explicit_posix:
            if needle in data:
                return needle
        if explicit_windows:
            folded = data.lower()
            for needle in explicit_windows:
                if needle in folded:
                    return needle
        for needle in upstream_posix:
            data = data.replace(needle, b"\0" * len(needle))
        for needle in upstream_windows:
            data = re.sub(re.escape(needle), b"\0" * len(needle), data, flags=re.IGNORECASE)
        for pattern in COMMON_PATHS:
            if match := pattern.search(data):
                return match.group()
        return None

    return contains

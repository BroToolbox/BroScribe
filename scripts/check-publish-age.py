#!/usr/bin/env python3
"""Fail if Cargo.lock contains a crates.io version that is too young or yanked.

Cargo 1.100+ enforces `registry.global-min-publish-age` while resolving, but it
keeps whatever is already in Cargo.lock, including versions yanked later. This
script checks every locked crates.io package against its publish time and yank
state in the crates.io sparse index, and rejects git and unknown sources.

For each problem it suggests a `cargo update --precise` version only when that
version is old enough, not yanked, a release (unless the locked version is a
prerelease itself), and satisfies every requirement the dependents in the lock
file place on the crate: the index requirements of locked registry dependents
and the manifests of the workspace members. Otherwise it says why none exists.

Usage:
  scripts/check-publish-age.py [Cargo.lock] [--min-age-days 3]
                               [--index-dir DIR [--offline]] [--now RFC3339]

  --index-dir DIR  store each fetched sparse-index file under DIR (same layout
                   as index.crates.io) for audit or later offline runs
  --offline        read the index only from --index-dir; never use the network
  --now            evaluate ages at this instant instead of the current time

Exit status: 0 all packages pass, 1 problems found, 2 usage, lock-file or
index errors. Needs Python 3.11+ (tomllib); online runs reach index.crates.io.
"""

from __future__ import annotations

import argparse
import glob
import json
import re
import sys
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    sys.exit("check-publish-age needs Python 3.11 or newer (tomllib)")

INDEX = "https://index.crates.io"
CRATES_IO_SOURCES = (
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
)
DEPENDENCY_TABLES = ("dependencies", "build-dependencies", "dev-dependencies")


class CheckError(Exception):
    """A lock-file, manifest or index problem that prevents a verdict (exit 2)."""


# ---------------------------------------------------------------- versions


@dataclass(frozen=True)
class Version:
    major: int
    minor: int
    patch: int
    pre: tuple = ()

    @classmethod
    def parse(cls, text: str) -> "Version":
        match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?", text.strip())
        if not match:
            raise CheckError(f"not a semantic version: {text!r}")
        pre = tuple(int(p) if p.isdigit() else p for p in match.group(4).split(".")) if match.group(4) else ()
        return cls(int(match.group(1)), int(match.group(2)), int(match.group(3)), pre)

    @property
    def core(self) -> tuple[int, int, int]:
        return (self.major, self.minor, self.patch)

    def key(self) -> tuple:
        """SemVer precedence: a release sorts after its prereleases; numeric
        prerelease identifiers sort before alphanumeric ones."""
        pre = tuple((0, p, "") if isinstance(p, int) else (1, 0, p) for p in self.pre)
        return (self.core, not self.pre, pre)

    def __lt__(self, other: "Version") -> bool:
        return self.key() < other.key()


def compat_key(version: Version) -> tuple:
    """Versions with the same key are semver-compatible (Cargo's caret rule)."""
    if version.major:
        return (version.major,)
    if version.minor:
        return (0, version.minor)
    return (0, 0, version.patch)


# ------------------------------------------------------------ requirements


@dataclass(frozen=True)
class Comparator:
    op: str
    major: int
    minor: int | None
    patch: int | None
    pre: tuple

    def matches(self, v: Version) -> bool:
        major, minor, patch = self.major, self.minor, self.patch
        if self.op == "=":
            return v.major == major and (minor is None or v.minor == minor) and (
                patch is None or (v.patch == patch and v.pre == self.pre))
        if self.op in (">", ">=", "<", "<="):
            if minor is None or patch is None:
                lower = Version(major, minor or 0, 0)
                upper = Version(major + 1, 0, 0) if minor is None else Version(major, minor + 1, 0)
                return {
                    ">": not v < upper,
                    ">=": not v < lower,
                    "<": v.core < lower.core,
                    "<=": v.core < upper.core,
                }[self.op]
            bound = Version(major, minor, patch, self.pre)
            if self.op == ">":
                return bound < v
            if self.op == ">=":
                return not v < bound
            if self.op == "<":
                return v < bound
            return not bound < v
        if self.op == "~":
            if minor is None:
                return v.major == major
            return v.core[:2] == (major, minor) and (patch is None or not v < Version(major, minor, patch, self.pre))
        # caret
        if minor is None:
            return v.major == major
        lower = Version(major, minor, patch or 0, self.pre)
        if v < lower:
            return False
        if major:
            return v.major == major
        if minor or patch is None:
            return (v.major, v.minor) == (0, minor)
        return v.core == (0, 0, patch)


@dataclass(frozen=True)
class Requirement:
    text: str
    comparators: tuple

    @classmethod
    def parse(cls, text: str) -> "Requirement":
        comparators = []
        for part in text.split(","):
            part = part.strip()
            if part in ("*", ""):
                continue
            match = re.fullmatch(
                r"(\^|~|=|>=|<=|>|<)?\s*(\d+)(?:\.(\d+|\*|x|X))?(?:\.(\d+|\*|x|X))?(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?",
                part,
            )
            if not match:
                raise CheckError(f"unsupported version requirement {text!r}")
            op = match.group(1) or "^"
            wild = lambda g: None if g is None or g in ("*", "x", "X") else int(g)  # noqa: E731
            minor, patch = wild(match.group(3)), wild(match.group(4))
            if minor is None:
                patch = None
            if match.group(3) in ("*", "x", "X") or match.group(4) in ("*", "x", "X"):
                op = "=" if op == "^" else op
            pre = tuple(int(p) if p.isdigit() else p for p in match.group(5).split(".")) if match.group(5) else ()
            comparators.append(Comparator(op, int(match.group(2)), minor, patch, pre))
        return cls(text, tuple(comparators))

    def matches(self, version: Version) -> bool:
        if not all(c.matches(version) for c in self.comparators):
            return False
        # A prerelease satisfies a requirement only when some comparator names
        # the same major.minor.patch with a prerelease (Cargo's rule).
        return not version.pre or any(
            c.pre and (c.major, c.minor, c.patch) == version.core for c in self.comparators)


# ------------------------------------------------------------------- index


def index_path(name: str) -> str:
    name = name.lower()
    if len(name) <= 2:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


class Index:
    """Sparse-index reader: the network (optionally saving files) or a local copy."""

    def __init__(self, directory: Path | None, offline: bool):
        if offline and directory is None:
            raise CheckError("--offline needs --index-dir")
        self.directory = directory
        self.offline = offline

    def _download(self, name: str) -> str:
        request = urllib.request.Request(
            f"{INDEX}/{index_path(name)}", headers={"User-Agent": "broscribe-check-publish-age"})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return response.read().decode()
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return ""
            raise CheckError(f"index.crates.io returned HTTP {error.code} for {name}") from None
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise CheckError(f"cannot read the crates.io index for {name}: {error}") from None

    def versions(self, name: str) -> list[dict]:
        local = self.directory / index_path(name) if self.directory else None
        if self.offline:
            if not local.is_file():
                raise CheckError(f"offline: no index file for {name} at {local}")
            text = local.read_text()
        else:
            text = self._download(name)
            if local is not None:
                local.parent.mkdir(parents=True, exist_ok=True)
                local.write_text(text)
        try:
            return [json.loads(line) for line in text.splitlines() if line.strip()]
        except json.JSONDecodeError as error:
            raise CheckError(f"malformed index file for {name}: {error}") from None


def published(entry: dict) -> datetime | None:
    # Versions published before the index recorded `pubtime` have none; they are old.
    stamp = entry.get("pubtime")
    return datetime.fromisoformat(stamp.replace("Z", "+00:00")) if stamp else None


# ------------------------------------------------------------ dependents


def lock_dependency_target(spec: str, versions_by_name: dict) -> tuple[str, str] | None:
    """The (name, version) a lock-file dependency string refers to."""
    parts = spec.split(" ")
    name = parts[0]
    if len(parts) >= 2:
        return name, parts[1]
    versions = versions_by_name.get(name, [])
    return (name, versions[0]) if len(versions) == 1 else None


def member_requirements(root: Path) -> dict[str, list[tuple[str, str]]]:
    """Requirements the workspace manifests place on each crate: name -> [(member, req)]."""
    manifest = root / "Cargo.toml"
    if not manifest.is_file():
        return {}
    try:
        workspace_toml = tomllib.loads(manifest.read_text())
    except tomllib.TOMLDecodeError as error:
        raise CheckError(f"{manifest}: {error}") from None
    workspace = workspace_toml.get("workspace", {})
    shared = workspace.get("dependencies", {})
    manifests = [manifest] if "package" in workspace_toml else []
    for pattern in workspace.get("members", []):
        manifests += sorted(Path(p) / "Cargo.toml" for p in glob.glob(str(root / pattern)))
    out: dict[str, list[tuple[str, str]]] = {}

    def add(member: str, key: str, spec) -> None:
        if isinstance(spec, dict) and spec.get("workspace"):
            spec = shared.get(key, {})
        if isinstance(spec, str):
            spec = {"version": spec}
        if not isinstance(spec, dict) or "version" not in spec:
            return  # path-only or git dependency
        out.setdefault(spec.get("package", key), []).append((member, spec["version"]))

    for path in manifests:
        if not path.is_file():
            continue
        data = tomllib.loads(path.read_text())
        member = data.get("package", {}).get("name", str(path.parent))
        tables = [data] + list(data.get("target", {}).values())
        for table in tables:
            for kind in DEPENDENCY_TABLES:
                for key, spec in table.get(kind, {}).items():
                    add(member, key, spec)
    return out


def dependent_requirements(name: str, version: str, packages: list[dict], index: dict,
                           versions_by_name: dict, members: dict) -> list[tuple[str, Requirement]]:
    """Requirements on `name` from every locked package that resolves to `version`."""
    locked = Version.parse(version)
    found: list[tuple[str, Requirement]] = []
    for pkg in packages:
        targets = [lock_dependency_target(d, versions_by_name) for d in pkg.get("dependencies", [])]
        if (name, version) not in targets:
            continue
        label = f"{pkg['name']} {pkg['version']}"
        if pkg.get("source") is None:
            reqs = [Requirement.parse(r) for m, r in members.get(name, []) if m == pkg["name"]]
        else:
            entries = {e["vers"]: e for e in index.get(pkg["name"], [])}
            deps = entries.get(pkg["version"], {}).get("deps", [])
            reqs = [Requirement.parse(d["req"]) for d in deps
                    if d.get("package", d["name"]) == name and d.get("kind") != "dev"]
        # Several declarations (targets, kinds) may name the crate; the ones the
        # lock resolved to this version are the ones that constrain it.
        for req in reqs:
            if req.matches(locked):
                found.append((label, req))
    return found


# ------------------------------------------------------------------- check


def suggestion(name: str, version: str, entries: dict, cutoff: datetime,
               requirements: list[tuple[str, Requirement]], yanked: bool) -> str:
    locked = Version.parse(version)
    candidates = []
    for text, entry in entries.items():
        if entry.get("yanked") or text == version:
            continue
        when = published(entry)
        if when is not None and when > cutoff:
            continue
        try:
            candidate = Version.parse(text)
        except CheckError:
            continue
        if candidate.pre and not locked.pre:
            continue
        if not yanked and not candidate < locked:
            continue
        if compat_key(candidate) != compat_key(locked):
            continue
        if all(req.matches(candidate) for _, req in requirements):
            candidates.append(candidate)
    if candidates:
        best = max(candidates, key=Version.key)
        precise = ".".join(map(str, best.core)) + ("-" + ".".join(map(str, best.pre)) if best.pre else "")
        return f"; try: cargo update -p {name}@{version} --precise {precise}"
    detail = ", ".join(f"{who} needs {req.text}" for who, req in requirements)
    reason = f" ({detail})" if detail else ""
    return f"; no old-enough, unyanked release satisfies its dependents{reason}; pin the dependent crate lower"


def check(lockfile: Path, min_age_days: float, now: datetime, index: Index) -> tuple[list[str], int]:
    try:
        lock = tomllib.loads(lockfile.read_text())
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise CheckError(f"cannot read {lockfile}: {error}") from None
    packages = lock.get("package", [])
    cutoff = now - timedelta(days=min_age_days)

    problems: list[str] = []
    registry = []
    for pkg in packages:
        source = pkg.get("source")
        if source is None:
            continue  # workspace member or path dependency
        if source.startswith("git+"):
            problems.append(f"{pkg['name']} {pkg['version']}: git dependency ({source}); not allowed")
        elif source.startswith(CRATES_IO_SOURCES):
            registry.append(pkg)
        else:
            problems.append(f"{pkg['name']} {pkg['version']}: unknown source {source}")

    names = sorted({pkg["name"] for pkg in registry})
    with ThreadPoolExecutor(max_workers=16) as pool:
        index_entries = dict(zip(names, pool.map(index.versions, names)))

    versions_by_name: dict[str, list[str]] = {}
    for pkg in packages:
        versions_by_name.setdefault(pkg["name"], []).append(pkg["version"])
    members = member_requirements(lockfile.resolve().parent)

    for pkg in registry:
        name, version = pkg["name"], pkg["version"]
        entries = {e["vers"]: e for e in index_entries[name]}
        entry = entries.get(version)
        if entry is None:
            problems.append(f"{name} {version}: not found in the crates.io index")
            continue
        when = published(entry)
        too_new = when is not None and when > cutoff
        if not entry.get("yanked") and not too_new:
            continue
        requirements = dependent_requirements(name, version, packages, index_entries, versions_by_name, members)
        if entry.get("yanked"):
            problems.append(f"{name} {version}: yanked from crates.io"
                            + suggestion(name, version, entries, cutoff, requirements, yanked=True))
        else:
            problems.append(f"{name} {version}: published {when:%Y-%m-%d %H:%M} UTC, younger than "
                            f"{min_age_days:g} days"
                            + suggestion(name, version, entries, cutoff, requirements, yanked=False))
    return problems, len(registry)


def parse_now(text: str | None) -> datetime:
    if text is None:
        return datetime.now(timezone.utc)
    try:
        value = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        raise CheckError(f"--now is not an RFC 3339 time: {text!r}") from None
    if value.tzinfo is None:
        raise CheckError("--now needs a time zone (e.g. 2026-11-17T00:00:00Z)")
    return value


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("lockfile", nargs="?", type=Path, default=Path("Cargo.lock"))
    parser.add_argument("--min-age-days", type=float, default=3.0)
    parser.add_argument("--index-dir", type=Path)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--now")
    args = parser.parse_args(argv)
    if args.min_age_days < 0:
        parser.error("--min-age-days must not be negative")

    try:
        problems, checked = check(args.lockfile, args.min_age_days, parse_now(args.now),
                                  Index(args.index_dir, args.offline))
    except CheckError as error:
        print(f"check-publish-age: {error}", file=sys.stderr)
        return 2
    if problems:
        print(f"{len(problems)} problem(s) in {args.lockfile} ({checked} crates.io packages checked):")
        for line in problems:
            print(f"  {line}")
        return 1
    print(f"ok: {checked} crates.io packages, all at least {args.min_age_days:g} days old and not yanked")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

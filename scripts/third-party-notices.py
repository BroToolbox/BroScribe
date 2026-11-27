#!/usr/bin/env python3
"""Generate THIRD_PARTY_NOTICES.txt and third-party-inventory.json for a build.

The inventory covers what a BroScribe build actually distributes for one Rust
target: the crates.io packages linked into the shipped executables (normal
dependencies of the binary packages for that target, excluding build scripts,
proc macros and dev-dependencies), the prebuilt native static libraries
sherpa-onnx-sys links, and the third-party files compiled into the app (the
bundled fonts, crates/broscribe-app/assets/ASSET_LICENSES.toml). It never
invents a licence or a licence text:

  * a package whose licence is missing, unparsable or not allowed by the policy
    (scripts/third-party-licenses.toml) fails the run;
  * a package or native library whose licence text cannot be found (the crate
    does not ship it and no reviewed text exists) is recorded as unresolved.
    With --allow-incomplete the files are still written, marked incomplete, for
    local builds; without it the run fails. Distribution refuses incomplete
    inventories (scripts/notarize-macos.sh).

Apache-2.0 obligations can be met with the standard licence text (the
repository's LICENSE) plus any NOTICE files the package ships; every other
licence needs the text the package ships or a reviewed [[text]] entry.

Usage:
  scripts/third-party-notices.py --target <rust-triple> --out-dir DIR
      [--metadata cargo-metadata.json] [--native-lib-dir DIR] [--allow-incomplete]
      [--asset-manifest ASSET_LICENSES.toml]

Without --metadata it runs `cargo metadata --locked --offline` for the target.
Exit status: 0 written (complete, or incomplete with --allow-incomplete),
1 policy failure, 2 usage or input error.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from itertools import product
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    sys.exit("third-party-notices needs Python 3.11 or newer (tomllib)")

REPO = Path(__file__).resolve().parent.parent
POLICY = REPO / "scripts" / "third-party-licenses.toml"
NATIVE_MANIFEST = REPO / "third_party" / "sherpa-onnx" / "NATIVE_LICENSES.toml"
ASSET_MANIFEST = REPO / "crates" / "broscribe-app" / "assets" / "ASSET_LICENSES.toml"
SHIPPED_PACKAGES = ("broscribe-app", "broscribe-cli", "broscribed", "broscribe-worker", "broscribe-update-runner")
NATIVE_SYS_CRATE = "sherpa-onnx-sys"
MAX_TEXT_BYTES = 1 << 20

LICENSE_NAME = re.compile(r"^(licen[cs]e|copying|notice|copyright|unlicense|third[-_]?party[-_]?notices?)"
                          r"([-_.][a-z0-9._-]*)?$", re.IGNORECASE)
CODE_SUFFIXES = {".rs", ".toml", ".py", ".c", ".h", ".cc", ".cpp", ".hpp", ".js", ".ts", ".json",
                 ".html", ".css", ".sh", ".yml", ".yaml", ".in", ".cmake", ".mk", ".go", ".java"}
SKIP_DIRS = {".git", "target", ".github"}

# Phrases that identify a licence text, used to match shipped files to the
# chosen licence when a package ships several files.
SIGNATURES = {
    "Apache-2.0": ("Apache License", "Version 2.0"),
    "Apache-2.0 WITH LLVM-exception": ("Apache License", "Version 2.0"),
    "MIT": ("Permission is hereby granted, free of charge",),
    "BSD-2-Clause": ("Redistribution and use in source and binary forms",),
    "BSD-3-Clause": ("Redistribution and use in source and binary forms", "endorse or promote"),
    "ISC": ("Permission to use, copy, modify, and/or distribute this software",),
    "Zlib": ("altered source versions must be plainly marked",),
    "BSL-1.0": ("Boost Software License",),
    "MPL-2.0": ("Mozilla Public License", "2.0"),
    "Unicode-3.0": ("UNICODE LICENSE",),
    "Unicode-DFS-2016": ("UNICODE", "DATA FILES AND SOFTWARE"),
    "NCSA": ("University of Illinois/NCSA Open Source License",),
    "CDLA-Permissive-2.0": ("Community Data License Agreement",),
    "bzip2-1.0.6": ('"bzip2", the associated library "libbzip2", and all documentation',),
}


class InputError(Exception):
    """Unusable input (exit 2)."""


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ------------------------------------------------------------ SPDX expressions


def tokenize(expression: str) -> list[str]:
    # Legacy crates.io syntax "MIT/Apache-2.0" means "MIT OR Apache-2.0".
    expression = re.sub(r"\s*/\s*", " OR ", expression.strip())
    return re.findall(r"\(|\)|[^\s()]+", expression)


def parse_expression(expression: str) -> list[tuple[str, ...]]:
    """The OR-alternatives of an SPDX expression, each the licences it combines."""
    tokens = tokenize(expression)
    position = 0

    def peek():
        return tokens[position] if position < len(tokens) else None

    def take():
        nonlocal position
        if position >= len(tokens):
            raise ValueError("unexpected end")
        position += 1
        return tokens[position - 1]

    def term():
        token = peek()
        if token is None:
            raise ValueError("unexpected end")
        if token == "(":
            take()
            alternatives = or_expr()
            if take() != ")":
                raise ValueError("missing )")
            return alternatives
        if token in (")", "AND", "OR", "WITH"):
            raise ValueError(f"unexpected {token}")
        license_id = take()
        if peek() == "WITH":
            take()
            exception = peek()
            if exception is None or exception in ("(", ")", "AND", "OR", "WITH"):
                raise ValueError("WITH needs an exception")
            license_id = f"{license_id} WITH {take()}"
        return [(license_id,)]

    def and_expr():
        alternatives = term()
        while peek() == "AND":
            take()
            right = term()
            alternatives = [tuple(sorted(set(a + b))) for a, b in product(alternatives, right)]
        return alternatives

    def or_expr():
        alternatives = and_expr()
        while peek() == "OR":
            take()
            alternatives += and_expr()
        return alternatives

    if not tokens:
        raise ValueError("empty expression")
    result = or_expr()
    if position != len(tokens):
        raise ValueError(f"unexpected {tokens[position]}")
    return list(dict.fromkeys(result))


# ------------------------------------------------------------------- policy


@dataclass
class Policy:
    allowed: set
    no_notice: set
    preference: list
    overrides: dict
    texts: dict
    root: Path

    @classmethod
    def load(cls, path: Path) -> "Policy":
        try:
            data = tomllib.loads(path.read_text())
        except (OSError, tomllib.TOMLDecodeError) as error:
            raise InputError(f"cannot read licence policy {path}: {error}") from None
        overrides = {(o["name"], o["version"]): o for o in data.get("override", [])}
        texts: dict = {}
        for entry in data.get("text", []):
            texts.setdefault((entry["name"], entry["version"]), []).append(entry)
        return cls(set(data.get("allowed", [])), set(data.get("no_notice", [])),
                   list(data.get("preference", [])), overrides, texts, path.parent)

    def rank(self, alternative: tuple[str, ...]) -> tuple:
        order = [self.preference.index(x) if x in self.preference else len(self.preference) for x in alternative]
        return (len(alternative), max(order), alternative)


# ---------------------------------------------------------------- packages


@dataclass
class TextFile:
    relative: str
    text: str
    sha256: str


@dataclass
class Result:
    name: str
    version: str
    source: str
    declared: str | None
    chosen: tuple = ()
    files: list = field(default_factory=list)
    standard_apache: bool = False
    status: str = "ok"
    detail: str = ""
    reviewed: str | None = None
    text_evidence: list = field(default_factory=list)


def shipped_text_files(root: Path) -> list[TextFile]:
    found = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if any(part in SKIP_DIRS for part in relative.parts[:-1]) or not path.is_file():
            continue
        in_licenses_dir = any(p.lower() in ("licenses", "licences", "license", "licence") for p in relative.parts[:-1])
        if not (LICENSE_NAME.match(path.name) or in_licenses_dir) or path.suffix.lower() in CODE_SUFFIXES:
            continue
        data = path.read_bytes()
        if len(data) > MAX_TEXT_BYTES:
            continue
        found.append(TextFile(relative.as_posix(), data.decode("utf-8", errors="replace"), sha256_bytes(data)))
    return found


def matches_signature(license_id: str, text: str) -> bool:
    phrases = SIGNATURES.get(license_id)
    if not phrases:
        return False
    folded = " ".join(text.split()).lower()
    if license_id == "BSD-2-Clause" and "endorse or promote" in folded:
        return False
    return all(" ".join(p.split()).lower() in folded for p in phrases)


def is_primary(text_file: TextFile) -> bool:
    """A licence file of the package itself (top level, a top-level licences
    folder, or a reviewed text), not one of vendored code deeper inside it."""
    parts = text_file.relative.split("/")
    return (text_file.relative.startswith("(reviewed) ") or len(parts) == 1
            or (len(parts) == 2 and parts[0].lower() in ("licenses", "licences", "license", "licence")))


def coverage(alternative: tuple[str, ...], files: list[TextFile], policy: Policy) -> tuple[bool, bool]:
    """(covered, needs the standard Apache-2.0 text) for one alternative."""
    primary = [f for f in files if is_primary(f)]
    needing = [x for x in alternative if x not in policy.no_notice]
    unmatched_ids = [x for x in needing if not any(matches_signature(x, f.text) for f in primary)]
    attribution_names = ("notice", "copyright", "authors")
    unrecognised = [f for f in primary
                    if not any(matches_signature(x, f.text) for x in SIGNATURES)
                    and not Path(f.relative).name.lower().startswith(attribution_names)]
    # One declared licence and one unrecognised licence file: that file is it.
    if len(unmatched_ids) == 1 and len(needing) == 1 and len(unrecognised) == 1:
        unmatched_ids = []
    standard = False
    if unmatched_ids == ["Apache-2.0"]:
        unmatched_ids, standard = [], True
    return not unmatched_ids, standard


def evaluate(package: dict, policy: Policy) -> Result:
    name, version = package["name"], package["version"]
    root = Path(package["manifest_path"]).parent
    result = Result(name, version, package.get("source") or f"path+{name}@{version}", package.get("license"))
    files = shipped_text_files(root)
    declared = package.get("license")

    override = policy.overrides.get((name, version))
    if override:
        target = root / override["file"]
        if not target.is_file() or sha256_bytes(target.read_bytes()) != override["sha256"]:
            result.status, result.detail = "error", (
                f"reviewed override expects {override['file']} with SHA-256 {override['sha256']}; "
                "the file changed or is missing, review it again")
            return result
        declared, result.reviewed = override["license"], override.get("evidence", "")
    elif name in {key[0] for key in policy.overrides}:
        result.status, result.detail = "error", "a reviewed override exists for another version; review this version"
        return result

    for entry in policy.texts.get((name, version), []):
        path = policy.root / entry["file"]
        if not path.is_file() or sha256_bytes(path.read_bytes()) != entry["sha256"]:
            result.status, result.detail = "error", f"reviewed text {entry['file']} is missing or changed"
            return result
        data = path.read_bytes()
        files.append(TextFile(f"(reviewed) {entry['file']}", data.decode("utf-8", errors="replace"),
                              sha256_bytes(data)))
        result.text_evidence.append(entry.get("evidence", ""))

    result.files = files
    if not declared:
        result.status, result.detail = "error", (
            "no SPDX licence in the package metadata; add a reviewed [[override]] after reading its licence file")
        return result
    try:
        alternatives = parse_expression(declared)
    except ValueError as error:
        result.status, result.detail = "error", f"unparsable licence expression {declared!r}: {error}"
        return result
    allowed = [a for a in alternatives if all(x in policy.allowed for x in a)]
    if not allowed:
        result.status, result.detail = "error", f"licence {declared!r} is not allowed by the policy"
        return result
    allowed.sort(key=policy.rank)
    for alternative in allowed:
        covered, standard = coverage(alternative, files, policy)
        if covered:
            result.chosen, result.standard_apache = alternative, standard
            return result
    result.chosen = allowed[0]
    result.status, result.detail = "missing-text", (
        f"the package ships no licence text for {' AND '.join(allowed[0])}; add a reviewed [[text]] entry")
    return result


# ------------------------------------------------------------ dependency graph


def distributed_packages(metadata: dict) -> tuple[list[dict], list[dict]]:
    """(linked third-party packages, excluded build-time packages) of the shipped binaries."""
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    roots = [pid for pid in members if packages[pid]["name"] in SHIPPED_PACKAGES]
    missing = set(SHIPPED_PACKAGES) - {packages[pid]["name"] for pid in roots}
    if missing:
        raise InputError(f"cargo metadata has no workspace package {', '.join(sorted(missing))}")

    def is_proc_macro(pid: str) -> bool:
        return any("proc-macro" in t.get("kind", []) for t in packages[pid].get("targets", []))

    linked, excluded, seen, stack = set(), set(), set(), list(roots)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid].get("deps", []):
            if not any(k.get("kind") is None for k in dep.get("dep_kinds", [])):
                continue  # build-script or dev-only edge
            target = dep["pkg"]
            if is_proc_macro(target):
                excluded.add(target)
                continue
            linked.add(target)
            stack.append(target)
    third_party = [packages[p] for p in sorted(linked) if p not in members]
    build_time = [packages[p] for p in sorted(excluded - linked) if p not in members]
    return third_party, build_time


def linked_static_libs(sys_package: dict) -> list[str]:
    build = Path(sys_package["manifest_path"]).parent / "build.rs"
    try:
        source = build.read_text()
    except OSError as error:
        raise InputError(f"cannot read {build}: {error}") from None
    match = re.search(r"SHERPA_ONNX_STATIC_LIBS\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\];", source, re.S)
    if not match:
        raise InputError(f"{build} no longer lists SHERPA_ONNX_STATIC_LIBS; review the native libraries it links")
    return re.findall(r'"([^"]+)"', match.group(1))


def unpacked_archive(lib_dir: Path | None) -> str | None:
    """The archive a verified library folder was unpacked from (its .verified marker)."""
    if lib_dir is None:
        return None
    marker = lib_dir / ".verified"
    try:
        lines = marker.read_text().splitlines()
    except OSError:
        raise InputError(f"{marker} is missing; unpack the libraries with third_party/sherpa-onnx/fetch.sh") from None
    archives = [line.removeprefix("archive=") for line in lines if line.startswith("archive=")]
    if len(archives) != 1:
        raise InputError(f"{marker} does not name exactly one archive")
    return archives[0]


def native_components(libraries: list[str], manifest: Path, lib_dir: Path | None, triple: str,
                      policy: Policy) -> list[dict]:
    try:
        data = tomllib.loads(manifest.read_text())
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise InputError(f"cannot read {manifest}: {error}") from None
    by_library = {c["library"]: c for c in data.get("component", [])}
    archive = unpacked_archive(lib_dir)
    out = []
    for library in libraries:
        component = by_library.get(library)
        record = {"library": library, "archive": archive}
        if lib_dir is not None:
            file_name = f"{library}.lib" if "windows" in triple else f"lib{library}.a"
            path = lib_dir / file_name
            if not path.is_file():
                raise InputError(f"linked native library {file_name} is missing from {lib_dir}")
            record["file"] = file_name
            record["sha256"] = sha256_bytes(path.read_bytes())
        if component is None:
            record.update(status="error", detail="linked native library has no entry in NATIVE_LICENSES.toml")
            out.append(record)
            continue
        record["project"] = component.get("project", "")
        texts = []
        problem = None
        for text in component.get("texts", []):
            path = manifest.parent / text["file"]
            if not path.is_file() or sha256_bytes(path.read_bytes()) != text["sha256"]:
                problem = f"reviewed text {text['file']} is missing or changed"
                break
            data = path.read_bytes()
            texts.append(TextFile(f"(native) {text['file']}", data.decode("utf-8", errors="replace"),
                                  sha256_bytes(data)))
        if problem:
            record.update(status="error", detail=problem)
        elif not component.get("license") or not texts:
            record.update(status="missing-text",
                          detail="native licence not reviewed: set license, evidence and pinned texts")
        elif archive is None or archive not in component.get("verified_archives", []):
            record.update(status="missing-text",
                          detail=f"native licence reviewed only for {component.get('verified_archives', [])}, "
                                 f"not for {archive or 'an unknown archive'}")
        else:
            try:
                alternatives = parse_expression(component["license"])
            except ValueError as error:
                alternatives, problem = [], f"unparsable licence {component['license']!r}: {error}"
            if not any(all(x in policy.allowed for x in a) for a in alternatives):
                record.update(status="error",
                              detail=problem or f"licence {component['license']!r} is not allowed by the policy")
            else:
                record.update(status="ok", license=component["license"], evidence=component.get("evidence", ""))
        record["_texts"] = texts
        out.append(record)
    return out


def embedded_assets(manifest: Path, policy: Policy) -> list[dict]:
    """Third-party files compiled into the app (include_bytes!), each pinned by
    SHA-256 together with its reviewed licence texts. A changed file or text,
    a missing licence or one the policy does not allow fails the run: these
    files are listed by hand, so there is nothing to leave unresolved."""
    try:
        data = tomllib.loads(manifest.read_text())
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise InputError(f"cannot read {manifest}: {error}") from None
    out = []
    for asset in data.get("asset", []):
        name = asset.get("file", "")
        record = {"file": name, "project": asset.get("project", ""), "source": asset.get("source", "")}
        path = manifest.parent / name
        problem = None
        if not name or not path.is_file() or sha256_bytes(path.read_bytes()) != asset.get("sha256"):
            problem = f"embedded file {name or '(unnamed)'} is missing or changed"
        texts = []
        for text in asset.get("texts", []) if problem is None else []:
            text_path = manifest.parent / text["file"]
            if not text_path.is_file() or sha256_bytes(text_path.read_bytes()) != text["sha256"]:
                problem = f"reviewed text {text['file']} is missing or changed"
                break
            raw = text_path.read_bytes()
            texts.append(TextFile(f"(embedded) {text['file']}", raw.decode("utf-8", errors="replace"),
                                  sha256_bytes(raw)))
        if problem is None and (not asset.get("license") or not texts):
            problem = "embedded file needs a reviewed licence and its text"
        if problem is None:
            try:
                alternatives = parse_expression(asset["license"])
            except ValueError as error:
                alternatives, problem = [], f"unparsable licence {asset['license']!r}: {error}"
            if problem is None and not any(all(x in policy.allowed for x in a) for a in alternatives):
                problem = f"licence {asset['license']!r} is not allowed by the policy"
        if problem:
            record.update(status="error", detail=problem)
        else:
            record.update(status="ok", license=asset["license"], sha256=asset["sha256"])
        record["_texts"] = texts
        out.append(record)
    return out


# ------------------------------------------------------------------ output

CRATES_IO_SOURCES = ("registry+https://github.com/rust-lang/crates.io-index", "sparse+https://index.crates.io/")


def source_url(result: Result) -> str:
    if result.source.startswith(CRATES_IO_SOURCES):
        return f"https://crates.io/crates/{result.name}/{result.version}"
    return result.source


def render_notices(results: list[Result], natives: list[dict], apache_text: str, complete: bool,
                   triple: str, assets: list[dict] = ()) -> str:
    texts: dict[str, tuple[str, list[str]]] = {}

    def use(text_file: TextFile, who: str) -> None:
        texts.setdefault(text_file.sha256, (text_file.text, []))[1].append(who)

    lines = [
        "BroScribe third-party notices",
        f"Target: {triple}",
        "",
        "BroScribe is licensed under the Apache License, Version 2.0 (LICENSE, NOTICE).",
        "It includes the third-party components below, under the licences listed.",
    ]
    if not complete:
        lines += ["", "INCOMPLETE: some licence texts are unresolved (see third-party-inventory.json).",
                  "This build is for local use only and must not be distributed."]
    lines += ["", "Components", "----------"]
    for r in results:
        licence = " AND ".join(r.chosen) if r.chosen else "unresolved"
        declared = f" (declared: {r.declared})" if r.declared and r.chosen and " AND ".join(r.chosen) != r.declared else ""
        lines.append(f"{r.name} {r.version}: {licence}{declared}")
        # The exact source of every crate, which also meets MPL-2.0's
        # obligation to say where the Source Code Form is available.
        lines.append(f"    source: {source_url(r)}")
        for evidence in r.text_evidence:
            lines.append(f"    licence text: {evidence}")
        for f in r.files:
            use(f, f"{r.name} {r.version} ({f.relative})")
        if r.standard_apache:
            use(TextFile("LICENSE", apache_text, sha256_bytes(apache_text.encode())), f"{r.name} {r.version}")
    for n in natives:
        lines.append(f"{n['library']} (native static library): {n.get('license', 'unresolved')}")
        if n.get("project"):
            lines.append(f"    component: {n['project']}")
        for f in n.get("_texts", []):
            use(f, f"{n['library']} ({f.relative})")
    for a in assets:
        file_name = Path(a["file"]).name
        lines.append(f"{file_name} (embedded file): {a.get('license', 'unresolved')}")
        if a.get("project"):
            lines.append(f"    component: {a['project']}")
        if a.get("source"):
            lines.append(f"    source: {a['source']}")
        for f in a.get("_texts", []):
            use(f, f"{file_name} ({f.relative})")
    lines += ["", "Licence and notice texts", "------------------------"]
    for sha, (text, users) in sorted(texts.items(), key=lambda item: sorted(item[1][1])[0]):
        lines += ["", "=" * 78, "Used by:"]
        lines += [f"  {u}" for u in sorted(set(users))]
        lines += ["-" * 78, text.rstrip(), ""]
    return "\n".join(lines).rstrip() + "\n"


def run_cargo_metadata(triple: str) -> dict:
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", triple]
    try:
        output = subprocess.run(command, cwd=REPO, check=True, capture_output=True, text=True).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", "") or str(error)
        raise InputError(f"cargo metadata failed: {detail.strip()}") from None
    return json.loads(output)


def generate(metadata: dict, triple: str, out_dir: Path, policy: Policy, native_manifest: Path,
             native_lib_dir: Path | None, allow_incomplete: bool, apache_text: str,
             asset_manifest: Path | None = None) -> tuple[int, list[str]]:
    third_party, build_time = distributed_packages(metadata)
    results = sorted((evaluate(p, policy) for p in third_party), key=lambda r: (r.name, r.version))
    sys_package = next((p for p in third_party if p["name"] == NATIVE_SYS_CRATE), None)
    natives = native_components(linked_static_libs(sys_package), native_manifest, native_lib_dir, triple,
                                policy) if sys_package else []

    errors = [f"{r.name} {r.version}: {r.detail}" for r in results if r.status == "error"]
    errors += [f"native {n['library']}: {n['detail']}" for n in natives if n["status"] == "error"]
    assets = embedded_assets(asset_manifest, policy) if asset_manifest else []
    errors += [f"embedded {a['file']}: {a['detail']}" for a in assets if a["status"] == "error"]
    unresolved = [f"{r.name} {r.version}: {r.detail}" for r in results if r.status == "missing-text"]
    unresolved += [f"native {n['library']}: {n['detail']}" for n in natives if n["status"] == "missing-text"]
    if errors:
        return 1, errors + unresolved
    complete = not unresolved
    if not complete and not allow_incomplete:
        return 1, unresolved

    inventory = {
        "schema": 1,
        "target": triple,
        "complete": complete,
        "unresolved": unresolved,
        "packages": [{
            "name": r.name,
            "version": r.version,
            "source": r.source,
            "source_url": source_url(r),
            "declared_license": r.declared,
            "license": " AND ".join(r.chosen) if r.chosen else None,
            "status": r.status,
            "reviewed_override": r.reviewed,
            "reviewed_text_evidence": r.text_evidence,
            "standard_apache_text": r.standard_apache,
            "texts": [{"path": f.relative, "sha256": f.sha256} for f in r.files],
        } for r in results],
        "native": [{k: v for k, v in n.items() if k != "_texts"} |
                   {"texts": [{"path": f.relative, "sha256": f.sha256} for f in n.get("_texts", [])]}
                   for n in natives],
        "embedded": [{k: v for k, v in a.items() if k != "_texts"} |
                     {"texts": [{"path": f.relative, "sha256": f.sha256} for f in a.get("_texts", [])]}
                     for a in assets],
        "build_time_only": [{"name": p["name"], "version": p["version"], "reason": "proc-macro, not linked"}
                            for p in build_time],
    }
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "THIRD_PARTY_NOTICES.txt").write_text(render_notices(results, natives, apache_text, complete, triple, assets))
    (out_dir / "third-party-inventory.json").write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n")
    return 0, unresolved


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--target", required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--metadata", type=Path)
    parser.add_argument("--native-lib-dir", type=Path)
    parser.add_argument("--allow-incomplete", action="store_true")
    parser.add_argument("--policy", type=Path, default=POLICY)
    parser.add_argument("--native-manifest", type=Path, default=NATIVE_MANIFEST)
    parser.add_argument("--asset-manifest", type=Path, default=ASSET_MANIFEST)
    parser.add_argument("--apache-text", type=Path, default=REPO / "LICENSE")
    args = parser.parse_args(argv)
    try:
        metadata = json.loads(args.metadata.read_text()) if args.metadata else run_cargo_metadata(args.target)
        apache_text = args.apache_text.read_text()
        if "Apache License" not in apache_text or "Version 2.0, January 2004" not in apache_text:
            raise InputError(f"{args.apache_text} is not the Apache License 2.0 text")
        code, messages = generate(metadata, args.target, args.out_dir, Policy.load(args.policy),
                                  args.native_manifest, args.native_lib_dir, args.allow_incomplete, apache_text,
                                  args.asset_manifest)
    except (InputError, OSError, json.JSONDecodeError, KeyError) as error:
        print(f"third-party-notices: {error}", file=sys.stderr)
        return 2
    for message in messages:
        print(f"third-party-notices: {'unresolved' if code == 0 else 'error'}: {message}", file=sys.stderr)
    if code == 0 and messages:
        print("third-party-notices: inventory INCOMPLETE (local build only)", file=sys.stderr)
    return code


if __name__ == "__main__":
    raise SystemExit(main())

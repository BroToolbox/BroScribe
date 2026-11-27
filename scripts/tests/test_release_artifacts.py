"""Tests for scripts/release-metadata.py and scripts/verify-macos-artifact.py.

All bundles, logs and signatures here are synthetic files; nothing is signed,
notarized or sent anywhere.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import os
import plistlib
import shutil
import struct
import subprocess
import tempfile
import unittest
import zipfile
import zlib
from pathlib import Path
from unittest import mock

from support import REPO, load_script

metadata = load_script("release-metadata.py")
verify = load_script("verify-macos-artifact.py")

COMMIT = "0123456789abcdef0123456789abcdef01234567"
DEV_ID = "Developer ID Application: Synthetic Example (SYNTH12345)"


def build_info_args(tmp: Path, **changes) -> argparse.Namespace:
    lock = tmp / "Cargo.lock"
    lock.write_text("version = 4\n")
    values = dict(version="0.2.0", build="12", target="aarch64-apple-darwin", git_commit=COMMIT,
                  git_dirty="false", rustc="rustc 1.100.0", cargo="cargo 1.100.0", cargo_lock=lock,
                  sherpa_archive="sherpa-onnx-v2.0.1-osx-arm64-static-lib.tar.bz2", sherpa_sha256="a" * 64,
                  profile="release", copyright="Copyright 2026 Synthetic Example")
    values.update(changes)
    return argparse.Namespace(**values)


def make_bundle(root: Path, build: int = 12, version: str = "0.2.0", complete: bool = True,
                dirty: bool = False) -> Path:
    app = root / "BroScribe.app"
    helper = app / verify.HELPER
    for path in verify.REQUIRED:
        (app / path).parent.mkdir(parents=True, exist_ok=True)
        (app / path).write_bytes(b"synthetic executable\x00/broscribe/crates/x.rs\x00")
    info = {"CFBundleIdentifier": "org.broscribe.BroScribe", "CFBundleShortVersionString": version,
            "CFBundleVersion": str(build), "CFBundleIconFile": "BroScribe",
            "NSHumanReadableCopyright": "Copyright 2026 Synthetic Example"}
    (app / "Contents/Info.plist").write_bytes(plistlib.dumps(info))
    (helper / "Contents/Info.plist").write_bytes(plistlib.dumps(
        info | {"CFBundleIdentifier": "org.broscribe.Agent"}))
    (app / "Contents/Resources/LICENSE").write_text((REPO / "LICENSE").read_text())
    build_info = metadata.build_info(build_info_args(root, version=version, build=str(build),
                                                     git_dirty="true" if dirty else "false"))
    (app / "Contents/Resources/build-info.json").write_text(json.dumps(build_info))
    (app / "Contents/Resources/third-party-inventory.json").write_text(json.dumps(
        {"target": "aarch64-apple-darwin", "complete": complete,
         "unresolved": [] if complete else ["native onnxruntime: native licence not reviewed"]}))
    return app


def write_zip_entry(archive, entry, data, mode=None):
    if isinstance(entry, str):
        entry = zipfile.ZipInfo(entry)
        entry.create_system = 3
        if mode is None:
            if entry.is_dir():
                mode = 0o040755
            elif entry.filename in verify.EXECUTABLES:
                mode = 0o100755
            else:
                mode = 0o100644
        entry.external_attr = mode << 16
    archive.writestr(entry, data)


def quiet(function, *args, **kwargs):
    with contextlib.redirect_stderr(io.StringIO()) as err, contextlib.redirect_stdout(io.StringIO()):
        code = function(*args, **kwargs)
    return code, err.getvalue()


class IconTests(unittest.TestCase):
    def test_committed_icons_are_rgba_at_every_size_with_the_grid_margin(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "BroScribe.iconset"
            metadata.write_iconset(out)
            for name, size in metadata.ICONSET.items():
                data = (out / name).read_bytes()
                self.assertEqual(metadata.png_size(data), (size, size), name)
            # The tile sits on Apple's grid: the corner pixel is transparent.
            data = (out / "icon_32x32.png").read_bytes()
            idat = data.index(b"IDAT")
            length = struct.unpack(">I", data[idat - 4:idat])[0]
            raw = zlib.decompress(data[idat + 4:idat + 4 + length])
            # Any PNG filter leaves the first pixel of the first row as it is.
            self.assertEqual(raw[1 + 3], 0, "corner alpha")

    def test_missing_or_wrong_sized_icons_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "src.iconset"
            source.mkdir()
            with self.assertRaises(metadata.MetadataError):
                metadata.write_iconset(Path(tmp) / "out.iconset", source)
            for name in metadata.ICONSET:
                (source / name).write_bytes((metadata.ICON_SOURCE / "icon_16x16.png").read_bytes())
            with self.assertRaises(metadata.MetadataError):
                metadata.write_iconset(Path(tmp) / "out.iconset", source)

    def test_iconset_folder_name_is_required(self):
        with tempfile.TemporaryDirectory() as tmp, self.assertRaises(metadata.MetadataError):
            metadata.write_iconset(Path(tmp) / "icons")

    def test_iconset_lists_every_size_iconutil_needs(self):
        self.assertEqual(sorted(set(metadata.ICONSET.values())), [16, 32, 64, 128, 256, 512, 1024])
        self.assertEqual(len(metadata.ICONSET), 10)


class BuildInfoTests(unittest.TestCase):
    def test_valid_build_info_records_lock_checksum(self):
        with tempfile.TemporaryDirectory() as tmp:
            info = metadata.build_info(build_info_args(Path(tmp)))
        self.assertEqual(info["build"], 12)
        self.assertEqual(info["cargo_lock_sha256"], hashlib.sha256(b"version = 4\n").hexdigest())
        self.assertFalse(info["git"]["dirty"])

    def test_rejects_bad_values(self):
        for change in ({"build": "0"}, {"build": "12a"}, {"version": "1.0"}, {"git_commit": "abc"},
                       {"git_dirty": "maybe"}, {"sherpa_sha256": "x"}, {"copyright": "2026 someone"}):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as tmp:
                with self.assertRaises(metadata.MetadataError):
                    metadata.build_info(build_info_args(Path(tmp), **change))


class NotaryLogTests(unittest.TestCase):
    def test_keeps_diagnostics_and_redacts_builder_details(self):
        raw = {
            "jobId": "00000000-0000-0000-0000-000000000000",
            "status": "Invalid",
            "statusSummary": "Archive contains critical validation errors",
            "archiveFilename": "submission.zip",
            "teamId": "SYNTH12345",
            "developerEmail": "person@example.invalid",
            "issues": [{
                "severity": "error",
                "path": "submission.zip/BroScribe.app/Contents/MacOS/broscribe",
                "message": "Signed by /Users/synthetic-builder/key for person@example.invalid",
                "docUrl": "https://developer.apple.com/",
                "architecture": "arm64",
                "internal": "dropped",
            }],
        }
        clean = metadata.sanitize_notary_log(raw, ["/Users/synthetic-builder", "ab"])
        self.assertNotIn("teamId", clean)
        self.assertNotIn("developerEmail", clean)
        self.assertNotIn("internal", clean["issues"][0])
        message = clean["issues"][0]["message"]
        self.assertEqual(message, "Signed by <redacted>/key for <redacted-email>")
        self.assertEqual(clean["status"], "Invalid")

    def test_cli_writes_owner_only_file_even_for_non_json_logs(self):
        with tempfile.TemporaryDirectory() as tmp:
            raw = Path(tmp) / "raw.json"
            raw.write_text("not json")
            out = Path(tmp) / "notarization.log"
            self.assertEqual(metadata.main(["notary-log", "--in", str(raw), "--out", str(out)]), 0)
            self.assertEqual(out.stat().st_mode & 0o777, 0o600)
            self.assertIn("statusSummary", json.loads(out.read_text()))


class ProvenanceTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.signing = self.root / "signing.json"
        self.signing.write_text(json.dumps({"authorities": [DEV_ID, "Developer ID Certification Authority"],
                                            "team_identifier": "SYNTH12345",
                                            "designated_requirement": "identifier x"}))

    def artifact(self, app: Path) -> Path:
        artifact = self.dist / "BroScribe-0.2.0-12-macos-arm64.zip"
        with zipfile.ZipFile(artifact, "w") as archive:
            for path in sorted(app.rglob("*")):
                if path.is_file():
                    write_zip_entry(archive, (Path("BroScribe.app") / path.relative_to(app)).as_posix(),
                                    path.read_bytes())
        return artifact

    def provenance(self, app, artifact, status="Accepted"):
        return quiet(metadata.main, ["provenance", "--artifact", str(artifact), "--app", str(app),
                                     "--signing-json", str(self.signing), "--submission-id", "synthetic-id",
                                     "--notary-status", status, "--dist-dir", str(self.dist)])

    def test_writes_checksum_and_provenance_that_the_verifier_accepts(self):
        app = make_bundle(self.root)
        artifact = self.artifact(app)
        self.assertEqual(self.provenance(app, artifact)[0], 0)
        record = json.loads((self.dist / "BroScribe-0.2.0-12-macos-arm64.provenance.json").read_text())
        self.assertEqual(record["build"], 12)
        self.assertEqual(record["artifact"]["sha256"], hashlib.sha256(artifact.read_bytes()).hexdigest())
        self.assertTrue(record["notarization"]["stapled"])
        sidecar = (self.dist / f"{artifact.name}.sha256").read_text()
        self.assertEqual(sidecar, f"{record['artifact']['sha256']}  {artifact.name}\n")
        code, err = quiet(verify.main, [str(artifact), "--provenance",
                                        str(self.dist / "BroScribe-0.2.0-12-macos-arm64.provenance.json"),
                                        "--require-complete-licences"])
        self.assertEqual(code, 0, err)

    def test_refuses_rejected_dirty_incomplete_or_non_increasing_builds(self):
        app = make_bundle(self.root)
        artifact = self.artifact(app)
        self.assertEqual(self.provenance(app, artifact, status="Invalid")[0], 1)
        (self.dist / "older.provenance.json").write_text(json.dumps({"build": 12, "git": {"commit": "f" * 40}}))
        code, err = self.provenance(app, artifact)
        self.assertEqual(code, 1)
        self.assertIn("does not increase", err)
        (self.dist / "older.provenance.json").unlink()
        for kwargs, message in (({"dirty": True}, "modified working tree"), ({"complete": False}, "incomplete")):
            with self.subTest(kwargs=kwargs):
                app = make_bundle(self.root, **kwargs)
                code, err = self.provenance(app, artifact)
                self.assertEqual(code, 1)
                self.assertIn(message, err)

    def test_refuses_non_developer_id_signer(self):
        app = make_bundle(self.root)
        self.signing.write_text(json.dumps({"authorities": ["Apple Development: Synthetic"],
                                            "team_identifier": "X", "designated_requirement": "x"}))
        code, err = self.provenance(app, self.artifact(app))
        self.assertEqual(code, 1)
        self.assertIn("Developer ID", err)


class VerifierTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)

    def problems(self, app, forbid=(), complete=False):
        return verify.check_bundle(verify.Bundle(app), list(forbid), complete)

    def test_complete_bundle_passes(self):
        self.assertEqual(self.problems(make_bundle(self.root), complete=True), [])

    def test_outer_daemon_copy_is_rejected(self):
        app = make_bundle(self.root)
        (app / "Contents/MacOS/broscribed").write_bytes(b"duplicate")
        self.assertIn("login-item helper", " ".join(self.problems(app)))

    def test_missing_licence_files_are_rejected(self):
        app = make_bundle(self.root)
        (app / "Contents/Resources/THIRD_PARTY_NOTICES.txt").unlink()
        self.assertEqual(self.problems(app), ["missing Contents/Resources/THIRD_PARTY_NOTICES.txt"])

    def test_version_disagreement_and_helper_mismatch_are_rejected(self):
        app = make_bundle(self.root)
        helper_plist = app / verify.HELPER / "Contents/Info.plist"
        helper_plist.write_bytes(plistlib.dumps(plistlib.loads(helper_plist.read_bytes()) | {"CFBundleVersion": "11"}))
        build = json.loads((app / "Contents/Resources/build-info.json").read_text())
        build["build"] = 13
        (app / "Contents/Resources/build-info.json").write_text(json.dumps(build))
        text = " ".join(self.problems(app))
        self.assertIn("disagrees with build-info.json", text)
        self.assertIn("helper's version or build", text)

    def test_builder_paths_are_detected(self):
        app = make_bundle(self.root)
        (app / "Contents/MacOS/broscribe-worker").write_bytes(b"\x00/Users/synthetic-builder/src/x.rs\x00")
        problems = self.problems(app, forbid=["/Users/synthetic-builder"])
        self.assertEqual(len(problems), 1)
        self.assertIn("broscribe-worker embeds", problems[0])

    def test_narrow_macos_upstream_exception_applies_only_to_executables(self):
        app = make_bundle(self.root)
        path = b"/Users/runner/work/sherpa-onnx/sherpa-onnx/csrc/log.cc"
        worker = app / "Contents/MacOS/broscribe-worker"
        worker.write_bytes(b"\0" + path + b"\0")
        with mock.patch.dict(os.environ, {"HOME": "/Users/runner", "RUNNER_TEMP": "/Volumes/build/runner-temp"}):
            self.assertEqual(quiet(verify.main, [str(app)])[0], 0)
            for leak in (b"/Users/runner/work/broscribe/broscribe/build.cpp",
                         b"/Volumes/build/runner-temp/own.cc",
                         b"/Users/runner/work/sherpa-onnx/another-project/log.cc"):
                with self.subTest(leak=leak):
                    worker.write_bytes(b"\0".join((path, leak)))
                    code, err = quiet(verify.main, [str(app)])
                    self.assertEqual(code, 1)
                    self.assertIn("broscribe-worker embeds", err)
            worker.write_bytes(path)
            (app / "Contents/Resources/embedded.log").write_bytes(path)
            self.assertIn("embedded.log embeds", quiet(verify.main, [str(app)])[1])

    @unittest.skipUnless(os.name == "posix" and shutil.which("bash"), "requires a POSIX shell")
    def test_prebuilt_macos_signing_does_not_compile_and_signs_inside_out(self):
        app = make_bundle(self.root)
        tools = self.root / "tools"
        tools.mkdir()
        calls = self.root / "codesign.log"
        for name, command in {
            "uname": "echo Darwin",
            "cargo": 'echo "unexpected compilation" >&2; exit 4',
            "codesign": 'printf "%s\\n" "$*" >> "$CODESIGN_CALLS"',
        }.items():
            tool = tools / name
            tool.write_text("#!/bin/sh\n" + command + "\n")
            tool.chmod(0o755)
        env = {**os.environ, "PATH": str(tools) + os.pathsep + os.environ["PATH"],
               "CODESIGN_CALLS": str(calls), "BROSCRIBE_SIGN_IDENTITY": "-"}
        result = subprocess.run(["bash", str(REPO / "scripts/sign-macos.sh"), str(app)],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        paths = [app / path for path in ("Contents/MacOS/broscribe-worker", "Contents/MacOS/broscribe",
                                         "Contents/MacOS/broscribe-update-runner", verify.HELPER, "")]
        logged = calls.read_text().splitlines()
        self.assertEqual(len(logged), len(paths) + 1)
        for line, path in zip(logged[:-1], paths, strict=True):
            self.assertTrue(line.startswith("--force --sign - --options runtime"), line)
            self.assertTrue(line.endswith(str(path)), line)
        self.assertEqual(logged[-1], f"--verify --deep --strict --verbose=2 {app}")

        build = app / "Contents/Resources/build-info.json"
        info = json.loads(build.read_bytes())
        info["git"]["dirty"] = True
        build.write_text(json.dumps(info))
        env["BROSCRIBE_SIGN_IDENTITY"] = DEV_ID
        calls.unlink()
        result = subprocess.run(["bash", str(REPO / "scripts/sign-macos.sh"), str(app)],
                                env=env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires a clean build", result.stderr)
        self.assertFalse(calls.exists())

    def test_incomplete_inventory_fails_only_when_required(self):
        app = make_bundle(self.root, complete=False)
        self.assertEqual(self.problems(app), [])
        self.assertIn("incomplete", " ".join(self.problems(app, complete=True)))

    def test_zip_needs_matching_checksum(self):
        app = make_bundle(self.root)
        artifact = self.root / "BroScribe.zip"
        with zipfile.ZipFile(artifact, "w") as archive:
            for path in sorted(app.rglob("*")):
                if path.is_file():
                    write_zip_entry(archive, (Path("BroScribe.app") / path.relative_to(app)).as_posix(),
                                    path.read_bytes())
        code, err = quiet(verify.main, [str(artifact)])
        self.assertEqual(code, 1)
        self.assertIn("no BroScribe.zip.sha256", err)
        (self.root / "BroScribe.zip.sha256").write_text(f"{'0' * 64}  BroScribe.zip\n")
        self.assertEqual(quiet(verify.main, [str(artifact)])[0], 1)
        digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
        (self.root / "BroScribe.zip.sha256").write_text(f"{digest}  BroScribe.zip\n")
        self.assertEqual(quiet(verify.main, [str(artifact)])[0], 0)
        (self.root / "BroScribe.zip.sha256").write_text(f"{digest}  BroScribe.zip  extra\n")
        code, err = quiet(verify.main, [str(artifact)])
        self.assertEqual(code, 1)
        self.assertIn("BroScribe.zip.sha256 does not match", err)

    def test_zip_rejects_private_and_group_or_other_writable_payload(self):
        app = make_bundle(self.root)
        artifact = self.root / "BroScribe.zip"
        signature = "BroScribe.app/Contents/_CodeSignature"
        entries = {
            f"{signature}/": (0o040755, b""),
            f"{signature}/CodeResources": (0o100644, b"synthetic signature"),
            "BroScribe.app/Contents/CodeResources": (0o100644, b"synthetic ticket"),
        }
        cases = (
            (f"{signature}/", 0o040700),
            (f"{signature}/", 0o040775),
            (f"{signature}/CodeResources", 0o100600),
            ("BroScribe.app/Contents/CodeResources", 0o100646),
            ("BroScribe.app/Contents/MacOS/broscribe", 0o100644),
            ("BroScribe.app/Contents/MacOS/broscribe", 0o100744),
        )
        for name, mode in cases:
            with self.subTest(name=name, mode=mode):
                with zipfile.ZipFile(artifact, "w") as archive:
                    for path in sorted(app.rglob("*")):
                        if path.is_file():
                            entry = (Path("BroScribe.app") / path.relative_to(app)).as_posix()
                            write_zip_entry(archive, entry, path.read_bytes(), mode if entry == name else None)
                    for entry, (normal, data) in entries.items():
                        write_zip_entry(archive, entry, data, mode if entry == name else normal)
                digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
                artifact.with_suffix(".zip.sha256").write_text(f"{digest}  {artifact.name}\n")
                code, err = quiet(verify.main, [str(artifact)])
                self.assertEqual(code, 1)
                self.assertIn(f"{name.rstrip('/')} has unsafe distribution permissions", err)

    LINK_MODE = 0o120777 << 16

    def layout_problems(self, extra=(), mutate=None):
        """archive_layout of a good bundle zip plus `extra` (ZipInfo or name,
        data) entries; `mutate(infos)` edits the read metadata in memory for
        flags a writer cannot produce (encryption, other compression, kinds)."""
        app = make_bundle(self.root)
        artifact = self.root / "layout.zip"
        with zipfile.ZipFile(artifact, "w", zipfile.ZIP_DEFLATED) as archive:
            write_zip_entry(archive, "BroScribe.app/", b"")
            for path in sorted(app.rglob("*")):
                if path.is_file():
                    write_zip_entry(archive, (Path("BroScribe.app") / path.relative_to(app)).as_posix(),
                                    path.read_bytes())
            for info, data in extra:
                write_zip_entry(archive, info, data)
        with zipfile.ZipFile(artifact) as archive:
            if mutate:
                mutate({info.filename: info for info in archive.infolist()})
            return verify.archive_layout(archive)

    def link(self, name, target):
        info = zipfile.ZipInfo(name)
        info.external_attr = self.LINK_MODE
        return info, target

    def assert_refused(self, message, extra=(), mutate=None):
        problems = self.layout_problems(extra, mutate)[0]
        self.assertTrue(any(message in p for p in problems), problems)

    def test_updater_archive_layout_counts_entries_and_installed_size(self):
        problems, entries, installed = self.layout_problems()
        self.assertEqual(problems, [])
        files = [p for p in (self.root / "BroScribe.app").rglob("*") if p.is_file()]
        self.assertEqual(entries, len(files) + 1)
        self.assertEqual(installed, sum(p.stat().st_size for p in files))

    def test_updater_archive_layout_accepts_links_inside_the_root(self):
        extra = [
            self.link("BroScribe.app/Contents/inside", b"Resources/LICENSE"),
            self.link("BroScribe.app/Contents/MacOS/tool", b"../Resources/LICENSE"),
            self.link("BroScribe.app/Contents/Resources/up", b"../../Contents/Info.plist"),
        ]
        problems, _, installed = self.layout_problems(extra)
        self.assertEqual(problems, [])
        files = [p for p in (self.root / "BroScribe.app").rglob("*") if p.is_file()]
        self.assertEqual(installed, sum(p.stat().st_size for p in files) + sum(len(t) for _, t in extra))

    def test_updater_archive_layout_refuses_unsafe_names(self):
        deep = "BroScribe.app/" + "/".join(["d"] * 64) + "/f"
        cases = {
            "AppleDouble file": ("BroScribe.app/Contents/._Info.plist", "unsafe path component"),
            "AppleDouble folder": ("BroScribe.app/__MACOSX/x", "unsafe path component"),
            "top-level __MACOSX": ("__MACOSX/BroScribe.app/._x", "outside the archive root"),
            "other root": ("Other.app/x", "outside the archive root"),
            "absolute": ("/BroScribe.app/x", "outside the archive root"),
            "parent step": ("BroScribe.app/../x", "unsafe path component"),
            "dot step": ("BroScribe.app/./x", "unsafe path component"),
            "empty component": ("BroScribe.app//x", "unsafe path component"),
            "trailing dot": ("BroScribe.app/Contents/x.", "unsafe path component"),
            "trailing space": ("BroScribe.app/Contents/x ", "unsafe path component"),
            "backslash": ("BroScribe.app/a\\b", "unsafe path component"),
            "drive colon": ("BroScribe.app/C:x", "unsafe path component"),
            "question mark": ("BroScribe.app/a?b", "unsafe path component"),
            "pipe": ("BroScribe.app/a|b", "unsafe path component"),
            "control character": ("BroScribe.app/a\x01b", "unsafe path component"),
            "delete character": ("BroScribe.app/a\x7fb", "unsafe path component"),
            "non-ASCII": ("BroScribe.app/caf\u00e9", "not ASCII"),
            "too long": ("BroScribe.app/" + "a" * 1011, "too long"),
            "too deep": (deep, "nested too deeply"),
        }
        for label, (name, message) in cases.items():
            with self.subTest(label):
                self.assert_refused(message, [(name, b"x")])
        # The limits themselves are accepted: 1024 bytes and 64 components.
        at_limit = ("BroScribe.app/" + "a" * 1010, b"x")
        at_depth = ("BroScribe.app/" + "/".join(["d"] * 62) + "/f", b"x")
        self.assertEqual(self.layout_problems([at_limit, at_depth])[0], [])

    def test_updater_archive_layout_refuses_duplicates_and_bad_ancestry(self):
        self.assert_refused("appears twice", [("BroScribe.app/Contents/Resources/license", b"x")])
        self.assert_refused("appears twice", [(zipfile.ZipInfo("BroScribe.app/Contents/Resources/LICENSE/"), b"")])
        self.assert_refused("lies under a file or link", [("BroScribe.app/Contents/Info.plist/x", b"x")])
        self.assert_refused("lies under a file or link", [
            self.link("BroScribe.app/Contents/Linked", b"Resources"),
            ("BroScribe.app/Contents/linked/x", b"x"),
        ])
        self.assert_refused("the archive root is not a folder", [("BroScribe.app", b"x")])

    def test_updater_archive_layout_refuses_bad_kinds_and_metadata(self):
        def set_attrs(name, **attrs):
            return lambda infos: [setattr(infos[name], k, v) for k, v in attrs.items()]

        plist = "BroScribe.app/Contents/Info.plist"
        cases = {
            "is encrypted": set_attrs(plist, flag_bits=0x1),
            "unsupported compression method": set_attrs(plist, compress_type=12),
            "special file": set_attrs(plist, external_attr=0o010644 << 16),
            "disagrees with its name": set_attrs("BroScribe.app/", external_attr=0o100644 << 16),
            "disagrees": set_attrs(plist, external_attr=0o040755 << 16),
            "carries data": set_attrs("BroScribe.app/", file_size=5),
        }
        for message, mutate in cases.items():
            with self.subTest(message):
                self.assert_refused(message, mutate=mutate)
        # MS-DOS attributes map to a kind as the zip crate does: the folder
        # bit on a file name disagrees with it.
        self.assert_refused("disagrees", mutate=set_attrs(plist, create_system=0, external_attr=0x10))
        self.assert_refused("unsafe distribution permissions",
                            mutate=set_attrs(plist, create_system=0, external_attr=0x20))
        self.assertEqual(self.layout_problems(mutate=set_attrs(plist, create_system=0, external_attr=0x01))[0], [])

    def test_updater_archive_layout_refuses_unsafe_links(self):
        cases = {
            "climbs above the root": (self.link("BroScribe.app/Contents/escape", b"../../x"), "points outside"),
            "root's parent": (self.link("BroScribe.app/top", b".."), "points outside"),
            "absolute": (self.link("BroScribe.app/Contents/abs", b"/etc/passwd"), "absolute or non-ASCII"),
            "non-ASCII": (self.link("BroScribe.app/Contents/u", "caf\u00e9".encode()), "absolute or non-ASCII"),
            "not UTF-8": (self.link("BroScribe.app/Contents/b", b"\xff"), "absolute or non-ASCII"),
            "parent after a name": (self.link("BroScribe.app/Contents/a", b"x/../../y"), "unsafe target"),
            "dot step": (self.link("BroScribe.app/Contents/d", b"a/./b"), "unsafe target"),
            "empty component": (self.link("BroScribe.app/Contents/e", b"a//b"), "unsafe target"),
            "AppleDouble target": (self.link("BroScribe.app/Contents/f", b"._x"), "unsafe target"),
            "empty target": (self.link("BroScribe.app/Contents/g", b""), "bad target length"),
            "overlong target": (self.link("BroScribe.app/Contents/h", b"a" * 1025), "bad target length"),
        }
        for label, (entry, message) in cases.items():
            with self.subTest(label):
                self.assert_refused(message, [entry])
        self.assertEqual(self.layout_problems([self.link("BroScribe.app/Contents/i", b"a" * 1024)])[0], [])

    def test_updater_archive_layout_entry_limit(self):
        with mock.patch.object(verify, "MAX_ENTRIES", 3):
            self.assert_refused("the updater accepts at most 3")


class SymbolVerifierTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.symbols = self.root / "symbols"
        self.dwarf = self.symbols / "broscribe.dSYM/Contents/Resources/DWARF/broscribe"
        self.dwarf.parent.mkdir(parents=True)
        self.dwarf.write_bytes(b"synthetic DWARF")
        (self.symbols / "UUIDS.txt").write_text(
            "UUID: 01234567-89AB-CDEF-0123-456789ABCDEF (arm64) broscribe\n")

    def inputs(self):
        yield self.symbols
        artifact = self.root / "symbols.zip"
        with zipfile.ZipFile(artifact, "w") as archive:
            for path in sorted(self.symbols.rglob("*")):
                if path.is_file():
                    archive.write(path, (Path("symbols") / path.relative_to(self.symbols)).as_posix())
        yield artifact

    def test_symbol_folder_and_archive_preserve_clean_debug_payload(self):
        for path in self.inputs():
            with self.subTest(path=path.name):
                self.assertEqual(quiet(verify.main, [
                    str(path), "--symbols", "--forbid", "/Users/synthetic-builder"])[0], 0)
        with zipfile.ZipFile(self.root / "symbols.zip") as archive:
            self.assertEqual(archive.read(
                "symbols/broscribe.dSYM/Contents/Resources/DWARF/broscribe"), b"synthetic DWARF")

    def test_dwarf_accepts_pinned_upstream_but_rejects_builder_paths(self):
        upstream = b"/Users/runner/work/onnxruntime-build/onnxruntime-build/onnxruntime/session.cc"
        self.dwarf.write_bytes(upstream)
        for path in self.inputs():
            self.assertEqual(quiet(verify.main, [str(path), "--symbols"])[0], 0)
        self.dwarf.write_bytes(upstream + b"\0/Users/runner/work/broscribe/source.cc")
        for path in self.inputs():
            self.assertIn("DWARF/broscribe embeds", quiet(verify.main, [str(path), "--symbols"])[1])

    def test_builder_paths_in_non_dwarf_files_are_refused(self):
        (self.symbols / "input-map.yml").write_bytes(
            b"binary-path: /Users/synthetic-builder/target/broscribe")
        for path in self.inputs():
            with self.subTest(path=path.name):
                code, err = quiet(verify.main, [
                    str(path), "--symbols", "--forbid", "/Users/synthetic-builder"])
                self.assertEqual(code, 1)
                self.assertIn("input-map.yml embeds", err)

    def test_relocation_maps_and_appledouble_are_refused(self):
        relocations = self.dwarf.parent.parent / "Relocations"
        relocations.mkdir()
        (relocations / "broscribe.yml").write_bytes(b"build-only map")
        (self.symbols / "._UUIDS.txt").write_bytes(b"AppleDouble")
        for path in self.inputs():
            with self.subTest(path=path.name):
                code, err = quiet(verify.main, [str(path), "--symbols"])
                self.assertEqual(code, 1)
                self.assertIn("build-only relocation maps", err)
                self.assertIn("AppleDouble metadata", err)

    def test_symlinked_symbols_are_refused(self):
        alias = self.symbols / "alias"
        alias.symlink_to("UUIDS.txt")
        self.assertEqual(quiet(verify.main, [str(self.symbols), "--symbols"])[0], 2)
        artifact = self.root / "linked.zip"
        with zipfile.ZipFile(artifact, "w") as archive:
            entry = zipfile.ZipInfo("symbols/alias")
            entry.create_system = 3
            entry.external_attr = 0o120777 << 16
            archive.writestr(entry, "UUIDS.txt")
        self.assertEqual(quiet(verify.main, [str(artifact), "--symbols"])[0], 2)

    def test_empty_symbol_payload_cannot_pass(self):
        self.dwarf.write_bytes(b"")
        (self.symbols / "UUIDS.txt").write_bytes(b"")
        for path in self.inputs():
            with self.subTest(path=path.name):
                code, err = quiet(verify.main, [str(path), "--symbols"])
                self.assertEqual(code, 1)
                self.assertIn("no nonempty DWARF files", err)
                self.assertIn("no nonempty UUIDS.txt", err)


if __name__ == "__main__":
    unittest.main()

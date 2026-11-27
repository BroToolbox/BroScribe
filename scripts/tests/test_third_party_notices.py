"""Tests for scripts/third-party-notices.py with synthetic cargo metadata."""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from support import REPO, load_script

notices = load_script("third-party-notices.py")

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
APACHE = (REPO / "LICENSE").read_text()
MIT_TEXT = ("MIT License\n\nCopyright (c) 2026 Synthetic Author\n\nPermission is hereby granted, free of charge, "
            "to any person obtaining a copy of this software.\n")


class Workspace:
    def __init__(self, test: unittest.TestCase):
        self._tmp = tempfile.TemporaryDirectory()
        test.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.packages: list[dict] = []
        self.nodes: dict[str, dict] = {}
        self.members: list[str] = []
        for name in notices.SHIPPED_PACKAGES:
            self.member(name)
        self.policy = self.root / "policy.toml"
        self.policy.write_text((REPO / "scripts" / "third-party-licenses.toml").read_text())
        self.native = self.root / "NATIVE_LICENSES.toml"
        self.native.write_text("")

    def _add(self, name: str, version: str, source, license_expr, files: dict, proc_macro=False) -> str:
        pid = f"{name} {version} ({source or 'path'})"
        directory = self.root / "src" / f"{name}-{version}"
        directory.mkdir(parents=True)
        (directory / "Cargo.toml").write_text("")
        for relative, text in files.items():
            path = directory / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        self.packages.append({
            "id": pid, "name": name, "version": version, "source": source, "license": license_expr,
            "manifest_path": str(directory / "Cargo.toml"),
            "targets": [{"kind": ["proc-macro" if proc_macro else "lib"]}],
        })
        self.nodes[pid] = {"id": pid, "deps": []}
        return pid

    def member(self, name: str) -> str:
        pid = self._add(name, "0.1.0", None, "Apache-2.0", {})
        self.members.append(pid)
        return pid

    def crate(self, name: str, license_expr, files=None, version="1.0.0", proc_macro=False) -> str:
        return self._add(name, version, REGISTRY, license_expr, files or {}, proc_macro)

    def depend(self, parent: str, child: str, kind=None) -> None:
        self.nodes[parent]["deps"].append({"pkg": child, "dep_kinds": [{"kind": kind, "target": None}]})

    @property
    def app(self) -> str:
        return self.members[0]

    def metadata(self) -> dict:
        return {"packages": self.packages, "workspace_members": self.members,
                "resolve": {"nodes": list(self.nodes.values())}}

    def generate(self, allow_incomplete=False, lib_dir=None):
        out = self.root / "out"
        code, messages = notices.generate(self.metadata(), "aarch64-apple-darwin", out,
                                          notices.Policy.load(self.policy), self.native, lib_dir,
                                          allow_incomplete, APACHE)
        return code, messages, out


class ExpressionTests(unittest.TestCase):
    def test_or_and_with_and_legacy_slash(self):
        self.assertEqual(notices.parse_expression("MIT OR Apache-2.0"), [("MIT",), ("Apache-2.0",)])
        self.assertEqual(notices.parse_expression("MIT/Apache-2.0"), [("MIT",), ("Apache-2.0",)])
        self.assertEqual(notices.parse_expression("(MIT OR Apache-2.0) AND Unicode-3.0"),
                         [("MIT", "Unicode-3.0"), ("Apache-2.0", "Unicode-3.0")])
        self.assertEqual(notices.parse_expression("Apache-2.0 WITH LLVM-exception OR MIT"),
                         [("Apache-2.0 WITH LLVM-exception",), ("MIT",)])

    def test_malformed_expressions_are_rejected(self):
        for bad in ("", "MIT OR", "(MIT", "MIT WITH", "AND MIT"):
            with self.assertRaises(ValueError, msg=bad):
                notices.parse_expression(bad)


class GenerateTests(unittest.TestCase):
    def test_local_third_party_dependency_keeps_its_notices_without_builder_paths(self):
        ws = Workspace(self)
        patched = ws._add("patched-ui", "1.0.0", None, "MIT", {"LICENSE": MIT_TEXT})
        ws.depend(ws.app, patched)
        code, messages, out = ws.generate()
        self.assertEqual((code, messages), (0, []))
        inventory = json.loads((out / "third-party-inventory.json").read_text())
        entry = next(p for p in inventory["packages"] if p["name"] == "patched-ui")
        self.assertEqual(entry["license"], "MIT")
        self.assertTrue(entry["source"], "a local dependency still needs source attribution")
        text = (out / "THIRD_PARTY_NOTICES.txt").read_text()
        self.assertIn("patched-ui", text)
        self.assertIn("Copyright (c) 2026 Synthetic Author", text)
        self.assertNotIn(str(ws.root), text + json.dumps(inventory))

    def test_only_linked_crates_ship_and_texts_are_resolved(self):
        ws = Workspace(self)
        dual = ws.crate("dual", "MIT OR Apache-2.0")
        mit = ws.crate("mitcrate", "MIT", {"LICENSE": MIT_TEXT, "NOTICE": "Synthetic notice\n"})
        macro = ws.crate("macro", "MIT", proc_macro=True)
        build = ws.crate("buildonly", "GPL-3.0-only")
        dev = ws.crate("devonly", "GPL-3.0-only")
        ws.depend(ws.app, dual)
        ws.depend(dual, mit)
        ws.depend(dual, macro)
        ws.depend(ws.app, build, kind="build")
        ws.depend(ws.app, dev, kind="dev")
        code, messages, out = ws.generate()
        self.assertEqual((code, messages), (0, []))
        inventory = json.loads((out / "third-party-inventory.json").read_text())
        self.assertTrue(inventory["complete"])
        names = [p["name"] for p in inventory["packages"]]
        self.assertEqual(names, ["dual", "mitcrate"])
        self.assertEqual(inventory["build_time_only"][0]["name"], "macro")
        dual_entry = inventory["packages"][0]
        self.assertEqual(dual_entry["license"], "Apache-2.0")
        self.assertTrue(dual_entry["standard_apache_text"])
        text = (out / "THIRD_PARTY_NOTICES.txt").read_text()
        self.assertIn("Synthetic notice", text)
        self.assertIn("Copyright (c) 2026 Synthetic Author", text)
        self.assertNotIn(str(ws.root), text + json.dumps(inventory))
        self.assertNotIn("INCOMPLETE", text)

    def test_disallowed_licence_fails_even_when_incomplete_is_allowed(self):
        ws = Workspace(self)
        ws.depend(ws.app, ws.crate("copyleft", "GPL-3.0-only", {"COPYING": "GNU GENERAL PUBLIC LICENSE\n"}))
        code, messages, out = ws.generate(allow_incomplete=True)
        self.assertEqual(code, 1)
        self.assertIn("not allowed", messages[0])
        self.assertFalse((out / "THIRD_PARTY_NOTICES.txt").exists())

    def test_missing_licence_metadata_fails(self):
        ws = Workspace(self)
        ws.depend(ws.app, ws.crate("nolicence", None, {"LICENSE": MIT_TEXT}))
        code, messages, _ = ws.generate(allow_incomplete=True)
        self.assertEqual(code, 1)
        self.assertIn("no SPDX licence", messages[0])

    def test_missing_text_is_unresolved_and_only_local_builds_may_continue(self):
        ws = Workspace(self)
        ws.depend(ws.app, ws.crate("bare", "MIT"))
        code, messages, out = ws.generate()
        self.assertEqual(code, 1)
        self.assertIn("ships no licence text for MIT", messages[0])
        code, _, out = ws.generate(allow_incomplete=True)
        self.assertEqual(code, 0)
        inventory = json.loads((out / "third-party-inventory.json").read_text())
        self.assertFalse(inventory["complete"])
        self.assertIn("INCOMPLETE", (out / "THIRD_PARTY_NOTICES.txt").read_text())

    def test_vendored_licence_deep_in_a_crate_does_not_cover_the_crate(self):
        ws = Workspace(self)
        ws.depend(ws.app, ws.crate("vendoring", "MIT", {"vendor/lib/LICENSE": MIT_TEXT}))
        code, messages, _ = ws.generate()
        self.assertEqual(code, 1)
        self.assertIn("ships no licence text", messages[0])

    def test_reviewed_override_is_pinned_to_file_hash(self):
        ws = Workspace(self)
        ws.depend(ws.app, ws.crate("fileonly", None, {"LICENSE": MIT_TEXT}))
        digest = hashlib.sha256(MIT_TEXT.encode()).hexdigest()
        base = ws.policy.read_text()
        ws.policy.write_text(base + f'\n[[override]]\nname = "fileonly"\nversion = "1.0.0"\nlicense = "MIT"\n'
                             f'file = "LICENSE"\nsha256 = "{digest}"\nevidence = "synthetic"\n')
        self.assertEqual(ws.generate()[0], 0)
        ws.policy.write_text(base + '\n[[override]]\nname = "fileonly"\nversion = "1.0.0"\nlicense = "MIT"\n'
                             f'file = "LICENSE"\nsha256 = "{"0" * 64}"\nevidence = "synthetic"\n')
        code, messages, _ = ws.generate()
        self.assertEqual(code, 1)
        self.assertIn("review it again", messages[0])

    def test_reviewed_text_entry_resolves_a_crate_without_text(self):
        ws = Workspace(self)
        ws.depend(ws.app, ws.crate("bare", "MIT"))
        (ws.root / "licenses").mkdir()
        (ws.root / "licenses" / "bare.txt").write_text(MIT_TEXT)
        digest = hashlib.sha256(MIT_TEXT.encode()).hexdigest()
        ws.policy.write_text(ws.policy.read_text() + f'\n[[text]]\nname = "bare"\nversion = "1.0.0"\n'
                             f'license = "MIT"\nfile = "licenses/bare.txt"\nsha256 = "{digest}"\n')
        code, messages, _ = ws.generate()
        self.assertEqual((code, messages), (0, []))

    def test_linked_native_libraries_need_reviewed_entries(self):
        ws = Workspace(self)
        sys_crate = ws.crate("sherpa-onnx-sys", "Apache-2.0", {
            "LICENSE": APACHE,
            "build.rs": 'const SHERPA_ONNX_STATIC_LIBS: &[&str] = &[\n    "alpha",\n    "beta",\n];\n',
        })
        ws.depend(ws.app, sys_crate)
        text = "Synthetic native licence\n"
        (ws.root / "licenses").mkdir()
        (ws.root / "licenses" / "alpha.txt").write_text(text)
        digest = hashlib.sha256(text.encode()).hexdigest()
        ws.native.write_text('[[component]]\nlibrary = "alpha"\nlicense = "MIT"\nevidence = "synthetic"\n'
                             'verified_archives = ["synthetic-osx.tar.bz2"]\n'
                             f'texts = [{{ file = "licenses/alpha.txt", sha256 = "{digest}" }}]\n')
        code, messages, _ = ws.generate(allow_incomplete=True)
        self.assertEqual(code, 1)
        self.assertIn("native beta: linked native library has no entry", messages[0])

        ws.native.write_text(ws.native.read_text() + '\n[[component]]\nlibrary = "beta"\nlicense = ""\ntexts = []\n')
        lib_dir = ws.root / "native"
        lib_dir.mkdir()
        for name in ("alpha", "beta"):
            (lib_dir / f"lib{name}.a").write_bytes(b"synthetic archive")
        with self.assertRaises(notices.InputError):
            ws.generate(allow_incomplete=True, lib_dir=lib_dir)  # no .verified marker
        (lib_dir / ".verified").write_text("version=0\narchive=synthetic-osx.tar.bz2\nsha256=00\n")
        code, messages, out = ws.generate(allow_incomplete=True, lib_dir=lib_dir)
        self.assertEqual(code, 0)
        self.assertIn("native beta: native licence not reviewed", messages[0])
        inventory = json.loads((out / "third-party-inventory.json").read_text())
        self.assertEqual([n["status"] for n in inventory["native"]], ["ok", "missing-text"])
        self.assertEqual(inventory["native"][0]["archive"], "synthetic-osx.tar.bz2")
        self.assertEqual(inventory["native"][0]["sha256"], hashlib.sha256(b"synthetic archive").hexdigest())
        self.assertIn("Synthetic native licence", (out / "THIRD_PARTY_NOTICES.txt").read_text())

        # A review of another target's archive does not cover this one.
        (lib_dir / ".verified").write_text("version=0\narchive=synthetic-linux.tar.bz2\nsha256=00\n")
        code, messages, out = ws.generate(allow_incomplete=True, lib_dir=lib_dir)
        self.assertEqual(code, 0)
        self.assertIn("native alpha: native licence reviewed only for ['synthetic-osx.tar.bz2']", messages[0])

    def test_repository_native_review_is_complete_for_the_pinned_macos_archive(self):
        import tomllib
        manifest = REPO / "third_party" / "sherpa-onnx" / "NATIVE_LICENSES.toml"
        policy = notices.Policy.load(REPO / "scripts" / "third-party-licenses.toml")
        libraries = [c["library"] for c in tomllib.loads(manifest.read_text())["component"]]
        with tempfile.TemporaryDirectory() as tmp:
            lib_dir = Path(tmp)
            for library in libraries:
                (lib_dir / f"lib{library}.a").write_bytes(b"synthetic")
            (lib_dir / ".verified").write_text(
                "version=2.0.1\narchive=sherpa-onnx-v2.0.1-osx-arm64-static-lib.tar.bz2\nsha256=00\n")
            records = notices.native_components(libraries, manifest, lib_dir, "aarch64-apple-darwin", policy)
        self.assertEqual({r["library"]: r["status"] for r in records}, dict.fromkeys(libraries, "ok"))

    def test_embedded_files_ship_with_their_pinned_licence_texts(self):
        ws = Workspace(self)
        assets = ws.root / "assets"
        (assets / "fonts").mkdir(parents=True)
        font = b"synthetic font bytes"
        licence = "Copyright 2026 Synthetic Foundry\n\nSIL Open Font License, Version 1.1 (synthetic)\n"
        (assets / "fonts" / "Synthetic-Regular.ttf").write_bytes(font)
        (assets / "fonts" / "OFL.txt").write_text(licence)
        manifest = assets / "ASSET_LICENSES.toml"

        def write(licence_id="OFL-1.1", font_sha=hashlib.sha256(font).hexdigest()):
            manifest.write_text(
                '[[asset]]\nfile = "fonts/Synthetic-Regular.ttf"\n'
                f'sha256 = "{font_sha}"\nproject = "Synthetic Sans 1.0"\nlicense = "{licence_id}"\n'
                'source = "https://example.invalid/synthetic-sans"\n'
                f'texts = [{{ file = "fonts/OFL.txt", sha256 = "{hashlib.sha256(licence.encode()).hexdigest()}" }}]\n')

        write()
        out = ws.root / "out"
        code, messages = notices.generate(ws.metadata(), "aarch64-apple-darwin", out,
                                          notices.Policy.load(ws.policy), ws.native, None, False, APACHE,
                                          manifest)
        self.assertEqual((code, messages), (0, []))
        text = (out / "THIRD_PARTY_NOTICES.txt").read_text()
        self.assertIn("Synthetic-Regular.ttf (embedded file): OFL-1.1", text)
        self.assertIn("SIL Open Font License, Version 1.1 (synthetic)", text)
        inventory = json.loads((out / "third-party-inventory.json").read_text())
        self.assertEqual([(a["file"], a["status"]) for a in inventory["embedded"]],
                         [("fonts/Synthetic-Regular.ttf", "ok")])

        # A changed file and a licence the policy does not allow both fail.
        for kwargs, detail in (({"font_sha": "0" * 64}, "is missing or changed"),
                               ({"licence_id": "GPL-3.0-only"}, "is not allowed by the policy")):
            write(**kwargs)
            code, messages = notices.generate(ws.metadata(), "aarch64-apple-darwin", out,
                                              notices.Policy.load(ws.policy), ws.native, None, True, APACHE,
                                              manifest)
            self.assertEqual(code, 1)
            self.assertIn(detail, messages[0])

    def test_repository_embedded_fonts_are_reviewed_and_allowed(self):
        policy = notices.Policy.load(REPO / "scripts" / "third-party-licenses.toml")
        records = notices.embedded_assets(notices.ASSET_MANIFEST, policy)
        self.assertEqual({r["file"]: r["status"] for r in records}, {
            "fonts/Inter-Regular.ttf": "ok", "fonts/Inter-Medium.ttf": "ok", "fonts/Inter-SemiBold.ttf": "ok",
            "fonts/Manrope-SemiBold.ttf": "ok", "fonts/Manrope-Bold.ttf": "ok"})

    def test_repository_crate_text_entries_are_pinned_and_present(self):
        policy = notices.Policy.load(REPO / "scripts" / "third-party-licenses.toml")
        for (name, version), entries in policy.texts.items():
            for entry in entries:
                with self.subTest(crate=f"{name} {version}", file=entry["file"]):
                    data = (policy.root / entry["file"]).read_bytes()
                    self.assertEqual(hashlib.sha256(data).hexdigest(), entry["sha256"])
                    self.assertTrue(entry.get("evidence"))

    def test_repository_native_manifest_covers_every_pinned_linked_library(self):
        manifest = REPO / "third_party" / "sherpa-onnx" / "NATIVE_LICENSES.toml"
        import tomllib
        libraries = {c["library"] for c in tomllib.loads(manifest.read_text())["component"]}
        self.assertEqual(libraries, {
            "sherpa-onnx-c-api", "sherpa-onnx-core", "kaldi-decoder-core", "sherpa-onnx-kaldifst-core",
            "sherpa-onnx-fstfar", "sherpa-onnx-fst", "kaldi-native-fbank-core", "kissfft-float",
            "onnxruntime", "ssentencepiece_core", "utf8proc"})

    def test_cli_reports_bad_metadata_as_input_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            metadata = Path(tmp) / "metadata.json"
            metadata.write_text("{not json")
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                code = notices.main(["--target", "aarch64-apple-darwin", "--out-dir", tmp,
                                     "--metadata", str(metadata)])
            self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()

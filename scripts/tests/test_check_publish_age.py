"""Offline tests for scripts/check-publish-age.py (synthetic lock files and index)."""

from __future__ import annotations

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from support import load_script

age = load_script("check-publish-age.py")

NOW = "2026-11-17T00:00:00Z"
OLD = "2026-10-01T00:00:00Z"
NEW = "2026-11-16T12:00:00Z"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def entry(version, pubtime=OLD, yanked=False, deps=()):
    return {"name": "x", "vers": version, "pubtime": pubtime, "yanked": yanked,
            "deps": [dict(d) for d in deps], "cksum": "0" * 64, "features": {}}


class Fixture:
    """A workspace with one member `app` and an offline index directory."""

    def __init__(self, test: unittest.TestCase, app_deps: dict, lock_packages: list, index: dict):
        self._tmp = tempfile.TemporaryDirectory()
        test.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/*"]\n')
        app = self.root / "crates" / "app"
        app.mkdir(parents=True)
        deps = "\n".join(f'{k} = "{v}"' for k, v in app_deps.items())
        (app / "Cargo.toml").write_text(f'[package]\nname = "app"\nversion = "0.1.0"\n\n[dependencies]\n{deps}\n')
        blocks = ['version = 4\n']
        for pkg in lock_packages:
            lines = [f'name = "{pkg["name"]}"', f'version = "{pkg["version"]}"']
            if pkg.get("source", REGISTRY):
                lines.append(f'source = "{pkg.get("source", REGISTRY)}"')
            if pkg.get("dependencies"):
                lines.append("dependencies = [" + ", ".join(f'"{d}"' for d in pkg["dependencies"]) + "]")
            blocks.append("[[package]]\n" + "\n".join(lines) + "\n")
        self.lock = self.root / "Cargo.lock"
        self.lock.write_text("\n".join(blocks))
        self.index = self.root / "index"
        for name, entries in index.items():
            path = self.index / age.index_path(name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("".join(json.dumps(e) + "\n" for e in entries))

    def run(self, *extra):
        out = io.StringIO()
        err = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = age.main([str(self.lock), "--index-dir", str(self.index), "--offline", "--now", NOW, *extra])
        return code, out.getvalue() + err.getvalue()


def app_on(*deps):
    return {"name": "app", "version": "0.1.0", "source": None, "dependencies": list(deps)}


class PublishAgeTests(unittest.TestCase):
    def test_old_unyanked_lock_passes_offline(self):
        fx = Fixture(self, {"widget": "1.2"}, [app_on("widget"), {"name": "widget", "version": "1.2.3"}],
                     {"widget": [entry("1.2.3")]})
        code, out = fx.run()
        self.assertEqual(code, 0, out)
        self.assertIn("ok: 1 crates.io packages", out)

    def test_too_new_suggests_newest_old_compatible_release_not_prerelease(self):
        fx = Fixture(self, {"widget": "1"}, [app_on("widget"), {"name": "widget", "version": "1.3.1"}],
                     {"widget": [entry("1.2.3"), entry("1.2.9"), entry("1.3.0-rc.1"),
                                 entry("1.2.10", yanked=True), entry("1.3.1", NEW)]})
        code, out = fx.run()
        self.assertEqual(code, 1)
        self.assertIn("widget 1.3.1: published 2026-11-16 12:00 UTC", out)
        self.assertIn("--precise 1.2.9", out)
        self.assertNotIn("rc.1", out)
        self.assertNotIn("1.2.10", out)

    def test_suggestion_respects_workspace_requirement(self):
        fx = Fixture(self, {"widget": "1.2.5"}, [app_on("widget"), {"name": "widget", "version": "1.2.6"}],
                     {"widget": [entry("1.2.4"), entry("1.2.6", NEW)]})
        code, out = fx.run()
        self.assertEqual(code, 1)
        self.assertNotIn("--precise", out)
        self.assertIn("app 0.1.0 needs 1.2.5", out)

    def test_suggestion_respects_registry_dependent_requirement(self):
        fx = Fixture(self, {"outer": "2"}, [
            app_on("outer"),
            {"name": "outer", "version": "2.0.0", "dependencies": ["widget"]},
            {"name": "widget", "version": "0.4.7"},
        ], {
            "outer": [entry("2.0.0", deps=[{"name": "widget", "req": "^0.4.5", "kind": "normal"}])],
            "widget": [entry("0.4.3"), entry("0.4.5"), entry("0.4.7", NEW)],
        })
        code, out = fx.run()
        self.assertEqual(code, 1)
        self.assertIn("--precise 0.4.5", out)

    def test_yanked_lock_entry_fails_and_suggests_compatible_replacement(self):
        fx = Fixture(self, {"widget": "1.2"}, [app_on("widget"), {"name": "widget", "version": "1.2.3"}],
                     {"widget": [entry("1.2.3", yanked=True), entry("1.2.4"), entry("2.0.0")]})
        code, out = fx.run()
        self.assertEqual(code, 1)
        self.assertIn("widget 1.2.3: yanked", out)
        self.assertIn("--precise 1.2.4", out)
        self.assertNotIn("2.0.0", out)

    def test_prerelease_lock_may_move_to_older_prerelease(self):
        fx = Fixture(self, {"widget": "=1.0.0-beta.3"},
                     [app_on("widget"), {"name": "widget", "version": "1.0.0-beta.3"}],
                     {"widget": [entry("1.0.0-beta.2"), entry("1.0.0-beta.3", NEW)]})
        fx.root.joinpath("crates/app/Cargo.toml").write_text(
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[dependencies]\nwidget = ">=1.0.0-beta.1, <2"\n')
        code, out = fx.run()
        self.assertEqual(code, 1)
        self.assertIn("--precise 1.0.0-beta.2", out)

    def test_git_and_unknown_sources_are_rejected(self):
        fx = Fixture(self, {}, [
            app_on(),
            {"name": "gitdep", "version": "0.1.0", "source": "git+https://example.invalid/repo#abc"},
            {"name": "mirror", "version": "0.1.0", "source": "registry+https://mirror.invalid/index"},
        ], {})
        code, out = fx.run()
        self.assertEqual(code, 1)
        self.assertIn("gitdep 0.1.0: git dependency", out)
        self.assertIn("mirror 0.1.0: unknown source", out)

    def test_offline_without_index_file_is_an_error_not_a_pass(self):
        fx = Fixture(self, {"widget": "1"}, [app_on("widget"), {"name": "widget", "version": "1.0.0"}], {})
        code, out = fx.run()
        self.assertEqual(code, 2)
        self.assertIn("offline: no index file for widget", out)

    def test_offline_requires_index_dir(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(age.main(["Cargo.lock", "--offline"]), 2)


class RequirementTests(unittest.TestCase):
    def matches(self, req, version):
        return age.Requirement.parse(req).matches(age.Version.parse(version))

    def test_caret_rules(self):
        self.assertTrue(self.matches("1.2.3", "1.9.0"))
        self.assertFalse(self.matches("1.2.3", "1.2.2"))
        self.assertFalse(self.matches("1.2.3", "2.0.0"))
        self.assertTrue(self.matches("0.4.5", "0.4.9"))
        self.assertFalse(self.matches("0.4.5", "0.5.0"))
        self.assertTrue(self.matches("^0.0.3", "0.0.3"))
        self.assertFalse(self.matches("^0.0.3", "0.0.4"))
        self.assertTrue(self.matches("0", "0.9.9"))

    def test_tilde_wildcard_and_ranges(self):
        self.assertTrue(self.matches("~1.2.3", "1.2.9"))
        self.assertFalse(self.matches("~1.2.3", "1.3.0"))
        self.assertTrue(self.matches("1.*", "1.7.0"))
        self.assertFalse(self.matches("1.2.*", "1.3.0"))
        self.assertTrue(self.matches(">=1.2, <1.5", "1.4.9"))
        self.assertFalse(self.matches(">=1.2, <1.5", "1.5.0"))
        self.assertTrue(self.matches("<=1.2", "1.2.7"))
        self.assertTrue(self.matches("=1.2.3", "1.2.3"))
        self.assertFalse(self.matches("=1.2.3", "1.2.4"))
        self.assertTrue(self.matches("*", "3.0.0"))

    def test_prerelease_needs_matching_comparator(self):
        self.assertFalse(self.matches("1", "1.3.0-rc.1"))
        self.assertTrue(self.matches("^1.3.0-rc.1", "1.3.0-rc.2"))
        self.assertFalse(self.matches("^1.3.0-rc.1", "1.4.0-rc.1"))

    def test_semver_precedence(self):
        order = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0"]
        parsed = [age.Version.parse(v) for v in order]
        self.assertEqual(sorted(parsed, key=age.Version.key), parsed)


if __name__ == "__main__":
    unittest.main()

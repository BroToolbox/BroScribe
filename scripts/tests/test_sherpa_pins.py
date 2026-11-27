"""Tests for third_party/sherpa-onnx/pins.sh (sourced by fetch.sh and the build scripts).

Runs the shell functions in bash against the repository's pin files and
synthetic lock files and folders; nothing is downloaded.
"""

from __future__ import annotations

import shutil
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

from support import REPO

PINS = REPO / "third_party" / "sherpa-onnx" / "pins.sh"
TARGETS = ("osx-arm64", "linux-aarch64", "linux-x64", "win-x64")


def bash(script: str, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["bash", "-c", f'source "$0"; {script}', str(PINS), *args],
                          capture_output=True, text=True)


def pinned(file_name: str) -> dict[str, str]:
    lines = (REPO / "third_party" / "sherpa-onnx" / file_name).read_text().splitlines()
    return {archive: value for value, archive in (line.split() for line in lines if line.strip())}


@unittest.skipIf(shutil.which("bash") is None, "bash is required")
class SherpaPinTests(unittest.TestCase):
    def test_every_target_has_one_checksum_and_one_size(self):
        sums, sizes = pinned("SHA256SUMS"), pinned("ARCHIVE_SIZES")
        self.assertEqual(set(sums), set(sizes))
        for target in TARGETS:
            archive = bash('sherpa_archive "$1"', target).stdout.strip()
            self.assertIn(archive, sums)
            self.assertEqual(bash('sherpa_sha256 "$1"', archive).stdout.strip(), sums[archive])
            self.assertEqual(bash('sherpa_size "$1"', archive).stdout.strip(), sizes[archive])
            self.assertTrue(sizes[archive].isdigit() and int(sizes[archive]) > 0)

    def test_unknown_target_and_triple_fail(self):
        self.assertNotEqual(bash('sherpa_archive "$1"', "osx-x64").returncode, 0)
        self.assertNotEqual(bash('sherpa_target_for_triple "$1"', "x86_64-apple-darwin").returncode, 0)
        self.assertEqual(bash('sherpa_target_for_triple "$1"', "aarch64-apple-darwin").stdout.strip(), "osx-arm64")

    def test_marker_names_version_archive_and_checksum(self):
        marker = bash('sherpa_marker osx-arm64').stdout
        archive = bash('sherpa_archive osx-arm64').stdout.strip()
        self.assertIn(f"archive={archive}\n", marker)
        self.assertIn(f"sha256={pinned('SHA256SUMS')[archive]}\n", marker)

    def test_folder_is_verified_only_for_the_exact_current_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            self.assertNotEqual(bash('sherpa_lib_verified "$1" osx-arm64', tmp).returncode, 0)
            (folder / ".verified-2.0.1").write_text("")
            self.assertNotEqual(bash('sherpa_lib_verified "$1" osx-arm64', tmp).returncode, 0)
            marker = bash('sherpa_marker osx-arm64').stdout
            (folder / ".verified").write_text(marker)
            self.assertEqual(bash('sherpa_lib_verified "$1" osx-arm64', tmp).returncode, 0)
            self.assertNotEqual(bash('sherpa_lib_verified "$1" linux-x64', tmp).returncode, 0)
            (folder / ".verified").write_text(marker.replace("sha256=", "sha256=0"))
            self.assertNotEqual(bash('sherpa_lib_verified "$1" osx-arm64', tmp).returncode, 0)

    def test_lock_must_pin_the_same_binding_version(self):
        version = bash('printf %s "$SHERPA_VERSION"').stdout
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / "Cargo.lock"
            lock.write_text(f'[[package]]\nname = "sherpa-onnx-sys"\nversion = "{version}"\n')
            self.assertEqual(bash('sherpa_check_lock "$1"', str(lock)).returncode, 0)
            lock.write_text('[[package]]\nname = "sherpa-onnx-sys"\nversion = "9.9.9"\n')
            result = bash('sherpa_check_lock "$1"', str(lock))
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("9.9.9", result.stderr)
            self.assertNotEqual(bash('sherpa_check_lock "$1"', str(Path(tmp) / "missing.lock")).returncode, 0)

    def test_repository_lock_and_manifest_agree_with_the_pin(self):
        version = bash('printf %s "$SHERPA_VERSION"').stdout
        self.assertEqual(bash('sherpa_check_lock "$1"', str(REPO / "Cargo.lock")).returncode, 0)
        manifest = tomllib.loads((REPO / "Cargo.toml").read_text())
        self.assertEqual(manifest["workspace"]["dependencies"]["sherpa-onnx"], f"={version}")
        self.assertEqual(manifest["workspace"]["package"]["rust-version"], "1.100")


if __name__ == "__main__":
    unittest.main()

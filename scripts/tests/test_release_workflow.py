"""Validate the release workflow's pinning and privilege boundaries offline."""

import base64
import json
import os
import re
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path
import unittest
from unittest.mock import patch

from support import REPO, load_script


class ReleaseWorkflow(unittest.TestCase):
    def setUp(self):
        self.workflow = (REPO / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.header, self.jobs = self.workflow.split("\njobs:\n", 1)
        job_parts = re.split(r"^  ([a-z_]+):\n", self.jobs, flags=re.MULTILINE)
        self.jobs = dict(zip(job_parts[1::2], job_parts[2::2], strict=True))

    def test_only_artifact_actions_pinned_by_full_sha(self):
        actions = re.findall(r"(?m)^\s+uses:\s*(\S+)", self.workflow)
        self.assertEqual(2, len(actions))
        for action, name in zip(actions, ("upload-artifact", "download-artifact"), strict=True):
            self.assertRegex(action, rf"^actions/{name}@[0-9a-f]{{40}}$")
        self.assertIn('RUST_TOOLCHAIN: "1.100.0"', self.header)
        self.assertIn('[[ "$channel" == "$RUST_TOOLCHAIN" ]]', self.jobs["build"])
        self.assertIn('fetch.sh --target "$SHERPA_TARGET"', self.jobs["build"])
        self.assertIn('--locked --workspace --all-targets -- -D warnings', self.jobs["build"])
        self.assertEqual({"macos-15", "ubuntu-24.04", "ubuntu-24.04-arm", "windows-2025"},
                         set(re.findall(r"(?m)^\s+runner:\s*(\S+)", self.jobs["build"])))

    def test_build_is_read_only_and_publishing_is_tag_only_and_draft(self):
        self.assertIn("permissions: {}", self.header)
        self.assertIn("tags: ['v*']", self.header)
        self.assertIn("workflow_dispatch:", self.header)
        self.assertEqual(["draft"], [name for name, body in self.jobs.items() if "contents: write" in body])
        self.assertIn("contents: read", self.jobs["build"])
        self.assertIn("if: github.event_name == 'push' && github.ref_type == 'tag' && startsWith(github.ref_name, 'v')", self.jobs["draft"])
        self.assertIn("needs: build", self.jobs["draft"])
        self.assertIn("--verify-tag --draft", self.jobs["draft"])
        self.assertNotIn("secrets.UPDATE_SIGNING_KEY", self.jobs["build"])
        self.assertEqual(re.findall(r"secrets\.([A-Z_]+)", self.jobs["draft"]), ["UPDATE_SIGNING_KEY"])

    def test_draft_signs_and_verifies_only_after_all_packaged_builds(self):
        draft = self.jobs["draft"]
        preparation, signing = draft.split("      - name: Sign and verify the update envelope", 1)
        signing, publication = signing.split("      - name: Stage draft for owner review", 1)
        self.assertIn("actions/download-artifact@", preparation)
        self.assertIn("manifest --packages artifacts --tag", preparation)
        self.assertIn("umask 077", signing)
        self.assertIn('UPDATE_SIGNING_KEY is absent', signing)
        self.assertIn('rm -f -- "$work/seed"', signing)
        self.assertIn('update sign-manifest --key-file "$work/seed"', signing)
        self.assertIn('update verify-manifest --envelope-stdin', signing)
        self.assertLess(signing.index("verify-manifest"), signing.index("broscribe-update.json"))
        # Both gates name the exact asset set instead of counting files.
        gate = 'python3 scripts/release-metadata.py assets --tag "$GITHUB_REF_NAME"'
        self.assertIn(gate + ' "${assets[@]}"', preparation)
        self.assertIn(gate + ' ${envelope[@]+"${envelope[@]}"} "${assets[@]}"', publication)
        self.assertIn('envelope=(--envelope)', publication)
        self.assertNotRegex(draft, r"#assets\[@\]\}\" == \"?1[0-9]")
        self.assertIn('assets+=("$RUNNER_TEMP/broscribe-update.json")', publication)
        self.assertLess(publication.index("release-metadata.py assets"), publication.index("gh release create"))

    def test_update_signing_key_accepts_wrapped_base64_and_rejects_invalid_characters(self):
        signing = self.jobs["draft"].split("      - name: Sign and verify the update envelope", 1)[1]
        python = textwrap.dedent(signing.split("<<'PY'\n", 1)[1].split("\n          PY", 1)[0])
        seed = bytes(range(32))
        encoded = base64.b64encode(seed).decode()
        with tempfile.TemporaryDirectory() as work:
            output = Path(work) / "seed"
            run = lambda key: subprocess.run(
                [sys.executable, "-c", python, str(output)],
                env={**os.environ, "UPDATE_SIGNING_KEY": key}, capture_output=True,
            )
            result = run(f" \t{encoded[:17]}\r\n{encoded[17:]}\n")
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertEqual(seed, output.read_bytes())
            output.unlink()
            self.assertNotEqual(0, run(f"!{encoded}").returncode)
            self.assertFalse(output.exists())

    def test_refresh_targets_published_assets_and_only_uploads_after_verification(self):
        refresh = (REPO / ".github/workflows/refresh-update.yml").read_text(encoding="utf-8")
        self.assertIn("workflow_dispatch:", refresh)
        self.assertRegex(refresh, r"cron: '[^']+ 1 \* \*'")
        self.assertIn("permissions: {}", refresh)
        self.assertEqual(1, refresh.count("contents: write"))
        self.assertNotIn("uses:", refresh)
        self.assertIn('git fetch --quiet --no-tags --depth=1 origin "${GITHUB_SHA}"', refresh)
        self.assertIn('RUSTUP_AUTO_INSTALL: "0"', refresh)
        self.assertIn('CARGO_INCREMENTAL: "0"', refresh)
        self.assertIn('PYTHONUTF8: "1"', refresh)
        self.assertIn('rust-toolchain.toml', refresh)
        self.assertIn('timeout-minutes:', refresh)
        self.assertIn(
            "    if: github.repository == 'BroToolbox/BroScribe' && "
            "(github.event_name == 'schedule' || github.ref_name == github.event.repository.default_branch)\n",
            refresh,
        )
        self.assertIn("gh api \"repos/$GH_REPO/releases/latest\"", refresh)
        self.assertIn("scripts/refresh-update.py", refresh)
        self.assertIn("verify-macos-artifact.py", refresh)
        self.assertIn("UPDATE_SIGNING_KEY is required", refresh)
        self.assertIn("github.event_name == 'workflow_dispatch' && inputs.allow_missing_baseline", refresh)
        self.assertLess(refresh.index("Build a signing CLI"), refresh.index("Authenticate the baseline"))
        self.assertIn("umask 077", refresh)
        self.assertLess(refresh.index('rm -f -- "$work/seed"'), refresh.index("update verify-manifest"))
        self.assertLess(refresh.index("update verify-manifest"), refresh.index('--publish "'))
        self.assertLess(refresh.index("latest release or its assets changed"), refresh.index('--publish "'))

    def test_unsigned_fallback_and_temporary_notary_keychain(self):
        build = self.jobs["build"]
        self.assertIn('BROSCRIBE_SIGN_IDENTITY=- scripts/sign-macos.sh', build)
        self.assertIn('scripts/package-macos-preview.py', build)
        self.assertIn("status=adhoc", build)
        self.assertIn("status=unsigned", build)
        self.assertIn('security delete-keychain "$keychain"', build)
        self.assertIn('BROSCRIBE_NOTARY_KEYCHAIN="$keychain" scripts/notarize-macos.sh', build)
        notary = (REPO / "scripts/notarize-macos.sh").read_text(encoding="utf-8")
        self.assertIn('notary_auth+=(--keychain "$BROSCRIBE_NOTARY_KEYCHAIN")', notary)
        for command in ("submit", "log"):
            self.assertRegex(notary, rf'notarytool {command} [^\n]+ "\$\{{notary_auth\[@\]\}}"')

    def test_macos_compiles_before_exposing_signing_credentials(self):
        build = self.jobs["build"]
        unsigned, signing = build.split("      - name: Sign, optionally notarize, and verify macOS", 1)
        self.assertIn("scripts/bundle-macos.sh --build-unsigned", unsigned)
        self.assertNotRegex(unsigned, r"secrets\.(MACOS|NOTARY)_")
        self.assertNotIn("scripts/bundle-macos.sh", signing)
        self.assertIn('BROSCRIBE_SIGN_IDENTITY="$MACOS_SIGN_IDENTITY" scripts/sign-macos.sh', signing)
        self.assertLess(signing.index("security import"), signing.index("scripts/notarize-macos.sh"))

    def test_only_macos_secret_setup_uses_a_private_umask(self):
        signing = self.jobs["build"].split("      - name: Sign, optionally notarize, and verify macOS", 1)[1]
        signing = signing.split("      - name:", 1)[0]
        public_preview, secrets, public_release = re.split(r"(?m)^ +umask (?:077|022)\n", signing)[1:]
        self.assertIn("scripts/package-macos-preview.py", public_preview)
        self.assertIn("security create-keychain", secrets)
        self.assertIn('work / filename', secrets)
        self.assertNotIn("scripts/sign-macos.sh", secrets)
        self.assertIn("scripts/sign-macos.sh", public_release)
        self.assertIn("scripts/notarize-macos.sh", public_release)

    def test_ci_windows_uses_the_release_msvc_setup_and_prioritizes_the_linker(self):
        ci = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8").split("\n  windows:\n", 1)[1]
        marker = "      - name: Native Windows prerequisites\n"
        ci_setup = ci.split(marker, 1)[1].split("      - name:", 1)[0]
        release_setup = self.jobs["build"].split(marker, 1)[1].split("      - name:", 1)[0]
        self.assertEqual(ci_setup, release_setup.replace("        if: runner.os == 'Windows'\n", "", 1))
        builds = re.findall(r'msvc_bin="\$\(cygpath -u "\$BROSCRIBE_MSVC_BIN"\)"\n'
                            r' +export PATH="\$msvc_bin:\$PATH"\n'
                            r' +scripts/build-target.sh x86_64-pc-windows-msvc (\w+)', ci)
        # Debug lane: Clippy, then tests (which also build the examples).
        # Release lane: the shipped libraries and executables only.
        self.assertEqual(builds, ["clippy", "test", "build"])
        self.assertIn("x86_64-pc-windows-msvc build --locked --workspace --release\n", ci)
        self.assertNotIn("CARGO_BUILD_JOBS", ci.split("\n  merge-gate:\n", 1)[0])

    def test_windows_timestamp_is_optional_and_a_missing_requested_timestamp_fails(self):
        signing = self.jobs["build"].split("        id: windows_sign", 1)[1].split("      - name:", 1)[0]
        self.assertIn("WINDOWS_TIMESTAMP_SERVER: ${{ vars.WINDOWS_TIMESTAMP_SERVER }}", signing)
        self.assertIn("$status = 'authenticode-untimestamped'", signing)
        self.assertIn("if ($env:WINDOWS_TIMESTAMP_SERVER)", signing)
        self.assertIn("$timestamp.TimestampServer = $env:WINDOWS_TIMESTAMP_SERVER", signing)
        self.assertIn("Set-AuthenticodeSignature -FilePath $path -Certificate $certificate -HashAlgorithm SHA256 @timestamp",
                      signing)
        self.assertIn("$timestamp.Count -and !$signed.TimeStamperCertificate", signing)


refresh = load_script("refresh-update.py")
CLI = Path("/synthetic/broscribe")


def envelope(published_at):
    return json.dumps({"manifest": json.dumps({"build": 42, "published_at": published_at})}).encode()


class RefreshUpdate(unittest.TestCase):
    def test_missing_baseline_requires_explicit_dispatch(self):
        release = {"assets": []}
        for event, opted_in in (("schedule", False), ("schedule", True), ("workflow_dispatch", False)):
            with self.assertRaisesRegex(ValueError, "no update envelope"):
                refresh.require_baseline(release, event, opted_in)
        with patch("sys.stderr") as log:
            self.assertFalse(refresh.require_baseline(release, "workflow_dispatch", True))
            self.assertIn("Explicit manual bootstrap", log.write.call_args_list[0].args[0])
        self.assertTrue(refresh.require_baseline({"assets": [{"name": refresh.ENVELOPE}]}, "schedule", False))

    def test_baseline_must_pass_cli_verification_before_the_manifest_is_read(self):
        with tempfile.TemporaryDirectory() as folder:
            baseline = Path(folder) / refresh.ENVELOPE
            baseline.write_bytes(b"synthetic forged envelope")
            command = [str(CLI), "update", "verify-manifest", "--envelope-stdin", "--allow-expired"]
            refused = subprocess.CalledProcessError(1, command)
            with patch.object(refresh.subprocess, "run", side_effect=refused) as run:
                with self.assertRaises(subprocess.CalledProcessError):
                    refresh.authenticate_baseline(baseline, CLI)
                run.assert_called_once_with(command, input=baseline.read_bytes(),
                                            stdout=subprocess.DEVNULL, check=True)
            baseline.write_bytes(envelope("2000-01-01T00:00:00Z"))
            with patch.object(refresh.subprocess, "run") as run:
                self.assertEqual(42, refresh.authenticate_baseline(baseline, CLI)["build"])
                run.assert_called_once_with(command, input=baseline.read_bytes(),
                                            stdout=subprocess.DEVNULL, check=True)

    def test_failed_upload_or_changed_download_restores_the_backup_and_fails(self):
        for failure in ("upload", "download", None):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                backup = root / "previous" / refresh.ENVELOPE
                backup.parent.mkdir()
                backup.write_bytes(envelope("2000-01-01T00:00:00Z"))
                signed = root / refresh.ENVELOPE
                signed.write_bytes(envelope("2026-11-26T00:00:00Z"))
                uploaded = []
                verified = []

                def run(command, *, input=None, **_):
                    if input is not None:
                        verified.append((input, "--allow-expired" in command))
                    elif command[2] == "upload":
                        uploaded.append(Path(command[4]))
                        if failure == "upload" and uploaded == [signed]:
                            raise subprocess.CalledProcessError(1, command)
                    else:
                        public = Path(command[-1]) / refresh.ENVELOPE
                        asset = backup if failure == "download" else uploaded[-1]
                        public.write_bytes(asset.read_bytes())

                with patch.object(refresh.subprocess, "run", side_effect=run):
                    if failure is None:
                        refresh.publish_envelope("v1.2.3", CLI, signed, backup)
                        self.assertEqual([signed], uploaded)
                        self.assertEqual((signed.read_bytes(), False), verified[-1])
                    else:
                        with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                            refresh.publish_envelope("v1.2.3", CLI, signed, backup)
                        self.assertEqual([signed, backup], uploaded)
                        self.assertEqual((backup.read_bytes(), True), verified[-1])

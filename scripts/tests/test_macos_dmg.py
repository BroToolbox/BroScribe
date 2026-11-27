"""The macOS disk image: verifier, provenance, release asset sets, packaging
script and refresh check.

Everything here is offline and synthetic. hdiutil, codesign, xcrun and spctl
are replaced by fakes (the verifier's `run_tool`, or stub executables on PATH
for the shell scripts, with stub sibling scripts), so the tests run on Linux
too; nothing is signed, notarized, mounted or sent anywhere.
"""

from __future__ import annotations

import hashlib
import json
import os
import plistlib
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

from support import REPO, SCRIPTS, load_script
from test_release_artifacts import DEV_ID, make_bundle, quiet, write_zip_entry
from test_release_metadata import NOW, SIGNING, zip_app
import test_desktop_artifacts as desktop_tests

metadata = load_script("release-metadata.py")
verify = load_script("verify-macos-artifact.py")
refresh = load_script("refresh-update.py")

STEM = "BroScribe-0.2.0-12-macos-arm64"
TEAM = metadata.MACOS_TEAM
DEVICE = "/dev/disk12"


def completed(command, code=0, stdout=b"", stderr=b""):
    return subprocess.CompletedProcess(command, code, stdout, stderr)


def distribution_modes(app: Path) -> None:
    """The modes a umask-022 build gives the bundle (what the ZIP records)."""
    for path in [app, *app.rglob("*")]:
        if path.is_symlink():
            continue
        relative = "BroScribe.app/" + path.relative_to(app).as_posix()
        path.chmod(0o755 if path.is_dir() or relative in verify.EXECUTABLES else 0o644)


def write_volume(root: Path, app: Path) -> Path:
    """The disk image's file system as hdiutil would build it from the stage."""
    volume = root / "volume"
    volume.mkdir()
    shutil.copytree(app, volume / "BroScribe.app", symlinks=True)
    distribution_modes(volume / "BroScribe.app")
    (volume / "Applications").symlink_to("/Applications")
    return volume


# hdiutil attach -plist on macOS 27.2 for an APFS ULFO image: the image's
# whole disk (GUID scheme), its APFS partition, the synthesized container and
# the mounted volume.
SYNTH_CONTAINER = "/dev/disk13"


def image_entities(device=DEVICE, container=SYNTH_CONTAINER) -> list[dict]:
    return [{"dev-entry": device, "content-hint": "GUID_partition_scheme"},
            {"dev-entry": device + "s1", "content-hint": "Apple_APFS"},
            {"dev-entry": container, "content-hint": "EF57347C-0000-11AA-AA11-00306543ECAC"}]


class FakeTools:
    """hdiutil, codesign, xcrun and spctl for verify.run_tool. Attaching
    copies the synthetic volume to the requested mount point; detaching
    empties it again. The disk image (a path ending in .dmg) and the mounted
    app fail separately."""

    def __init__(self, volume: Path, *, image_format="ULFO", encrypted=False, licence=False,
                 dmg_signature_ok=True, app_signature_ok=True, timestamp=True,
                 dmg_stapled=True, app_stapled=True, gatekeeper=True,
                 authorities=None, team=TEAM, volume_kind="apfs", entities=None,
                 attach_code=0, detach_failures=0):
        self.volume, self.calls = volume, []
        self.image_format, self.encrypted, self.licence = image_format, encrypted, licence
        self.dmg_signature_ok, self.app_signature_ok, self.timestamp = dmg_signature_ok, app_signature_ok, timestamp
        self.dmg_stapled, self.app_stapled, self.gatekeeper = dmg_stapled, app_stapled, gatekeeper
        self.authorities = SIGNING["authorities"] if authorities is None else authorities
        self.team, self.volume_kind = team, volume_kind
        self.entities = image_entities() if entities is None else entities
        self.attach_code, self.detach_failures = attach_code, detach_failures
        self.mounted: Path | None = None
        self.seen_root: list[str] = []

    @property
    def attached(self) -> bool:
        return any(call[:2] == ["hdiutil", "attach"] for call in self.calls)

    def __call__(self, command, *, timeout=None):
        self.calls.append(list(command))
        assert timeout is None or timeout > 0
        tool, action = command[0], command[1] if len(command) > 1 else ""
        is_dmg = command[-1].endswith(".dmg")
        if (tool, action) == ("hdiutil", "imageinfo"):
            return completed(command, 0, plistlib.dumps({
                "Format": self.image_format,
                "Properties": {"Encrypted": self.encrypted, "Software License Agreement": self.licence}}))
        if (tool, action) == ("hdiutil", "attach"):
            mountpoint = Path(command[command.index("-mountpoint") + 1])
            entities = [dict(entity) for entity in self.entities]
            if self.attach_code:
                return completed(command, self.attach_code, plistlib.dumps({"system-entities": entities}),
                                 b"hdiutil: attach failed - synthetic")
            for child in self.volume.iterdir():
                target = mountpoint / child.name
                if child.is_symlink():
                    target.symlink_to(os.readlink(child))
                elif child.is_dir():
                    shutil.copytree(child, target, symlinks=True)
                else:
                    shutil.copy2(child, target)
            self.mounted = mountpoint
            self.seen_root = sorted(os.listdir(mountpoint))
            volume = {"dev-entry": SYNTH_CONTAINER + "s1", "mount-point": str(mountpoint),
                      "content-hint": "41504653-0000-11AA-AA11-00306543ECAC"}
            if self.volume_kind is not None:
                volume["volume-kind"] = self.volume_kind
            entities.append(volume)
            return completed(command, 0, plistlib.dumps({"system-entities": entities}))
        if (tool, action) == ("hdiutil", "detach"):
            if self.detach_failures:
                self.detach_failures -= 1
                return completed(command, 16, b"", b"hdiutil: couldn't eject - Resource busy")
            if self.mounted is not None:
                for child in self.mounted.iterdir():
                    if child.is_dir() and not child.is_symlink():
                        shutil.rmtree(child)
                    else:
                        child.unlink()
            return completed(command, 0)
        if tool == "codesign" and "--display" in command:
            lines = [f"Authority={authority}" for authority in self.authorities] + [f"TeamIdentifier={self.team}"]
            if self.timestamp:
                lines.append("Timestamp=27 Nov 2026 at 12:00:00")
            return completed(command, 0, b"", "\n".join(lines).encode())
        if tool == "codesign":
            ok = self.dmg_signature_ok if is_dmg else self.app_signature_ok
            return completed(command, 0 if ok else 3, b"", b"" if ok else
                             b"test-requirement: code failed to satisfy specified code requirement(s)")
        if tool == "xcrun":
            ok = self.dmg_stapled if is_dmg else self.app_stapled
            return completed(command, 0 if ok else 65, b"" if ok else b"does not have a ticket stapled to it")
        if tool == "spctl":
            return completed(command, 0 if self.gatekeeper else 3, b"", b"accepted" if self.gatekeeper else b"rejected")
        raise AssertionError(f"unexpected tool call {command}")


class DiskImageFixture(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.app = make_bundle(self.root)
        distribution_modes(self.app)
        self.zip = zip_app(self.app, self.dist / f"{STEM}.zip")
        self.volume = write_volume(self.root, self.app)
        self.dmg = self.dist / f"{STEM}.dmg"
        self.write_dmg(b"synthetic disk image")
        patcher = mock.patch.object(verify, "DETACH_PAUSE", 0)
        patcher.start()
        self.addCleanup(patcher.stop)

    def write_dmg(self, data: bytes) -> None:
        self.dmg.write_bytes(data)
        digest = hashlib.sha256(data).hexdigest()
        self.dmg.with_name(self.dmg.name + ".sha256").write_text(f"{digest}  {self.dmg.name}\n")

    def report(self, **changes) -> dict:
        report = {"format": "ULFO", "filesystem": "APFS",
                  "signing": {"authorities": SIGNING["authorities"], "team_identifier": TEAM},
                  "notarization": {"submission_id": "00000000-0000-0000-0000-000000000001",
                                   "status": "Accepted", "stapled": True}}
        report.update(changes)
        return report

    def provenance(self, report=None) -> Path:
        signing = SIGNING | {"team_identifier": TEAM}
        record = metadata.provenance(self.zip, self.app, signing, "synthetic-id", "Accepted", self.dist)
        record["disk_image"] = metadata.disk_image_record(self.dmg, self.zip, report or self.report(), signing)
        path = self.dist / f"{STEM}.provenance.json"
        path.write_text(json.dumps(record))
        return path

    def run_verifier(self, tools: FakeTools, *extra: str) -> tuple[int, str]:
        with mock.patch.object(verify, "run_tool", tools):
            return quiet(verify.main, ["--dmg", str(self.dmg), "--compare-zip", str(self.zip), *extra])


def zip_tree(app: Path, artifact: Path) -> Path:
    """zip_app, keeping symbolic links as link entries (as ditto does)."""
    with zipfile.ZipFile(artifact, "w", zipfile.ZIP_DEFLATED) as archive:
        write_zip_entry(archive, "BroScribe.app/", b"")
        for path in sorted(app.rglob("*")):
            name = (Path("BroScribe.app") / path.relative_to(app)).as_posix()
            if path.is_symlink():
                write_zip_entry(archive, name, os.readlink(path).encode(), mode=0o120777)
            elif path.is_file():
                write_zip_entry(archive, name, path.read_bytes())
    return artifact


class DiskImageVerifierTests(DiskImageFixture):
    def test_identical_app_passes_and_the_image_is_mounted_privately_and_detached(self):
        tools = FakeTools(self.volume)
        code, err = self.run_verifier(tools, "--require-complete-licences")
        self.assertEqual(code, 0, err)
        attach = next(call for call in tools.calls if call[:2] == ["hdiutil", "attach"])
        for flag in ("-readonly", "-nobrowse", "-noautoopen", "-plist"):
            self.assertIn(flag, attach)
        mountpoint = Path(attach[attach.index("-mountpoint") + 1])
        self.assertTrue(mountpoint.parent.name.startswith("broscribe-dmg-"))
        self.assertTrue(mountpoint.is_relative_to(tempfile.gettempdir()))
        self.assertEqual(tools.calls[-1], ["hdiutil", "detach", DEVICE])
        self.assertFalse(mountpoint.exists(), "the private mount folder is removed after detaching")
        self.assertEqual(tools.seen_root, ["Applications", "BroScribe.app"])
        self.assertFalse(any(call[0] in ("codesign", "xcrun", "spctl") for call in tools.calls))

    def test_signed_stapled_image_passes_with_pinned_team_and_gatekeeper(self):
        provenance = self.provenance()
        tools = FakeTools(self.volume)
        code, err = self.run_verifier(tools, "--provenance", str(provenance), "--require-complete-licences",
                                      "--codesign", "--require-stapled")
        self.assertEqual(code, 0, err)
        requirement = "=" + verify.DEVELOPER_ID_REQUIREMENT.format(team=TEAM)
        self.assertIn(f'certificate leaf[subject.OU] = "{TEAM}"', requirement)
        self.assertIn("certificate 1[field.1.2.840.113635.100.6.2.6] exists", requirement)
        attach = next(i for i, call in enumerate(tools.calls) if call[:2] == ["hdiutil", "attach"])
        outer = [["codesign", "--verify", "--strict", "--verbose=2", "-R", requirement, str(self.dmg)],
                 ["codesign", "--display", "--verbose=4", str(self.dmg)],
                 ["xcrun", "stapler", "validate", str(self.dmg)],
                 ["spctl", "-a", "-t", "open", "--context", "context:primary-signature", "-v", str(self.dmg)]]
        # The image's own signature, timestamp, ticket and Gatekeeper verdict
        # are checked before it is attached.
        self.assertEqual([call for call in tools.calls[:attach] if call[0] != "hdiutil"], outer)
        inner = tools.calls[attach + 1:-1]
        self.assertEqual(len(inner), 2, inner)
        self.assertEqual(inner[0][:3], ["codesign", "--verify", "--deep"])
        self.assertTrue(inner[0][-1].endswith("/BroScribe.app") and "-R" in inner[0])
        self.assertEqual(inner[1][:3], ["xcrun", "stapler", "validate"])
        self.assertTrue(inner[1][-1].endswith("/BroScribe.app"))
        # Native checks of the mounted app happen before it is detached.
        self.assertEqual(tools.calls[-1], ["hdiutil", "detach", DEVICE])

    def test_any_difference_from_the_zip_app_is_refused(self):
        worker = self.volume / "BroScribe.app/Contents/MacOS/broscribe-worker"
        notice = self.volume / "BroScribe.app/Contents/Resources/NOTICE"
        cases = {
            "different content": lambda: worker.write_bytes(worker.read_bytes()[:-1] + b"\x01"),
            "has mode 0700": lambda: worker.chmod(0o700),
            "only in the disk image": lambda: (worker.parent / "extra").write_bytes(b"x"),
            "missing from the disk image": notice.unlink,
            "is a link in the disk image but a file": lambda: (notice.unlink(), notice.symlink_to("LICENSE")),
            "setuid": lambda: worker.chmod(0o4755),
        }
        for message, change in cases.items():
            with self.subTest(message):
                shutil.rmtree(self.volume)
                self.volume = write_volume(self.root, self.app)
                change()
                code, err = self.run_verifier(FakeTools(self.volume))
                self.assertEqual(code, 1)
                self.assertIn("differs from the ZIP", err)
                self.assertIn(message, err)

    def test_a_different_link_target_inside_the_app_is_refused(self):
        # The same link in both, pointing elsewhere on the disk image only.
        (self.app / "Contents/Resources/Current").symlink_to("LICENSE")
        zip_tree(self.app, self.zip)
        self.write_dmg(b"synthetic disk image")
        shutil.rmtree(self.volume)
        self.volume = write_volume(self.root, self.app)
        self.assertEqual(self.run_verifier(FakeTools(self.volume))[0], 0)
        link = self.volume / "BroScribe.app/Contents/Resources/Current"
        link.unlink()
        link.symlink_to("NOTICE")
        code, err = self.run_verifier(FakeTools(self.volume))
        self.assertEqual(code, 1)
        self.assertIn("BroScribe.app/Contents/Resources/Current has a different target", err)

    def test_root_must_hold_only_the_app_and_the_applications_link(self):
        link = self.volume / "Applications"
        cases = {
            ".DS_Store": (lambda: (self.volume / ".DS_Store").write_bytes(b"x"), "'.DS_Store' (Finder metadata)"),
            "AppleDouble": (lambda: (self.volume / "._BroScribe.app").write_bytes(b"x"), "(AppleDouble metadata)"),
            "__MACOSX": (lambda: (self.volume / "__MACOSX").mkdir(), "'__MACOSX'"),
            ".Trashes": (lambda: (self.volume / ".Trashes").mkdir(), "'.Trashes'"),
            "background": (lambda: (self.volume / ".background").mkdir(), "'.background'"),
            # Not Finder or AppleDouble metadata, and still refused: the
            # allowlist is empty.
            ".fseventsd": (lambda: (self.volume / ".fseventsd").mkdir(),
                           "'.fseventsd' (a file system event log, which this APFS image should not have; "
                           "DMG_HIDDEN_ALLOWED allows no hidden entry)"),
            "other hidden": (lambda: (self.volume / ".hidden").write_bytes(b"x"),
                             "'.hidden' (a hidden entry; DMG_HIDDEN_ALLOWED allows none)"),
            "extra file": (lambda: (self.volume / "README.txt").write_bytes(b"x"), "must hold exactly"),
            "link elsewhere": (lambda: (link.unlink(), link.symlink_to("/Applications/Utilities")),
                               "does not point to /Applications"),
            "relative link": (lambda: (link.unlink(), link.symlink_to("../Applications")),
                              "does not point to /Applications"),
            "folder instead of link": (lambda: (link.unlink(), link.mkdir()), "is not a symbolic link"),
            "missing link": (link.unlink, "must hold exactly"),
            "app as link": (lambda: (shutil.rmtree(self.volume / "BroScribe.app"),
                                     (self.volume / "BroScribe.app").symlink_to("/Applications/BroScribe.app")),
                            "is not a folder"),
        }
        self.assertEqual(verify.DMG_HIDDEN_ALLOWED, {})
        for label, (change, message) in cases.items():
            with self.subTest(label):
                shutil.rmtree(self.volume)
                self.volume = write_volume(self.root, self.app)
                change()
                code, err = self.run_verifier(FakeTools(self.volume))
                self.assertEqual(code, 1, label)
                self.assertIn(message, err)
                self.assertNotIn("Finder or AppleDouble", err)

    def test_the_applications_link_is_never_followed(self):
        # A link to a real folder full of files: following it would hash or
        # scan that folder. Only the link's own target string is read.
        elsewhere = self.root / "elsewhere"
        elsewhere.mkdir()
        (elsewhere / "secret").write_bytes(b"/Users/synthetic-builder/private")
        link = self.volume / "Applications"
        link.unlink()
        link.symlink_to(elsewhere)
        code, err = self.run_verifier(FakeTools(self.volume), "--forbid", "/Users/synthetic-builder")
        self.assertEqual(code, 1)
        self.assertIn("does not point to /Applications", err)
        self.assertNotIn("secret", err)

    def test_builder_paths_in_the_mounted_app_are_refused(self):
        for target in (self.app, self.volume / "BroScribe.app"):
            (target / "Contents/Resources/NOTICE").write_bytes(b"built in /Users/synthetic-builder/src")
        zip_app(self.app, self.zip)
        distribution_modes(self.volume / "BroScribe.app")
        code, err = self.run_verifier(FakeTools(self.volume), "--forbid", "/Users/synthetic-builder")
        self.assertEqual(code, 1)
        self.assertIn("Contents/Resources/NOTICE embeds a forbidden builder or home path", err)
        self.assertNotIn("differs from the ZIP", err)

    def test_checksum_sidecar_and_name_are_required(self):
        sidecar = self.dmg.with_name(self.dmg.name + ".sha256")
        sidecar.write_text(f"{'0' * 64}  {self.dmg.name}\n")
        code, err = self.run_verifier(FakeTools(self.volume))
        self.assertEqual(code, 1)
        self.assertIn(".dmg.sha256 does not match", err)
        sidecar.unlink()
        self.assertIn("no BroScribe-0.2.0-12-macos-arm64.dmg.sha256", self.run_verifier(FakeTools(self.volume))[1])
        other = self.dist / "Other.dmg"
        self.write_dmg(b"synthetic disk image")
        self.dmg.rename(other)
        with mock.patch.object(verify, "run_tool", FakeTools(self.volume)):
            code, err = quiet(verify.main, ["--dmg", str(other), "--compare-zip", str(self.zip)])
        self.assertEqual(code, 1)
        self.assertIn("is not the disk image of", err)

    def test_image_format_encryption_and_licence_are_checked(self):
        for kwargs, message in (({"image_format": "UDZO"}, "not ULFO"), ({"encrypted": True}, "encrypted"),
                                ({"licence": True}, "licence agreement")):
            with self.subTest(kwargs=kwargs):
                code, err = self.run_verifier(FakeTools(self.volume, **kwargs))
                self.assertEqual(code, 1)
                self.assertIn(message, err)

    def test_nothing_is_attached_when_an_outer_check_fails(self):
        provenance = self.provenance()
        native = ("--provenance", str(provenance), "--codesign", "--require-stapled")
        sidecar = self.dmg.with_name(self.dmg.name + ".sha256")
        cases = [
            ("checksum", {}, lambda: sidecar.write_text(f"{'0' * 64}  {self.dmg.name}\n"), ".dmg.sha256 does not match"),
            ("no checksum", {}, sidecar.unlink, "no BroScribe-0.2.0-12-macos-arm64.dmg.sha256"),
            ("format", {"image_format": "UDZO"}, None, "not ULFO"),
            ("encryption", {"encrypted": True}, None, "encrypted"),
            ("licence", {"licence": True}, None, "licence agreement"),
            ("signature", {"dmg_signature_ok": False}, None, f"Developer ID Application of team {TEAM}"),
            ("timestamp", {"timestamp": False}, None, "no secure timestamp"),
            ("ticket", {"dmg_stapled": False}, None, "stapler validate failed on the disk image"),
            ("Gatekeeper", {"gatekeeper": False}, None, "Gatekeeper refuses the disk image"),
            ("signer", {"team": "SYNTH12345"}, None, "is not the one in the provenance"),
        ]
        for label, kwargs, change, message in cases:
            with self.subTest(label):
                self.write_dmg(b"synthetic disk image")
                provenance = self.provenance()
                if change:
                    change()
                tools = FakeTools(self.volume, **kwargs)
                code, err = self.run_verifier(tools, *native)
                self.assertEqual(code, 1)
                self.assertIn(message, err)
                self.assertIn("was not attached", err)
                self.assertFalse(tools.attached, f"{label}: the image was attached")
                self.assertFalse(any(call[:2] == ["hdiutil", "detach"] for call in tools.calls))

    def test_mounted_app_signature_and_ticket_failures_are_reported_then_detached(self):
        provenance = self.provenance()
        native = ("--provenance", str(provenance), "--codesign", "--require-stapled")
        for kwargs, message in (({"app_signature_ok": False}, "codesign --verify failed on the mounted app"),
                                ({"app_stapled": False}, "stapler validate failed on the mounted app")):
            with self.subTest(message):
                tools = FakeTools(self.volume, **kwargs)
                code, err = self.run_verifier(tools, *native)
                self.assertEqual(code, 1)
                self.assertIn(message, err)
                self.assertTrue(tools.attached)
                self.assertEqual(tools.calls[-1], ["hdiutil", "detach", DEVICE])

    def test_provenance_must_describe_this_image_and_this_zip(self):
        provenance = self.provenance()
        record = json.loads(provenance.read_text())
        cases = {
            "name, SHA-256 or size": lambda r: r["disk_image"].update(sha256="0" * 64),
            "must be ULFO with APFS": lambda r: r["disk_image"].update(format="UDZO"),
            "not notarized and stapled": lambda r: r["disk_image"]["notarization"].update(stapled=False),
            "different teams": lambda r: r["disk_image"]["signing"].update(team_identifier="SYNTH12345"),
            "does not describe the --compare-zip": lambda r: r["artifact"].update(sha256="0" * 64),
            "has no disk_image": lambda r: r.pop("disk_image"),
            "builder or home path": lambda r: r.update(note="/Users/synthetic-builder/x"),
        }
        for message, change in cases.items():
            with self.subTest(message):
                altered = json.loads(json.dumps(record))
                change(altered)
                provenance.write_text(json.dumps(altered))
                tools = FakeTools(self.volume)
                code, err = self.run_verifier(tools, "--provenance", str(provenance),
                                              "--forbid", "/Users/synthetic-builder")
                self.assertEqual(code, 1)
                self.assertIn(message, err)
                self.assertFalse(tools.attached)

    def test_a_provenance_that_is_not_an_object_is_refused_before_attaching(self):
        provenance = self.provenance()
        for content in ("null", "[]", '"text"'):
            with self.subTest(content):
                provenance.write_text(content)
                tools = FakeTools(self.volume)
                code, err = self.run_verifier(tools, "--provenance", str(provenance))
                self.assertEqual(code, 2)
                self.assertIn("is not a JSON object", err)
                self.assertFalse(tools.attached)

    def test_provenance_disk_image_is_validated_as_when_it_was_recorded(self):
        record = json.loads(self.provenance().read_text())
        path = self.dist / f"{STEM}.provenance.json"
        cases = {
            "no signing team": lambda r: (r["disk_image"]["signing"].update(team_identifier=""),
                                          r["signing"].update(team_identifier="")),
            "different teams": lambda r: r["disk_image"]["signing"].update(team_identifier="SYNTH12345"),
            "not signed by a Developer ID Application":
                lambda r: r["disk_image"]["signing"].update(authorities=["Apple Development: Synthetic"]),
            "no notarization submission id": lambda r: r["disk_image"]["notarization"].update(submission_id=""),
            "not notarized and stapled": lambda r: r["disk_image"]["notarization"].update(status="In Progress"),
            "must be ULFO with APFS": lambda r: r["disk_image"].update(filesystem="HFS+"),
        }
        for message, change in cases.items():
            with self.subTest(message):
                altered = json.loads(json.dumps(record))
                change(altered)
                path.write_text(json.dumps(altered))
                tools = FakeTools(self.volume)
                code, err = self.run_verifier(tools, "--provenance", str(path))
                self.assertEqual(code, 1)
                self.assertIn("provenance disk_image: the disk image", err)
                self.assertIn(message, err)
                self.assertFalse(tools.attached)

    def test_codesign_pins_the_recorded_team_to_macos_team(self):
        # A self-consistent record of another team passes the record rules
        # alone, but not --codesign, which pins MACOS_TEAM.
        record = json.loads(self.provenance().read_text())
        record["signing"]["team_identifier"] = "SYNTH12345"
        record["disk_image"]["signing"]["team_identifier"] = "SYNTH12345"
        path = self.dist / f"{STEM}.provenance.json"
        path.write_text(json.dumps(record))
        self.assertEqual(self.run_verifier(FakeTools(self.volume), "--provenance", str(path))[0], 0)
        tools = FakeTools(self.volume, team="SYNTH12345")
        code, err = self.run_verifier(tools, "--provenance", str(path), "--codesign")
        self.assertEqual(code, 1)
        self.assertIn(f"signed by team SYNTH12345, not {TEAM}", err)
        self.assertFalse(tools.attached)

    def test_native_signer_must_match_the_recorded_signer(self):
        provenance = self.provenance()
        for kwargs in ({"authorities": [DEV_ID]}, {"authorities": [*SIGNING["authorities"], "Apple Root CA"]},
                       {"team": "SYNTH12345"}):
            with self.subTest(kwargs=kwargs):
                tools = FakeTools(self.volume, **kwargs)
                code, err = self.run_verifier(tools, "--provenance", str(provenance), "--codesign")
                self.assertEqual(code, 1)
                self.assertIn("is not the one in the provenance", err)
                self.assertFalse(tools.attached)

    def test_the_mounted_volume_must_be_apfs(self):
        for kind in ("hfs", None):
            with self.subTest(kind=kind):
                tools = FakeTools(self.volume, volume_kind=kind)
                code, err = self.run_verifier(tools)
                self.assertEqual(code, 1)
                self.assertIn(f"the mounted volume is {kind!r}, not the apfs volume", err)
                self.assertEqual(tools.calls[-1], ["hdiutil", "detach", DEVICE])

    def test_the_image_disk_is_detached_whatever_its_number(self):
        # The synthesized APFS container may get the lower device number.
        cases = {
            "partition scheme first": (image_entities("/dev/disk20", "/dev/disk3"), "/dev/disk20"),
            "partition scheme later": ([image_entities("/dev/disk20", "/dev/disk3")[2],
                                        *image_entities("/dev/disk20", "/dev/disk3")[:2]], "/dev/disk20"),
            "no content hints": ([{"dev-entry": "/dev/disk20"}, {"dev-entry": "/dev/disk20s1"},
                                  {"dev-entry": "/dev/disk3"}], "/dev/disk20"),
        }
        for label, (entities, device) in cases.items():
            with self.subTest(label):
                tools = FakeTools(self.volume, entities=entities)
                code, err = self.run_verifier(tools)
                self.assertEqual(code, 0, err)
                self.assertEqual(tools.calls[-1], ["hdiutil", "detach", device])

    def test_busy_detach_is_retried_then_forced(self):
        tools = FakeTools(self.volume, detach_failures=2)
        self.assertEqual(self.run_verifier(tools)[0], 0)
        detaches = [call for call in tools.calls if call[:2] == ["hdiutil", "detach"]]
        self.assertEqual(detaches, [["hdiutil", "detach", DEVICE]] * 3)
        tools = FakeTools(self.volume, detach_failures=verify.DETACH_ATTEMPTS)
        self.assertEqual(self.run_verifier(tools)[0], 0)
        self.assertEqual(tools.calls[-1], ["hdiutil", "detach", "-force", DEVICE])
        tools = FakeTools(self.volume, detach_failures=99)
        code, err = self.run_verifier(tools)
        self.assertEqual(code, 1)
        self.assertIn(f"could not detach the disk image ({DEVICE})", err)

    def test_failed_attach_still_detaches_a_known_device(self):
        tools = FakeTools(self.volume, attach_code=1)
        code, err = self.run_verifier(tools)
        self.assertEqual(code, 1)
        self.assertIn("hdiutil attach failed", err)
        self.assertEqual(tools.calls[-1], ["hdiutil", "detach", DEVICE])

    def test_a_stuck_tool_is_an_error_not_a_pass(self):
        def stuck(command, *, timeout=None):
            raise subprocess.TimeoutExpired(command, timeout)
        with mock.patch.object(verify, "run_tool", stuck):
            code, err = quiet(verify.main, ["--dmg", str(self.dmg), "--compare-zip", str(self.zip)])
        self.assertEqual(code, 2)
        self.assertIn("timed out", err)

    def test_usage(self):
        for argv in (["--dmg", str(self.dmg)], [str(self.zip), "--dmg", str(self.dmg), "--compare-zip", str(self.zip)],
                     ["--dmg", str(self.dmg), "--compare-zip", str(self.zip), "--symbols"], []):
            with self.subTest(argv=argv), self.assertRaises(SystemExit):
                quiet(verify.main, argv)


class DiskImageProvenanceTests(DiskImageFixture):
    def test_provenance_records_the_disk_image_beside_unchanged_zip_fields(self):
        signing_json = self.root / "signing.json"
        signing_json.write_text(json.dumps(SIGNING | {"team_identifier": TEAM}))
        report = self.root / "disk-image.json"
        report.write_text(json.dumps(self.report()))
        base = ["provenance", "--artifact", str(self.zip), "--app", str(self.app), "--signing-json",
                str(signing_json), "--submission-id", "synthetic-id", "--notary-status", "Accepted",
                "--dist-dir", str(self.dist)]
        self.assertEqual(quiet(metadata.main, base)[0], 0)
        plain = json.loads((self.dist / f"{STEM}.provenance.json").read_text())
        code, err = quiet(metadata.main, [*base, "--disk-image", str(self.dmg), "--disk-image-json", str(report)])
        self.assertEqual(code, 0, err)
        record = json.loads((self.dist / f"{STEM}.provenance.json").read_text())
        image = record.pop("disk_image")
        self.assertEqual(record, plain)
        self.assertEqual(image, {
            "name": self.dmg.name, "sha256": hashlib.sha256(self.dmg.read_bytes()).hexdigest(),
            "size": self.dmg.stat().st_size, "format": "ULFO", "filesystem": "APFS",
            "signing": {"authorities": SIGNING["authorities"], "team_identifier": TEAM},
            "notarization": {"submission_id": "00000000-0000-0000-0000-000000000001",
                             "status": "Accepted", "stapled": True}})
        self.assertEqual(quiet(metadata.main, [*base, "--disk-image", str(self.dmg)])[0], 1)

    def test_only_a_developer_id_notarized_stapled_image_of_the_same_team_is_recorded(self):
        signing = SIGNING | {"team_identifier": TEAM}
        cases = {
            "not signed by a Developer ID": self.report(signing={"authorities": ["Apple Development: X"],
                                                                 "team_identifier": TEAM}),
            "different teams": self.report(signing={"authorities": [DEV_ID], "team_identifier": "SYNTH12345"}),
            "not notarized and stapled": self.report(notarization={"submission_id": "x", "status": "Invalid",
                                                                   "stapled": True}),
            "no notarization submission id": self.report(notarization={"status": "Accepted", "stapled": True}),
            "must be ULFO": self.report(format="UDZO"),
        }
        for message, report in cases.items():
            with self.subTest(message), self.assertRaisesRegex(metadata.MetadataError, message):
                metadata.disk_image_record(self.dmg, self.zip, report, signing)
        with self.assertRaisesRegex(metadata.MetadataError, "must be named"):
            metadata.disk_image_record(self.dmg.with_name("BroScribe.dmg"), self.zip, self.report(), signing)


def layout_names(build=42, mac="notarized", dmg=(True, True), envelope=False, extra=()) -> list[str]:
    stem = f"BroScribe-1.2.3-{build}"
    names = []
    for label, archive in (("linux-x86_64", "tar.gz"), ("linux-aarch64", "tar.gz"), ("windows-x64", "zip")):
        names += [f"{stem}-{label}.{archive}", f"{stem}-{label}.{archive}.sha256",
                  f"{stem}-{label}.provenance.json"]
    mac_stem = f"{stem}-macos-arm64" + ("-adhoc" if mac == "adhoc" else "")
    names += [f"{mac_stem}.zip", f"{mac_stem}.zip.sha256", f"{mac_stem}.provenance.json"]
    image = f"{stem}-macos-arm64.dmg"
    names += [name for name, present in ((image, dmg[0]), (image + ".sha256", dmg[1])) if present]
    if envelope:
        names.append(metadata.ENVELOPE_ASSET)
    return names + list(extra)


class ReleaseAssetSetTests(unittest.TestCase):
    def test_each_layout_has_its_exact_asset_count(self):
        for mac, dmg, envelope, count in (("notarized", (True, True), False, 14),
                                          ("notarized", (True, True), True, 15),
                                          ("adhoc", (False, False), False, 12),
                                          ("adhoc", (False, False), True, 13)):
            with self.subTest(mac=mac, envelope=envelope):
                names = layout_names(mac=mac, dmg=dmg, envelope=envelope)
                self.assertEqual(len(names), count)
                packages = metadata.release_layout("v1.2.3", names, envelope)
                self.assertEqual(sum(len(files) for files in packages.values()) + envelope, count)
                self.assertEqual(metadata.release_layout("v1.2.3", names), packages)
                mac_folder = "macos-arm64" + ("-adhoc" if mac == "adhoc" else "")
                self.assertEqual(len(packages[mac_folder]), 5 if mac == "notarized" else 3)

    def test_wrong_sets_are_refused(self):
        cases = {
            "needs both": layout_names(dmg=(True, False)),
            "only BroScribe-1.2.3-42-macos-arm64.dmg.sha256": layout_names(dmg=(False, True)),
            "needs both BroScribe-1.2.3-42-macos-arm64.dmg and": layout_names(dmg=(False, False)),
            "never has a disk image": layout_names(mac="adhoc", dmg=(True, True)),
            "unexpected release assets: notes.txt": layout_names(extra=("notes.txt",)),
            "unexpected release assets: BroScribe-1.2.3-42-macos-arm64-unsigned.dmg":
                layout_names(extra=("BroScribe-1.2.3-42-macos-arm64-unsigned.dmg",)),
            "duplicate": layout_names(extra=("BroScribe-1.2.3-42-linux-x86_64.tar.gz",)),
            "exactly one macos-arm64": layout_names(extra=("BroScribe-1.2.3-42-macos-arm64-adhoc.zip",)),
            "vMAJOR.MINOR.PATCH": layout_names(),
        }
        for message, names in cases.items():
            with self.subTest(message), self.assertRaisesRegex(metadata.MetadataError, message):
                metadata.release_layout("1.2.3" if "MAJOR" in message else "v1.2.3", names)
        mixed = [name.replace("-42-", "-43-") if "linux-x86_64" in name else name for name in layout_names()]
        with self.assertRaisesRegex(metadata.MetadataError, "same build"):
            metadata.release_layout("v1.2.3", mixed)
        with self.assertRaisesRegex(metadata.MetadataError, "has no broscribe-update.json"):
            metadata.release_layout("v1.2.3", layout_names(), envelope=True)
        with self.assertRaisesRegex(metadata.MetadataError, "unsigned release"):
            metadata.release_layout("v1.2.3", layout_names(envelope=True), envelope=False)

    def draft_files(self, mac="notarized", folder=None, **kwargs) -> list[Path]:
        folders = {"linux-x86_64": "linux-x86_64", "linux-aarch64": "linux-aarch64",
                   "windows-x64": "windows-x64-unsigned"}
        files = []
        for name in layout_names(mac=mac, **kwargs):
            if name == metadata.ENVELOPE_ASSET:
                files.append(Path("/runner-temp") / name)
                continue
            label = next((label for label in folders if f"-{label}." in name), None)
            parent = folders[label] if label else (folder or f"macos-arm64-{mac}")
            files.append(Path("artifacts") / parent / name)
        return files

    def test_draft_gate_checks_names_and_folders(self):
        for mac, envelope in (("notarized", False), ("notarized", True), ("adhoc", False), ("adhoc", True)):
            with self.subTest(mac=mac, envelope=envelope):
                dmg = (True, True) if mac == "notarized" else (False, False)
                files = self.draft_files(mac, dmg=dmg, envelope=envelope)
                code, err = quiet(metadata.main, ["assets", "--tag", "v1.2.3",
                                                  *(["--envelope"] if envelope else []), *map(str, files)])
                self.assertEqual(code, 0, err)
        for files, message in (
                (self.draft_files("notarized", folder="macos-arm64-adhoc"), "disagrees with its macOS assets"),
                (self.draft_files("adhoc", folder="macos-arm64-notarized", dmg=(False, False)), "disagrees"),
                ([*self.draft_files()[:-1], Path("artifacts/linux-x86_64") / self.draft_files()[-1].name],
                 "split across artifact folders"),
                (self.draft_files(dmg=(True, False)), "needs both"),
                (self.draft_files(envelope=True), "unsigned release")):
            with self.subTest(message), self.assertRaisesRegex(metadata.MetadataError, message):
                metadata.check_draft_assets("v1.2.3", files, False)

    def test_refresh_uses_the_same_rule(self):
        release = {"tag_name": "v1.2.3", "draft": False, "prerelease": False,
                   "published_at": "2026-11-27T10:00:00Z"}
        names = layout_names(envelope=True)
        _, packages = refresh.release_assets(release | {"assets": [{"name": n} for n in names]})
        self.assertEqual(packages, metadata.release_layout("v1.2.3", names))
        for broken in (layout_names(dmg=(True, False)), layout_names(mac="adhoc", dmg=(True, True)),
                       layout_names(extra=("stray.bin",))):
            with self.assertRaises(ValueError):
                refresh.release_assets(release | {"assets": [{"name": n} for n in broken]})

    def test_every_missing_sidecar_is_refused_for_every_platform(self):
        """Each package's `.sha256` and `.provenance.json` (`.zip.sha256`,
        `.tar.gz.sha256`), for the notarized and the ad-hoc macOS layouts, by
        release_layout, the draft gate and the refresh qualification."""
        release = {"tag_name": "v1.2.3", "draft": False, "prerelease": False,
                   "published_at": "2026-11-27T10:00:00Z"}
        for mac in ("notarized", "adhoc"):
            dmg = (True, True) if mac == "notarized" else (False, False)
            names = layout_names(mac=mac, dmg=dmg)
            sidecars = [name for name in names if name.endswith((".sha256", ".provenance.json"))
                        and ".dmg" not in name]
            self.assertEqual(len(sidecars), 8)
            for kind, count in ((".zip.sha256", 2), (".tar.gz.sha256", 2), (".provenance.json", 4)):
                self.assertEqual(sum(name.endswith(kind) for name in sidecars), count, kind)
            for missing in sidecars:
                with self.subTest(mac=mac, missing=missing):
                    rest = [name for name in names if name != missing]
                    with self.assertRaisesRegex(metadata.MetadataError, f"missing release assets: {missing}$"):
                        metadata.release_layout("v1.2.3", rest)
                    files = [path for path in self.draft_files(mac, dmg=dmg) if path.name != missing]
                    code, err = quiet(metadata.main, ["assets", "--tag", "v1.2.3", *map(str, files)])
                    self.assertEqual(code, 1)
                    self.assertIn(f"missing release assets: {missing}", err)
                    with self.assertRaisesRegex(ValueError, f"missing release assets: {missing}"):
                        refresh.release_assets(release | {"assets": [{"name": n} for n in rest]})


class RefreshDiskImageTests(DiskImageFixture):
    def package(self) -> Path:
        provenance = self.provenance()
        folder = self.root / "packages/macos-arm64"
        folder.mkdir(parents=True)
        for path in (self.zip, self.zip.with_name(self.zip.name + ".sha256"), self.dmg,
                     self.dmg.with_name(self.dmg.name + ".sha256"), provenance):
            if path.exists():
                shutil.copy2(path, folder / path.name)
        return folder

    def test_ad_hoc_preview_has_nothing_to_check_and_a_half_pair_is_refused(self):
        (self.root / "packages/macos-arm64-adhoc").mkdir(parents=True)
        with mock.patch.object(verify, "run_tool", side_effect=AssertionError("no tools")):
            refresh.verify_disk_image(self.root / "packages")
        folder = self.package()
        (folder / self.dmg.name).unlink()
        with self.assertRaisesRegex(ValueError, "exactly one ZIP and one disk image"):
            refresh.verify_disk_image(self.root / "packages")


class RefreshMainTests(unittest.TestCase):
    """refresh-update.py main() end to end, offline: a published release of
    four synthetic packages and its baseline envelope, a fake `broscribe`
    that accepts exactly that envelope, and faked disk image tools."""

    TAG, VERSION, BUILD = "v1.2.3", "1.2.3", 42

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        patcher = mock.patch.object(verify, "DETACH_PAUSE", 0)
        patcher.start()
        self.addCleanup(patcher.stop)
        # Linux and Windows packages from the desktop packaging fixture.
        self.stage, self.work = self.root / "build", self.root / "work"
        self.stage.mkdir()
        self.work.mkdir()
        published = self.root / "published"
        desktop = None
        for target in (*desktop_tests.LINUX, desktop_tests.WINDOWS):
            build, files = desktop_tests.DesktopArtifacts.fixture(self, target)
            folder = published / desktop_tests.TARGETS[target][1]
            folder.mkdir(parents=True)
            archive = desktop_tests.package.write_release(folder, self.work, build, files, [])
            desktop = json.loads(archive.with_name(
                archive.name.removesuffix(".zip").removesuffix(".tar.gz") + ".provenance.json").read_bytes())
        # The notarized macOS package: ZIP, DMG, both checksums, one provenance.
        self.mac = published / "macos-arm64"
        self.mac.mkdir()
        self.stem = f"BroScribe-{self.VERSION}-{self.BUILD}-macos-arm64"
        mac_root = self.root / "mac"
        mac_root.mkdir()
        self.app = make_bundle(mac_root, build=self.BUILD, version=self.VERSION)
        info = self.app / "Contents/Resources/build-info.json"
        info.write_text(json.dumps(json.loads(info.read_text())
                                   | {name: desktop[name] for name in ("git", "cargo_lock_sha256")}))
        distribution_modes(self.app)
        self.write_mac_package(b"synthetic disk image")
        self.volume = write_volume(mac_root, self.app)
        self.published = published
        # The baseline envelope of exactly these assets.
        manifest = metadata.combined_manifest(published, self.TAG, NOW, allow_legacy_layout=True)
        self.envelope = json.dumps({
            "format": metadata.ENVELOPE_FORMAT, "schema": 1, "manifest": json.dumps(manifest),
            "signatures": [{"key_id": "0" * 16, "ed25519": "ab" * 64}]}).encode()
        self.cli = self.root / "broscribe"
        self.cli.write_text(
            f"#!{sys.executable}\n"
            "import hashlib, sys\n"
            "assert sys.argv[1:4] == ['update', 'verify-manifest', '--envelope-stdin'], sys.argv\n"
            f"sys.exit(0 if hashlib.sha256(sys.stdin.buffer.read()).hexdigest() == "
            f"'{hashlib.sha256(self.envelope).hexdigest()}' else 1)\n")
        self.cli.chmod(0o755)

    def write_mac_package(self, dmg_bytes: bytes) -> None:
        zip_path = zip_app(self.app, self.mac / f"{self.stem}.zip")
        dmg = self.mac / f"{self.stem}.dmg"
        dmg.write_bytes(dmg_bytes)
        (self.mac / f"{dmg.name}.sha256").write_text(f"{hashlib.sha256(dmg_bytes).hexdigest()}  {dmg.name}\n")
        signing = SIGNING | {"team_identifier": TEAM}
        report = {"format": "ULFO", "filesystem": "APFS",
                  "signing": {"authorities": SIGNING["authorities"], "team_identifier": TEAM},
                  "notarization": {"submission_id": "00000000-0000-0000-0000-000000000001",
                                   "status": "Accepted", "stapled": True}}
        record = metadata.provenance(zip_path, self.app, signing, "synthetic-id", "Accepted", self.mac)
        record["disk_image"] = metadata.disk_image_record(dmg, zip_path, report, signing)
        (self.mac / f"{self.stem}.provenance.json").write_text(json.dumps(record))
        (self.mac / f"{zip_path.name}.sha256").write_text(f"{record['artifact']['sha256']}  {zip_path.name}\n")

    def run_refresh(self, tools: FakeTools) -> tuple[int, str, Path]:
        run = Path(tempfile.mkdtemp(dir=self.root, prefix="run-"))
        downloads = run / "downloads"
        downloads.mkdir()
        for path in self.published.glob("*/*"):
            shutil.copy2(path, downloads / path.name)
        (downloads / refresh.ENVELOPE).write_bytes(self.envelope)
        release = run / "release.json"
        release.write_text(json.dumps({
            "tag_name": self.TAG, "draft": False, "prerelease": False, "published_at": "2026-11-17T10:00:00Z",
            "assets": [{"name": path.name} for path in sorted(downloads.iterdir())]}))
        out = run / "manifest.json"
        argv = ["refresh-update.py", str(release), "--cli", str(self.cli), "--downloads", str(downloads),
                "--packages", str(run / "packages"), "--out", str(out)]
        with mock.patch.object(sys, "argv", argv), mock.patch.object(verify, "run_tool", tools):
            code, err = quiet(refresh.main)
        return code, err, out

    def substitute_disk_image(self, data: bytes) -> None:
        """Another disk image with its sidecar and provenance rewritten to match."""
        dmg = self.mac / f"{self.stem}.dmg"
        dmg.write_bytes(data)
        digest = hashlib.sha256(data).hexdigest()
        (self.mac / f"{dmg.name}.sha256").write_text(f"{digest}  {dmg.name}\n")
        provenance = self.mac / f"{self.stem}.provenance.json"
        record = json.loads(provenance.read_text())
        record["disk_image"].update(sha256=digest, size=len(data))
        provenance.write_text(json.dumps(record))

    def test_the_genuine_release_refreshes(self):
        tools = FakeTools(self.volume)
        code, err, out = self.run_refresh(tools)
        self.assertEqual(code, 0, err)
        manifest = json.loads(out.read_text())
        self.assertEqual([entry["url"].rsplit("/", 1)[1] for entry in manifest["artifacts"]
                          if entry["target"] == "aarch64-apple-darwin"], [f"{self.stem}.zip"])
        self.assertTrue(tools.attached)
        self.assertTrue(any(call[0] == "spctl" for call in tools.calls))

    def test_a_substituted_disk_image_is_refused_before_any_manifest(self):
        # The ZIP still matches the authenticated envelope; only the DMG (and
        # the sidecar and provenance fields that describe it) changed.
        self.substitute_disk_image(b"substituted disk image")
        cases = {
            "another app": ({}, lambda: (self.volume / "BroScribe.app/Contents/MacOS/broscribe-worker")
                            .write_bytes(b"substituted worker"), "differs from the ZIP"),
            "another signer": ({"team": "SYNTH12345"}, None, "is not the one in the provenance"),
            "no ticket": ({"dmg_stapled": False}, None, "stapler validate failed on the disk image"),
        }
        for label, (kwargs, change, message) in cases.items():
            with self.subTest(label):
                shutil.rmtree(self.volume)
                self.volume = write_volume(self.root / "mac", self.app)
                if change:
                    change()
                tools = FakeTools(self.volume, **kwargs)
                code, err, out = self.run_refresh(tools)
                self.assertEqual(code, 1)
                self.assertIn(message, err)
                self.assertIn("failed verification against", err)
                self.assertFalse(out.exists(), "no manifest is emitted")

    def test_changing_the_zip_and_the_disk_image_together_fails_envelope_authentication(self):
        # A consistent replacement of both: a different app in a new ZIP and
        # DMG, with every checksum and the provenance recomputed.
        (self.app / "Contents/MacOS/broscribe-worker").write_bytes(b"replacement worker")
        distribution_modes(self.app)
        self.write_mac_package(b"replacement disk image")
        shutil.rmtree(self.volume)
        self.volume = write_volume(self.root / "mac", self.app)
        tools = FakeTools(self.volume)
        code, err, out = self.run_refresh(tools)
        self.assertEqual(code, 1)
        self.assertIn("the existing update envelope describes different release artifacts", err)
        self.assertFalse(out.exists())
        self.assertEqual(tools.calls, [], "the disk image is never examined for an unauthenticated ZIP")


STUB_LOG = 'log() { printf "%s\\n" "$*" >> "$FAKE_LOG"; }\n'


def write_stub(path: Path, body: str, shell="#!/bin/sh") -> None:
    """An executable stub; shell stubs get a `log` function writing to FAKE_LOG."""
    path.write_text(f"{shell}\n{STUB_LOG if shell in ('#!/bin/sh', '#!/bin/bash') else ''}{body}\n")
    path.chmod(0o755)


@unittest.skipUnless(os.name == "posix" and shutil.which("bash"), "requires bash")
class NotaryTimeLimitTests(unittest.TestCase):
    def run_common(self, script: str, **env) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", "-c", f'set -euo pipefail; source "$1"; {script}', "bash",
                               str(SCRIPTS / "common.sh")], env={**os.environ, **env},
                              capture_output=True, text=True, timeout=60)

    def test_the_outer_limit_is_the_notary_timeout_plus_the_upload_grace(self):
        grace = 600
        for value, seconds in (("30m", 1800), ("45m", 2700), ("90s", 90), ("2h", 7200), ("9999s", 9999)):
            with self.subTest(value=value):
                result = self.run_common(f"notary_wall_seconds {value}")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(int(result.stdout), seconds + grace)
        for value in ("0m", "45", "45min", "10000s", "", "-5m"):
            with self.subTest(value=value):
                self.assertNotEqual(self.run_common(f'notary_wall_seconds "{value}"').returncode, 0)

    def test_the_notary_timeout_defaults_to_45m_and_is_validated(self):
        environment = {key: value for key, value in os.environ.items() if key != "BROSCRIBE_NOTARY_TIMEOUT"}
        result = subprocess.run(["bash", "-c", 'source "$1"; checked_notary_timeout', "bash",
                                 str(SCRIPTS / "common.sh")], env=environment, capture_output=True, text=True)
        self.assertEqual(result.stdout, "45m\n")
        self.assertEqual(self.run_common("checked_notary_timeout", BROSCRIBE_NOTARY_TIMEOUT="30m").stdout, "30m\n")
        result = self.run_common("checked_notary_timeout", BROSCRIBE_NOTARY_TIMEOUT="forever")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be a duration such as 45m", result.stderr)

    def test_bounded_stops_a_stuck_tool(self):
        result = self.run_common("bounded 1 /bin/sleep 30")
        self.assertEqual(result.returncode, 124)
        self.assertIn("did not finish within 1 s", result.stderr)


def stub_repo(root: Path, *scripts: str, grace: int | None = None) -> Path:
    """A copy of scripts/<name> beside the real common.sh (optionally with a
    one-second notary upload grace), so the scripts find their siblings."""
    folder = root / "repo/scripts"
    folder.mkdir(parents=True)
    for name in scripts:
        shutil.copy2(SCRIPTS / name, folder)
    common = (SCRIPTS / "common.sh").read_text()
    if grace is not None:
        common = common.replace("NOTARY_UPLOAD_GRACE_SECONDS=600", f"NOTARY_UPLOAD_GRACE_SECONDS={grace}")
    (folder / "common.sh").write_text(common)
    return folder


XCRUN_STUB = ('log xcrun "$@"; case "$1 $2" in '
              '"stapler staple") case "$3" in *.dmg) printf " ticket" >> "$3" ;; esac ;; '
              '"notarytool submit") if [ -n "$FAKE_NOTARY_SLEEP" ]; then exec /bin/sleep "$FAKE_NOTARY_SLEEP"; fi; '
              'printf "{\\"id\\": \\"%s\\", \\"status\\": \\"%s\\"}" '
              '00000000-0000-0000-0000-00000000000a "$FAKE_NOTARY_STATUS" ;; '
              '"notarytool log") for a; do :; done; '
              'printf "{\\"status\\": \\"Invalid\\", \\"developerEmail\\": \\"x@example.invalid\\"}" > "$a" ;; '
              'esac')


@unittest.skipUnless(os.name == "posix" and shutil.which("bash") and shutil.which("shasum"),
                     "requires bash and shasum")
class PackagingScriptTests(unittest.TestCase):
    """scripts/package-macos-dmg.sh with stub tools: what it calls in which
    order, and what it leaves in dist."""

    TOOLS = {
        "uname": "echo Darwin",
        "plutil": 'case "$2" in CFBundleShortVersionString) echo 0.2.0 ;; CFBundleVersion) echo 12 ;; esac',
        "ditto": 'log ditto "$@"; for a; do :; done; '
                 'if [ "$1" = -c ]; then printf zip > "$a"; else mkdir -p "$a"; cp -R "$5"/. "$a"/; fi',
        # FAKE_CREATE_FAILURES "Resource busy" failures (or FAKE_CREATE_ERROR
        # once), then a synthetic image.
        "hdiutil": 'log hdiutil "$@"; for a; do :; done; '
                   'if [ "$1" = create ]; then '
                   'n=$(cat "$FAKE_LOG.creates" 2>/dev/null || echo 0); echo $((n + 1)) > "$FAKE_LOG.creates"; '
                   'if [ -n "$FAKE_CREATE_ERROR" ]; then echo "hdiutil: create failed - $FAKE_CREATE_ERROR" >&2; '
                   'exit 1; fi; '
                   'if [ "$n" -lt "${FAKE_CREATE_FAILURES:-0}" ]; then '
                   'echo "hdiutil: create failed - Resource busy" >&2; exit 1; fi; fi; '
                   'printf "synthetic image" > "$a"',
        "codesign": 'log codesign "$@"; case "$*" in *--display*) '
                    'printf "Authority=Developer ID Application: Synthetic (3CHB92P2NM)\\n'
                    'Authority=Developer ID Certification Authority\\nTeamIdentifier=3CHB92P2NM\\n'
                    'Timestamp=27 Nov 2026\\n" >&2 ;; esac',
        "xcrun": XCRUN_STUB,
        "sleep": 'log sleep "$@"',
    }

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.make_repo()
        tools = self.root / "tools"
        tools.mkdir()
        for name, body in self.TOOLS.items():
            write_stub(tools / name, body)
        self.target = self.root / "target"
        self.app = self.target / "BroScribe.app"
        (self.app / "Contents/MacOS").mkdir(parents=True)
        (self.app / "Contents/MacOS/broscribe-app").write_bytes(b"synthetic")
        self.dist = self.target / "dist"
        self.dist.mkdir()
        (self.dist / f"{STEM}.zip").write_bytes(b"synthetic zip")
        self.log = self.root / "calls.log"
        self.report = self.root / "reports/new/report.json"
        self.env = {**os.environ, "PATH": f"{tools}{os.pathsep}{os.environ['PATH']}", "FAKE_LOG": str(self.log),
                    "CARGO_TARGET_DIR": str(self.target), "FAKE_NOTARY_STATUS": "Accepted",
                    "BROSCRIBE_SIGN_IDENTITY": "Developer ID Application: Synthetic (3CHB92P2NM)",
                    "BROSCRIBE_DMG_REPORT": str(self.report), "HOME": str(self.root / "home")}
        self.env.pop("BROSCRIBE_NOTARY_TIMEOUT", None)

    def make_repo(self, grace: int | None = None) -> None:
        # A stub verifier and the real release-metadata.py beside the script.
        if (self.root / "repo").exists():
            shutil.rmtree(self.root / "repo")
        scripts = stub_repo(self.root, "package-macos-dmg.sh", "release-metadata.py", grace=grace)
        (scripts / "verify-macos-artifact.py").write_text(
            "import os, sys\n"
            "open(os.environ['FAKE_LOG'], 'a').write('verify ' + ' '.join(sys.argv[1:]) + '\\n')\n"
            "dmg = sys.argv[sys.argv.index('--dmg') + 1]\n"
            "open(os.environ['FAKE_LOG'], 'a').write('verify-saw ' + open(dmg).read() + '\\n')\n"
            "sys.exit(int(os.environ.get('FAKE_VERIFY_STATUS', '0')))\n")
        self.script = scripts / "package-macos-dmg.sh"

    def run_script(self, *args, **env):
        return subprocess.run(["bash", str(self.script), *args], env={**self.env, **env},
                              capture_output=True, text=True, timeout=120)

    def calls(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_accepted_image_is_signed_notarized_stapled_then_checksummed_and_verified(self):
        stale = self.dist / f"{STEM}.provenance.json"
        stale.write_text("{}")
        result = self.run_script("BroScribeRelease")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        index = lambda prefix, end="": next(i for i, call in enumerate(calls)
                                             if call.startswith(prefix) and call.endswith(end))
        self.assertLess(index("xcrun stapler validate", "/BroScribe.app"), index("hdiutil create"))
        stage = calls[index("ditto --norsrc")]
        self.assertTrue(stage.startswith("ditto --norsrc --noextattr --noqtn --noacl "), stage)
        create = calls[index("hdiutil create")]
        for part in ("-volname BroScribe", "-fs APFS", "-format ULFO", "-srcfolder"):
            self.assertIn(part, create)
        self.assertNotIn(str(self.dist), create, "the image is built outside dist")
        sign = calls[index("codesign --force")]
        # No hardened runtime, entitlements or --deep: a disk image has no code.
        self.assertRegex(sign, r"^codesign --force --sign Developer ID Application: Synthetic \(3CHB92P2NM\) "
                               r"--timestamp \S+\.dmg$")
        submit = calls[index("xcrun notarytool submit")]
        self.assertIn("--keychain-profile BroScribeRelease", submit)
        self.assertIn("--wait --timeout 45m", submit)
        order = [index(p, e) for p, e in (("hdiutil create", ""), ("codesign --force", ""),
                                          ("xcrun notarytool submit", ""), ("xcrun stapler staple", ".dmg"),
                                          ("xcrun stapler validate", ".dmg"), ("verify ", ""))]
        self.assertEqual(order, sorted(order))
        verify_call = calls[index("verify ")]
        self.assertIn(f"--compare-zip {self.dist}/{STEM}.zip", verify_call)
        self.assertIn("--require-complete-licences --codesign --require-stapled", verify_call)
        self.assertIn("verify-saw synthetic image ticket", calls)
        dmg = self.dist / f"{STEM}.dmg"
        self.assertEqual(dmg.read_bytes(), b"synthetic image ticket")
        self.assertEqual((self.dist / f"{STEM}.dmg.sha256").read_text(),
                         f"{hashlib.sha256(dmg.read_bytes()).hexdigest()}  {dmg.name}\n")
        # The report's folder is created; a stale provenance is gone.
        report = json.loads(self.report.read_text())
        self.assertEqual(report["notarization"], {"submission_id": "00000000-0000-0000-0000-00000000000a",
                                                  "status": "Accepted", "stapled": True})
        self.assertEqual(report["signing"]["team_identifier"], "3CHB92P2NM")
        self.assertEqual((report["format"], report["filesystem"]), ("ULFO", "APFS"))
        self.assertEqual(sorted(p.name for p in self.dist.iterdir()),
                         [f"{STEM}.dmg", f"{STEM}.dmg.sha256", f"{STEM}.zip"])

    def test_resource_busy_is_retried_with_a_growing_pause(self):
        result = self.run_script("BroScribeRelease", FAKE_CREATE_FAILURES="3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call for call in self.calls() if call.startswith("sleep")],
                         ["sleep 5", "sleep 10", "sleep 15"])
        self.assertEqual(sum(call.startswith("hdiutil create") for call in self.calls()), 4)
        self.assertIn("resource busy; retrying in 15 s", result.stderr)
        self.assertTrue((self.dist / f"{STEM}.dmg").is_file())

    def test_resource_busy_gives_up_after_five_attempts_and_other_errors_at_once(self):
        result = self.run_script("BroScribeRelease", FAKE_CREATE_FAILURES="99")
        self.assertEqual(result.returncode, 1)
        self.assertIn("could not create the disk image (attempt 5 of 5)", result.stderr)
        self.assertEqual([call for call in self.calls() if call.startswith("sleep")],
                         ["sleep 5", "sleep 10", "sleep 15", "sleep 20"])
        self.log.unlink()
        Path(f"{self.log}.creates").unlink()
        result = self.run_script("BroScribeRelease", FAKE_CREATE_ERROR="No space left on device")
        self.assertEqual(result.returncode, 1)
        self.assertIn("(attempt 1 of 5)", result.stderr)
        self.assertFalse(any(call.startswith("sleep") for call in self.calls()))
        self.assertEqual([p.name for p in self.dist.iterdir()], [f"{STEM}.zip"])

    def test_the_submission_is_stopped_after_the_notary_timeout_plus_grace(self):
        self.make_repo(grace=1)
        result = self.run_script("BroScribeRelease", BROSCRIBE_NOTARY_TIMEOUT="1s", FAKE_NOTARY_SLEEP="30")
        self.assertEqual(result.returncode, 1)
        self.assertIn("did not finish within 2 s", result.stderr)
        self.assertIn("disk image notarization failed (notarytool exit 124)", result.stderr)
        self.assertEqual([p.name for p in self.dist.iterdir()], [f"{STEM}.zip"])

    def test_rejection_keeps_a_redacted_log_and_nothing_else(self):
        result = self.run_script("BroScribeRelease", FAKE_NOTARY_STATUS="Invalid")
        self.assertEqual(result.returncode, 1)
        self.assertIn("disk image notarization Invalid", result.stderr)
        log = self.dist / "notarization-dmg-00000000-0000-0000-0000-00000000000a.log"
        self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)
        self.assertNotIn("example.invalid", log.read_text())
        self.assertEqual(sorted(p.name for p in self.dist.iterdir()), sorted([log.name, f"{STEM}.zip"]))
        self.assertFalse(any(call.startswith(("xcrun stapler staple", "verify")) for call in self.calls()))

    def test_failed_verification_moves_nothing_into_dist(self):
        result = self.run_script("BroScribeRelease", FAKE_VERIFY_STATUS="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([p.name for p in self.dist.iterdir()], [f"{STEM}.zip"])
        self.assertFalse(self.report.exists())

    def test_distribution_needs_a_developer_id_and_the_zip(self):
        for identity in ("-", "Apple Development: Synthetic", ""):
            with self.subTest(identity=identity):
                result = self.run_script("BroScribeRelease", BROSCRIBE_SIGN_IDENTITY=identity)
                self.assertEqual(result.returncode, 1)
                self.assertIn("Developer ID Application identity", result.stderr)
        self.assertFalse(any(call.startswith(("hdiutil", "xcrun notarytool")) for call in self.calls()))
        (self.dist / f"{STEM}.zip").unlink()
        result = self.run_script("BroScribeRelease")
        self.assertEqual(result.returncode, 1)
        self.assertIn("notarize-macos.sh writes it first", result.stderr)
        result = self.run_script("BroScribeRelease", BROSCRIBE_NOTARY_TIMEOUT="forever")
        self.assertEqual(result.returncode, 1)
        self.assertIn("must be a duration such as 45m", result.stderr)

    def test_unsigned_mode_never_signs_notarizes_or_writes_to_dist(self):
        result = self.run_script("--unsigned", BROSCRIBE_SIGN_IDENTITY="")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(call.startswith(("codesign", "xcrun")) for call in self.calls()))
        local = self.target / "dmg-unsigned"
        name = f"{STEM}-unsigned"
        self.assertEqual(sorted(p.name for p in local.iterdir()),
                         [f"{name}.dmg", f"{name}.dmg.sha256", f"{name}.zip"])
        self.assertEqual([p.name for p in self.dist.iterdir()], [f"{STEM}.zip"])
        verify_call = next(call for call in self.calls() if call.startswith("verify "))
        self.assertNotIn("--codesign", verify_call)
        self.assertIn(f"{name}.zip", verify_call)


@unittest.skipUnless(os.name == "posix" and shutil.which("bash") and shutil.which("shasum"),
                     "requires bash and shasum")
class NotarizeScriptTests(unittest.TestCase):
    """scripts/notarize-macos.sh with stub tools and stub sibling scripts: the
    order of its steps, and that a failure anywhere after the ZIP entered dist
    leaves no release-named file there."""

    TOOLS = {
        "uname": "echo Darwin",
        "plutil": PackagingScriptTests.TOOLS["plutil"],
        "ditto": PackagingScriptTests.TOOLS["ditto"],
        "codesign": 'log codesign "$@"; case "$*" in '
                    '*--requirements*) [ "$FAKE_FAIL" = requirements ] && { echo "codesign: synthetic failure" >&2; '
                    'exit 1; }; echo "designated => identifier org.broscribe.BroScribe" ;; '
                    '*--display*) printf "Authority=Developer ID Application: Synthetic (3CHB92P2NM)\\n'
                    'Authority=Developer ID Certification Authority\\nTeamIdentifier=3CHB92P2NM\\n" >&2 ;; esac',
        "xcrun": XCRUN_STUB,
        "spctl": 'log spctl "$@"',
    }
    RELEASE_NAMES = {f"{STEM}.zip", f"{STEM}.zip.sha256", f"{STEM}.dmg", f"{STEM}.dmg.sha256",
                     f"{STEM}.provenance.json", f"{STEM}.manifest.json", f"{STEM}.update-envelope.json"}

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.make_repo()
        tools = self.root / "tools"
        tools.mkdir()
        for name, body in self.TOOLS.items():
            write_stub(tools / name, body)
        self.target = self.root / "target"
        self.app = self.target / "BroScribe.app"
        (self.app / "Contents/MacOS").mkdir(parents=True)
        self.dist = self.target / "dist"
        self.log = self.root / "calls.log"
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("BROSCRIBE_")}
        self.env.update(PATH=f"{tools}{os.pathsep}{os.environ['PATH']}", FAKE_LOG=str(self.log),
                        CARGO_TARGET_DIR=str(self.target), FAKE_NOTARY_STATUS="Accepted",
                        BROSCRIBE_SIGN_IDENTITY="Developer ID Application: Synthetic (3CHB92P2NM)",
                        HOME=str(self.root / "home"))

    def make_repo(self, grace: int | None = None) -> None:
        if (self.root / "repo").exists():
            shutil.rmtree(self.root / "repo")
        scripts = stub_repo(self.root, "notarize-macos.sh", grace=grace)
        python = f"#!{sys.executable}"
        log = ("import os, sys\nfrom pathlib import Path\n"
               "def log(*parts):\n    open(os.environ['FAKE_LOG'], 'a').write(' '.join(parts) + '\\n')\n"
               "def arg(flag):\n    return Path(sys.argv[sys.argv.index(flag) + 1])\n")
        write_stub(scripts / "verify-macos-artifact.py", log +
                   "log('verify', *sys.argv[1:])\n"
                   "fail = os.environ.get('FAKE_FAIL', '')\n"
                   "sys.exit(1 if (fail == 'verify-dmg' and '--dmg' in sys.argv)"
                   " or (fail == 'verify-zip' and sys.argv[1].endswith('.zip')) else 0)\n", python)
        write_stub(scripts / "release-metadata.py", log +
                   "log('release-metadata', *sys.argv[1:])\n"
                   "if sys.argv[1] == 'notary-log':\n"
                   "    out = arg('--out'); out.write_text('sanitized\\n'); out.chmod(0o600); sys.exit(0)\n"
                   "if os.environ.get('FAKE_FAIL') == 'provenance':\n"
                   "    arg('--dist-dir').joinpath(arg('--artifact').stem + '.provenance.json').write_text('{partial')\n"
                   "    sys.exit(1)\n"
                   "artifact, dist = arg('--artifact'), arg('--dist-dir')\n"
                   "(dist / (artifact.stem + '.provenance.json')).write_text('{}')\n"
                   "(dist / (artifact.name + '.sha256')).write_text('0' * 64 + '  ' + artifact.name + '\\n')\n",
                   python)
        write_stub(scripts / "package-macos-dmg.sh",
                   'log package-macos-dmg.sh "$@" "timeout=$BROSCRIBE_NOTARY_TIMEOUT"\n'
                   '[ "$FAKE_FAIL" = dmg ] && { echo "package-macos-dmg.sh: synthetic failure" >&2; exit 1; }\n'
                   'dist="$CARGO_TARGET_DIR/dist"\n'
                   'printf image > "$dist/BroScribe-0.2.0-12-macos-arm64.dmg"\n'
                   'printf sum > "$dist/BroScribe-0.2.0-12-macos-arm64.dmg.sha256"\n'
                   'printf "{}" > "$BROSCRIBE_DMG_REPORT"\n', "#!/bin/bash")
        self.script = scripts / "notarize-macos.sh"

    def run_script(self, **env):
        return subprocess.run(["bash", str(self.script), "BroScribeRelease"], env={**self.env, **env},
                              capture_output=True, text=True, timeout=120)

    def calls(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []

    def release_files(self) -> set[str]:
        return {path.name for path in self.dist.iterdir()} & self.RELEASE_NAMES if self.dist.exists() else set()

    def test_the_disk_image_is_made_after_stapling_and_before_the_single_provenance(self):
        result = self.run_script(BROSCRIBE_NOTARY_TIMEOUT="30m")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.release_files(), self.RELEASE_NAMES - {f"{STEM}.manifest.json",
                                                                     f"{STEM}.update-envelope.json"})
        calls = self.calls()
        index = lambda prefix, needle="": next(i for i, call in enumerate(calls)
                                                if call.startswith(prefix) and needle in call)
        submit = calls[index("xcrun notarytool submit")]
        self.assertIn("submission.zip --keychain-profile BroScribeRelease --wait --timeout 30m", submit)
        steps = [index("xcrun notarytool submit"), index("xcrun stapler staple"), index("ditto -c", f"{STEM}.zip"),
                 index("package-macos-dmg.sh"), index("codesign --display --requirements"),
                 index("release-metadata provenance"), index("verify", f"{STEM}.zip --provenance"),
                 index("verify --dmg")]
        self.assertEqual(steps, sorted(steps))
        self.assertIn("timeout=30m", calls[index("package-macos-dmg.sh")])
        self.assertEqual(sum(call.startswith("release-metadata provenance") for call in calls), 1)
        provenance = calls[index("release-metadata provenance")]
        self.assertIn(f"--disk-image {self.dist}/{STEM}.dmg --disk-image-json", provenance)
        self.assertIn(f"--compare-zip {self.dist}/{STEM}.zip", calls[index("verify --dmg")])

    def test_any_failure_after_the_zip_entered_dist_leaves_no_release_file(self):
        for fail, message in (("dmg", "the disk image was not produced; the ZIP is removed with it"),
                              ("requirements", "removed this build's release files"),
                              ("provenance", "failed their provenance or verification checks"),
                              ("verify-zip", "failed their provenance or verification checks"),
                              ("verify-dmg", "failed their provenance or verification checks")):
            with self.subTest(fail=fail):
                if self.log.exists():
                    self.log.unlink()
                result = self.run_script(FAKE_FAIL=fail)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertIn("removed this build's release files", result.stderr)
                self.assertEqual(self.release_files(), set(), fail)
                self.assertTrue(any(call.startswith("ditto -c") and f"{STEM}.zip" in call for call in self.calls()),
                                "the ZIP had entered dist")

    def test_a_rejected_app_leaves_only_its_sanitized_log(self):
        result = self.run_script(FAKE_NOTARY_STATUS="Invalid")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.release_files(), set())
        self.assertEqual([path.name for path in self.dist.iterdir()],
                         ["notarization-00000000-0000-0000-0000-00000000000a.log"])
        self.assertFalse(any(call.startswith("package-macos-dmg.sh") for call in self.calls()))

    def test_the_app_submission_is_stopped_after_the_notary_timeout_plus_grace(self):
        self.make_repo(grace=1)
        result = self.run_script(BROSCRIBE_NOTARY_TIMEOUT="1s", FAKE_NOTARY_SLEEP="30")
        self.assertEqual(result.returncode, 1)
        self.assertIn("did not finish within 2 s", result.stderr)
        self.assertIn("notarization failed (notarytool exit 124)", result.stderr)
        self.assertEqual(self.release_files(), set())

    def test_release_workflow_verifies_the_disk_image_only_for_notarized_builds(self):
        workflow = (REPO / ".github/workflows/release.yml").read_text()
        signing = workflow.split("      - name: Sign, optionally notarize, and verify macOS", 1)[1]
        signing = signing.split("      - name:", 1)[0]
        adhoc, notarized = signing.split("          else\n", 1)
        self.assertIn("An ad-hoc build must not produce a disk image.", adhoc)
        self.assertNotIn("--dmg", adhoc)
        self.assertIn("BROSCRIBE_NOTARY_TIMEOUT: 30m", signing.split("run: |", 1)[0])
        self.assertIn('BROSCRIBE_SIGN_IDENTITY="$MACOS_SIGN_IDENTITY" \\\n'
                      '              BROSCRIBE_NOTARY_KEYCHAIN="$keychain" scripts/notarize-macos.sh', notarized)
        self.assertIn('--dmg "$CARGO_TARGET_DIR/dist/$stem.dmg"', notarized)
        self.assertLess(notarized.index("scripts/notarize-macos.sh"), notarized.index("--dmg"))
        dmg_check = notarized.split("--dmg", 1)[1].split("echo", 1)[0]
        for flag in ("--compare-zip", "--provenance", "--codesign", "--require-stapled"):
            self.assertIn(flag, dmg_check)
        notes = workflow.split("release_notes=", 1)[1].split("gh release create", 1)[0]
        self.assertLess(notes.index("install from"), notes.index("in-app update payload"))
        self.assertIn("ad-hoc macOS ZIP is a preview", notes)

    def test_release_workflow_checks_an_unsigned_disk_image_on_every_macos_run(self):
        workflow = (REPO / ".github/workflows/release.yml").read_text()
        steps = workflow.split("      - name: ")
        names = [step.split("\n", 1)[0] for step in steps]
        bundle = names.index("Build unsigned macOS bundle")
        check = names.index("Check the disk image path with an unsigned test image")
        self.assertEqual(check, bundle + 1)
        self.assertLess(check, names.index("Sign, optionally notarize, and verify macOS"))
        body = steps[check]
        self.assertIn("if: runner.os == 'macOS'\n", body)
        self.assertNotIn("secrets.", body)
        self.assertIn('scripts/package-macos-dmg.sh --unsigned "$CARGO_TARGET_DIR/BroScribe.app"', body)
        self.assertIn('verify-macos-artifact.py --dmg "${images[0]}" --compare-zip "${images[0]%.dmg}.zip"', body)
        self.assertIn("must stay out of dist", body)
        # Room for build, tests and two bounded notarizations on macOS only.
        self.assertIn("timeout-minutes: ${{ matrix.timeout_minutes }}", workflow)
        matrix = workflow.split("include:", 1)[1].split("runs-on:", 1)[0]
        limits = dict(zip([line.split(": ")[1] for line in matrix.splitlines() if "- platform:" in line],
                          [int(line.split(": ")[1]) for line in matrix.splitlines() if "timeout_minutes:" in line]))
        self.assertEqual(limits, {"macos-arm64": 210, "linux-x86_64": 150, "linux-aarch64": 150,
                                  "windows-x64": 150})


if __name__ == "__main__":
    unittest.main()

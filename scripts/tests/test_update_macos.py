"""Tests for scripts/update-macos.py with synthetic app bundles and a fake system.

No real codesign, launchctl, agent or installed app is used: every command the
updater runs goes to FakeSystem, and the bundles live in a temporary folder.
FakeSystem models the agent's install admission as the agent keeps it: granted
only while nothing records, durable across agent restarts, refusing every
capture start until released by fence.
"""

from __future__ import annotations

import contextlib
import copy
import io
import json
import os
import plistlib
import shutil
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

from support import load_script

if os.name == "posix":
    import fcntl

update = load_script("update-macos.py")

REQUIREMENT = 'identifier "{id}" and anchor apple generic and certificate leaf[subject.OU] = "SYNTH12345"'
TARGET = "aarch64-apple-darwin"


def make_app(path: Path, version: str, build: int, info_build: int | None = None) -> Path:
    for relative, identifier in ((Path("."), update.APP_ID), (update.HELPER, update.HELPER_ID)):
        contents = path / relative / "Contents"
        contents.mkdir(parents=True, exist_ok=True)
        with (contents / "Info.plist").open("wb") as handle:
            plistlib.dump({"CFBundleIdentifier": identifier, "CFBundleShortVersionString": version,
                           "CFBundleVersion": str(build)}, handle)
    (path / update.BUILD_INFO).parent.mkdir(parents=True, exist_ok=True)
    (path / update.BUILD_INFO).write_text(json.dumps({
        "schema": 1, "product": "BroScribe", "version": version,
        "build": build if info_build is None else info_build, "target": TARGET}))
    (path / update.CLI).parent.mkdir(parents=True, exist_ok=True)
    (path / update.CLI).write_text("synthetic cli\n")
    return path


def app_of(path: Path) -> Path | None:
    for candidate in [path, *path.parents]:
        if candidate.name.endswith(".app") and candidate.name != "BroScribe Agent.app":
            return candidate
        if candidate.name.startswith(".BroScribe.app."):
            return candidate
    return None


def bundle_build(app: Path) -> int:
    with (app / "Contents/Info.plist").open("rb") as handle:
        return int(plistlib.load(handle)["CFBundleVersion"])


def identity(app: Path) -> dict:
    info = json.loads((app / update.BUILD_INFO).read_text())
    return {"version": info["version"], "build": info["build"], "target": info["target"]}


def report(*failing: str) -> tuple[int, str]:
    """A doctor report as `broscribe --json doctor` writes it, exit 0."""
    checks = [{"check": name, "ok": name not in failing, "state": "problem" if name in failing else "ok",
               "required": True, "detail": "synthetic"} for name in ("config", "library", "me")]
    checks.append({"check": "ffmpeg", "ok": False, "state": "problem", "required": False, "detail": "synthetic"})
    return 0, json.dumps({"ok": not failing, "checks": checks, "daemon": {"running": True}})


def error(code: str) -> tuple[int, str, str]:
    return 1, json.dumps({"error": {"code": code, "message": f"synthetic {code}"}}), ""


class FakeSystem(update.System):
    def __init__(self, installed: Path):
        self.installed = installed
        self.data_dir = installed.parent.parent / "profile"
        self.data_dir.mkdir(exist_ok=True)
        self.clock = 0.0
        self.calls: list[tuple] = []
        self.events: list[tuple] = []
        self.messages: list[str] = []
        self.requirements: dict[Path, str] = {}
        self.next_pid = 100
        self.agent = {"installed": True, "running": True, "pid": self.pid(), **identity(installed)}
        # The agent's persisted admission record (its data folder): it
        # outlives agent processes.
        self.last_fence = 4
        self.hold: dict | None = None
        self.recording: str | None = None
        self.lost_recordings: list[str] = []
        self.capture_attempts: list[str] = []
        self.hooks: dict[str, object] = {}
        self.state_overrides: list[dict] = []
        self.gui_open = False
        self.accessory_builds: set[int] = set()
        self.gui_ignores_quit = False
        self.without_admission: set[int] = set()
        self.without_guarded_uninstall: set[int] = set()
        # By bundle build: "never" (the agent does not come up), "same_pid"
        # (the old process keeps answering) or identity fields it reports.
        self.start_overrides: dict[int, object] = {}
        self.doctor_reports: dict[int, tuple[int, str]] = {}

    def pid(self) -> int:
        self.next_pid += 1
        return self.next_pid - 1

    def try_record(self) -> None:
        """A capture start (detection, CLI, app) racing the update."""
        if not self.agent["running"]:
            result = "no_agent"
        elif self.hold is not None:
            result = "update_installing"
        elif self.recording:
            result = "busy"
        else:
            self.recording = "s-synthetic"
            result = "started"
        self.capture_attempts.append(result)
        self.events.append(("record", result))

    def agent_view(self) -> dict:
        return {key: self.agent[key] for key in ("pid", "version", "build", "target")}

    def requirement_for(self, path: Path) -> str:
        app = app_of(path)
        helper = "BroScribe Agent.app" in str(path)
        base = self.requirements.get(app, REQUIREMENT)
        return base.format(id=update.HELPER_ID if helper else update.APP_ID)

    def run(self, command):
        program = Path(command[0]).name
        if program == "codesign" and command[1] == "--verify":
            self.calls.append(("verify", command[-1]))
            if (Path(command[-1]) / "meeting-library").exists():
                return 1, "", "unsealed contents"
            return 0, "", ""
        if program == "codesign":
            return 0, "", f"Executable=x\ndesignated => {self.requirement_for(Path(command[-1]))}\n"
        if program == "ditto":
            source, destination = Path(command[1]), Path(command[2])
            shutil.copytree(source, destination, dirs_exist_ok=True)
            self.requirements[destination] = self.requirements.get(source, REQUIREMENT)
            self.calls.append(("copy", source.name, destination.name))
            return 0, "", ""
        if program == "pgrep":
            target = command[-1]
            if "broscribe-app($| )" in target:
                return (0 if self.gui_open else 1), "", ""
            if "broscribed($| )" in target:
                return (0 if self.agent["running"] else 1), "", ""
            return 1, "", ""
        if program == "pkill":
            assert command[1] == "-TERM" and "broscribe-app($| )" in command[-1]
            self.events.append(("quit_gui", bundle_build(self.installed)))
            if not self.gui_ignores_quit:
                self.gui_open = False
            return 0, "", ""
        if program == "broscribe":
            app = Path(command[0]).parents[2]
            build = bundle_build(app)
            if command[1:] == ["daemon", "uninstall", "--help"]:
                return 0, ("" if build in self.without_guarded_uninstall else "--admission-fence --admission-fence-if-idle"), ""
            key = " ".join(command[2:])
            if command[2:5] == ["daemon", "uninstall", "--admission-fence"]:
                key = "daemon uninstall"
            self.calls.append(("cli", key, build))
            hook = self.hooks.get(key)
            if hook:
                hook(build)
            return self.cli(app, build, command[2:])
        raise AssertionError(f"unexpected command {command}")

    def cli(self, app: Path, build: int, args: list[str]):
        if args == ["daemon", "status"]:
            running = self.agent["running"]
            state = None
            if running:
                state = {"pid": self.agent["pid"], "version": self.agent["version"], "protocol": 4,
                         "build": {"build": self.agent["build"], "target": self.agent["target"]},
                         "install": copy.deepcopy(self.hold),
                         "recording": {"session_id": self.recording} if self.recording else None,
                         "stopped": [], "jobs": {"running": None, "waiting": 3, "deferred": 0}}
                if self.state_overrides:
                    state.update(self.state_overrides.pop(0))
            return 0, json.dumps({"installed": self.agent["installed"], "running": running, "state": state}), ""
        if args[:2] == ["daemon", "uninstall"]:
            assert args[2] in ("--admission-fence", "--admission-fence-if-idle")
            if self.hold is None or not self.hold["durable"] or int(args[3]) != self.hold["fence"]:
                return error("update_admission_lost")
            if args[2] == "--admission-fence" and not self.agent["running"]:
                return error("update_admission_lost")
            if args[2] == "--admission-fence-if-idle":
                if self.agent["running"]:
                    return error("busy")
                self.events.append(("idle_uninstall", build))
            if self.agent["running"] and self.recording:
                self.lost_recordings.append(self.recording)
                self.recording = None
            self.agent.update(installed=False, running=False)
            self.events.append(("uninstall", build))
            return 0, "{}", ""
        if args == ["daemon", "stop"]:
            if self.hold is not None:
                return error("update_installing")
            self.agent["running"] = False
            return 0, "{}", ""
        if args == ["daemon", "install"]:
            self.agent["installed"] = True
            return 0, "{}", ""
        if args == ["daemon", "start"]:
            override = self.start_overrides.get(build)
            if self.agent["installed"] and not self.agent["running"] and override != "never":
                pid = self.agent["pid"] if override == "same_pid" else self.pid()
                self.agent.update(running=True, pid=pid, **identity(app))
                if isinstance(override, dict):
                    self.agent.update(override)
                self.events.append(("start", build, pid))
                if build in self.accessory_builds:
                    self.gui_open = True
            return 0, "{}", ""
        if args == ["update", "admission", "acquire"]:
            if build in self.without_admission:
                return 2, "", "error: unrecognized subcommand 'admission'\n"
            if not self.agent["running"]:
                return error("daemon_unavailable")
            if self.hold is not None:
                return error("busy")
            if self.recording:
                return error("recording_active")
            self.last_fence += 1
            self.hold = {"fence": self.last_fence, "durable": True}
            self.events.append(("acquire", build, self.last_fence))
            installed = {"product": "BroScribe", "app_id": update.APP_ID, **identity(app)}
            return 0, json.dumps({"state": "held", "fence": self.last_fence, "durable": True,
                                  "agent": self.agent_view(), "installed": installed}), ""
        if args[:3] == ["update", "admission", "release"] and args[3] == "--fence":
            fence = int(args[4])
            if not self.agent["running"]:
                return error("daemon_unavailable")
            if self.hold is None or self.hold["fence"] != fence:
                return error("update_admission_lost")
            if self.agent_view() | {"pid": 0} != identity(app) | {"pid": 0}:
                return error("update_agent_unproven")
            self.hold = None
            self.events.append(("release", build, self.agent["pid"]))
            return 0, json.dumps({"state": "released", "fence": fence, "agent": self.agent_view()}), ""
        if args == ["doctor"]:
            code, out = self.doctor_reports.get(build, report())
            try:
                value = json.loads(out)
                if isinstance(value, dict):
                    value["data_dir"] = str(self.data_dir)
                    out = json.dumps(value)
            except ValueError:
                pass
            return code, out, ""
        raise AssertionError(f"unexpected broscribe {args}")

    def sleep(self, seconds):
        self.clock += seconds

    def monotonic(self):
        return self.clock

    def now(self):
        return datetime(2026, 11, 17, 12, 0, tzinfo=timezone.utc)

    def log(self, message):
        self.messages.append(message)


@unittest.skipUnless(os.name == "posix", "requires POSIX file locks")
class UpdateTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        root = Path(self._tmp.name).resolve()
        self.applications = root / "Applications"
        self.applications.mkdir()
        self.installed = make_app(self.applications / "BroScribe.app", "0.1.0", 10)
        self.new = make_app(root / "build" / "BroScribe.app", "0.2.0", 12)
        self.backups = root / "backups"
        self.library = root / "Library" / "journal.synthetic"
        self.library.parent.mkdir()
        self.library.write_text("synthetic journal\n")
        self.system = FakeSystem(self.installed)

    def run_update(self, *extra):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = update.main(["--new", str(self.new), "--installed", str(self.installed),
                                "--backup-dir", str(self.backups), *extra], system=self.system)
        return code, err.getvalue()

    def cli_calls(self):
        return [(c[1], c[2]) for c in self.system.calls if c[0] == "cli"]

    def event(self, kind, *detail):
        return next(i for i, e in enumerate(self.system.events) if e[0] == kind and e[1:1 + len(detail)] == detail)

    def assert_untouched(self):
        self.assertEqual(bundle_build(self.installed), 10)
        self.assertFalse(any(p.name.startswith(".BroScribe.app") for p in self.applications.iterdir()))
        self.assertNotIn("daemon uninstall", [c for c, _ in self.cli_calls()])

    def assert_rolled_back(self, code, err):
        self.assertEqual(code, 1)
        self.assertIn("rolled back", err)
        self.assertEqual(bundle_build(self.installed), 10)
        self.assertEqual(sorted(p.name for p in self.applications.iterdir()), ["BroScribe.app"])
        # The previous agent proved it runs build 10 as a new process before
        # the admission ended, and released it with the previous CLI.
        restored = self.system.agent
        self.assertEqual((restored["installed"], restored["running"], restored["version"], restored["build"]),
                         (True, True, "0.1.0", 10))
        self.assertNotEqual(restored["pid"], 100)
        self.assertIsNone(self.system.hold)
        self.assertLess(self.event("start", 10), self.event("release", 10, restored["pid"]))

    def test_installed_bundle_with_unsealed_files_stays_in_place(self):
        library = self.installed / "meeting-library"
        library.mkdir()
        note = library / "keep.md"
        note.write_text("synthetic meeting")
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("Move any personal files out", err)
        self.assert_untouched()
        self.assertEqual(note.read_text(), "synthetic meeting")

    def test_update_holds_the_admission_from_acquire_to_proven_new_build(self):
        code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual(bundle_build(self.installed), 12)
        acquire = self.event("acquire", 10, 5)
        uninstall = self.event("uninstall", 10)
        start = self.event("start", 12)
        release = self.event("release", 12)
        self.assertLess(acquire, uninstall)
        self.assertLess(uninstall, start)
        self.assertLess(start, release)
        self.assertEqual({k: self.system.agent[k] for k in ("installed", "running", "version", "build")},
                         {"installed": True, "running": True, "version": "0.2.0", "build": 12})
        self.assertNotEqual(self.system.agent["pid"], 100)
        self.assertIsNone(self.system.hold)
        self.assertNotIn("daemon stop", [c for c, _ in self.cli_calls()])
        backups = list(self.backups.iterdir())
        self.assertEqual([p.name for p in backups], ["BroScribe-0.1.0-10-20261117T120000Z.app"])
        self.assertEqual(bundle_build(backups[0]), 10)
        self.assertEqual(self.backups.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.library.read_text(), "synthetic journal\n")
        self.assertEqual(sorted(p.name for p in self.applications.iterdir()), ["BroScribe.app"])

    def test_existing_backup_directory_keeps_its_permissions(self):
        self.backups.mkdir()
        self.backups.chmod(0o755)
        keep = self.backups / "keep.txt"
        keep.write_text("synthetic file")
        code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual(self.backups.stat().st_mode & 0o777, 0o755)
        self.assertEqual(keep.read_text(), "synthetic file")
        private = self.backups / "BroScribe-backup-20261117T120000Z"
        self.assertEqual(private.stat().st_mode & 0o777, 0o700)
        self.assertEqual(bundle_build(private / "BroScribe-0.1.0-10-20261117T120000Z.app"), 10)

    def test_capture_after_the_last_status_is_refused_until_release(self):
        # Right before the old agent is unregistered, and once the new agent
        # runs, before the admission is released: both are refused.
        self.system.hooks["daemon uninstall"] = lambda build: self.system.try_record()
        self.system.hooks["update admission release --fence 5"] = lambda build: self.system.try_record()
        code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual(self.system.capture_attempts, ["update_installing", "update_installing"])
        self.assertEqual(self.system.lost_recordings, [])
        self.assertIsNone(self.system.recording)

    def test_recording_started_after_the_last_status_refuses_the_update(self):
        self.system.hooks["update admission acquire"] = lambda build: self.system.try_record()
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("refused the install admission", err)
        self.assertIn("recording_active", err)
        self.assert_untouched()
        self.assertEqual(self.system.recording, "s-synthetic")
        self.assertEqual(self.system.lost_recordings, [])
        self.assertTrue(self.system.agent["installed"] and self.system.agent["running"])
        self.assertIsNone(self.system.hold)

    def test_installation_without_admission_fails_closed(self):
        self.system.without_admission = {10}
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("cannot hold an install admission", err)
        self.assert_untouched()
        self.assertTrue(self.system.agent["running"])

    def test_registered_upgrade_requires_guarded_uninstall_on_both_builds(self):
        for unsupported in (10, 12):
            with self.subTest(unsupported=unsupported):
                self.system.without_guarded_uninstall = {unsupported}
                code, err = self.run_update()
                self.assertEqual(code, 1)
                self.assertIn("cannot unregister the login job under install admission", err)
                self.assert_untouched()
                self.assertIsNone(self.system.hold)

    def test_refuses_while_recording_or_unsaved_or_processing_or_held(self):
        for override in ({"recording": {"session_id": "s-synthetic"}},
                         {"stopped": [{"session_id": "s-synthetic"}]},
                         {"jobs": {"running": {"job": "j-synthetic"}, "waiting": 0, "deferred": 0}},
                         {"install": {"fence": 3, "durable": True}}):
            with self.subTest(override=override):
                self.system.state_overrides = [override]
                code, err = self.run_update()
                self.assertEqual(code, 1)
                self.assert_untouched()
                self.assertFalse(self.backups.exists())

    def test_refuses_agent_of_unknown_or_other_build(self):
        for override in ({"build": None}, {"build": {"build": 9, "target": TARGET}},
                         {"build": {"build": 10, "target": "x86_64-apple-darwin"}}):
            with self.subTest(override=override):
                self.system.state_overrides = [override]
                code, err = self.run_update()
                self.assertEqual(code, 1)
                self.assertIn("rollback could not be proven", err)
                self.assert_untouched()

    def test_refuses_registered_agent_that_does_not_answer(self):
        self.system.agent["running"] = False
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("registered but does not answer", err)
        self.assert_untouched()

    def test_refuses_open_app(self):
        self.system.gui_open = True
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("quit it first", err)
        self.assert_untouched()

    def test_refuses_older_or_same_build(self):
        shutil.rmtree(self.new)
        make_app(self.new, "0.0.9", 9)
        self.assertEqual(self.run_update()[0], 1)
        shutil.rmtree(self.new)
        make_app(self.new, "0.1.0", 10)
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("does not exceed", err)
        self.assert_untouched()
        self.assertEqual(self.run_update("--allow-same-build")[0], 0)

    def test_refuses_build_information_that_disagrees_with_info_plist(self):
        shutil.rmtree(self.new)
        make_app(self.new, "0.2.0", 12, info_build=11)
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("build-info.json", err)
        self.assert_untouched()

    def test_refuses_changed_or_ad_hoc_requirement(self):
        self.system.requirements[self.new] = 'identifier "{id}" and anchor apple generic and certificate leaf = H"00"'
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("designated requirement", err)
        self.system.requirements[self.new] = 'cdhash H"0123"'
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("ad-hoc", err)
        self.assert_untouched()

    def test_dry_run_changes_nothing(self):
        code, err = self.run_update("--dry-run")
        self.assertEqual(code, 0, err)
        self.assert_untouched()
        self.assertFalse(self.backups.exists())
        self.assertIsNone(self.system.hold)
        self.assertNotIn("update admission acquire", [c for c, _ in self.cli_calls()])
        self.assertIn("dry run", self.system.messages[-1])

    def test_new_agent_that_never_answers_rolls_back_under_the_admission(self):
        self.system.start_overrides = {12: "never"}
        self.system.hooks["update admission release --fence 5"] = lambda build: self.system.try_record()
        code, err = self.run_update()
        self.assert_rolled_back(code, err)
        self.assertEqual(self.system.capture_attempts, ["update_installing"])
        self.assertIn(("idle_uninstall", 12), self.system.events)

    def test_same_version_rebuild_needs_the_exact_build_to_run(self):
        shutil.rmtree(self.new)
        make_app(self.new, "0.1.0", 11)
        # The restarted agent reports the right version but the old build.
        self.system.start_overrides = {11: {"build": 10}}
        code, err = self.run_update()
        self.assert_rolled_back(code, err)
        self.assertIn("not 0.1.0 build 11", err)
        self.system.start_overrides = {}
        code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual((self.system.agent["version"], self.system.agent["build"]), ("0.1.0", 11))

    def test_agent_that_did_not_restart_rolls_back(self):
        self.system.start_overrides = {12: "same_pid"}
        code, err = self.run_update()
        self.assert_rolled_back(code, err)
        self.assertIn("has not restarted", err)

    def test_doctor_reporting_ok_false_with_exit_zero_rolls_back(self):
        self.system.doctor_reports = {12: report("library")}
        code, err = self.run_update()
        self.assert_rolled_back(code, err)
        self.assertIn("library now fails", err)

    def test_rollback_quits_rejected_builds_menu_bar_app_before_restoring(self):
        self.system.accessory_builds = {12}
        self.system.doctor_reports = {12: report("library")}
        code, err = self.run_update()
        self.assert_rolled_back(code, err)
        self.assertFalse(self.system.gui_open)
        self.assertLess(self.event("quit_gui", 12), self.event("start", 10))

    def test_rollback_stays_failed_if_rejected_builds_app_cannot_quit(self):
        self.system.accessory_builds = {12}
        self.system.gui_ignores_quit = True
        self.system.doctor_reports = {12: report("library")}
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("rollback failed", err)
        self.assertIn("the rejected build's app to quit", err)
        self.assertEqual(bundle_build(self.installed), 12)
        self.assertIsNotNone(self.system.hold)

    def test_doctor_problem_present_before_is_not_a_regression(self):
        self.system.doctor_reports = {10: report("me"), 12: report("me")}
        code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual(bundle_build(self.installed), 12)

    def test_malformed_doctor_report_refuses_or_rolls_back(self):
        malformed = [(0, "not json"), (0, json.dumps({"ok": True})),
                     (0, json.dumps({"ok": True, "checks": [{"check": "config", "ok": True}]})),
                     (0, report("config")[1].replace('"ok": false', '"ok": true', 1)), (1, report()[1])]
        for case in malformed:
            with self.subTest(baseline=case):
                self.system.doctor_reports = {10: case}
                code, err = self.run_update()
                self.assertEqual(code, 1)
                self.assertIn("doctor", err)
                self.assert_untouched()
        for case in malformed:
            with self.subTest(after=case):
                self.system = FakeSystem(self.installed)
                self.system.doctor_reports = {12: case}
                code, err = self.run_update()
                self.assert_rolled_back(code, err)
        # A required check the new build no longer reports is a regression.
        self.system = FakeSystem(self.installed)
        _, full = report()
        fewer = json.loads(full)
        fewer["checks"] = [c for c in fewer["checks"] if c["check"] != "me"]
        self.system.doctor_reports = {12: (0, json.dumps(fewer))}
        code, err = self.run_update()
        self.assert_rolled_back(code, err)
        self.assertIn("me is no longer checked", err)

    def test_failed_rollback_keeps_the_admission_held(self):
        self.system.start_overrides = {12: "never", 10: "never"}
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("rollback failed", err)
        self.assertEqual(self.system.hold, {"fence": 5, "durable": True})
        self.assertIn("update admission release --fence 5", err)
        self.assertNotIn("release", [e[0] for e in self.system.events])

    def test_failed_uninstall_after_unregistering_restarts_before_releasing_admission(self):
        original = self.system.cli

        def failed_after_removal(app, build, args):
            answer = original(app, build, args)
            if args[:2] == ["daemon", "uninstall"]:
                return error("synthetic_failure_after_unregistration")
            return answer

        self.system.cli = failed_after_removal

        def locks_free(_build):
            for relative in ("daemon-start.lock", "run/app.lock"):
                with (self.system.data_dir / relative).open("a") as handle:
                    fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)

        self.system.hooks["daemon start"] = locks_free
        self.system.hooks["update admission release --fence 5"] = locks_free
        code, err = self.run_update()
        self.assert_rolled_back(code, err)
        self.assertIn("synthetic_failure_after_unregistration", err)

    def test_profile_directory_is_created_privately_and_symlinks_are_refused(self):
        self.system.agent.update(installed=False, running=False)
        self.system.data_dir.rmdir()
        target = self.system.data_dir.parent / "other"
        target.mkdir()
        self.system.data_dir.symlink_to(target, target_is_directory=True)
        code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("not a plain directory", err)
        self.assert_untouched()
        self.assertEqual(list(target.iterdir()), [])

        self.system.data_dir.unlink()
        code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual(self.system.data_dir.stat().st_mode & 0o777, 0o700)

    def test_final_check_refuses_a_gui_with_the_instance_lock_even_before_pgrep_sees_it(self):
        app = update.AppReservation(self.system.data_dir)
        app.acquire()
        try:
            code, err = self.run_update()
        finally:
            app.release()
        self.assertEqual(code, 1)
        self.assertIn("quit it first", err)
        self.assert_untouched()
        self.assertIsNone(self.system.hold)
        self.assertTrue(self.system.agent["running"])

    def test_old_gui_cannot_acquire_instance_lock_during_rename(self):
        self.system.agent.update(installed=False, running=False)
        original = Path.rename
        checked = []

        def rename(path, target):
            if path == self.installed:
                competing_gui = update.AppReservation(self.system.data_dir)
                with self.assertRaisesRegex(update.UpdateError, "quit it first"):
                    competing_gui.acquire()
                checked.append(path)
            return original(path, target)

        with mock.patch.object(Path, "rename", rename):
            code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual(checked, [self.installed])
        app = update.AppReservation(self.system.data_dir)
        app.acquire()
        app.release()

    def test_unregistered_agent_stays_unregistered(self):
        self.system.agent.update(installed=False, running=False)
        code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertEqual(bundle_build(self.installed), 12)
        calls = [c for c, _ in self.cli_calls()]
        self.assertNotIn("daemon install", calls)
        self.assertNotIn("update admission acquire", calls)

    def test_unregistered_agent_cannot_start_between_absence_check_and_swap(self):
        self.system.agent.update(installed=False, running=False)
        original_rename = Path.rename

        def rename(path: Path, target) -> Path:
            if path == self.installed:
                self.assertEqual(bundle_build(path), 10)
                # The old binary was launched just before the rename. It
                # checks the real profile lock before it can bind or record.
                with (self.system.data_dir / "daemon-start.lock").open("a") as lock:
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        self.system.events.append(("startup_fenced", 10))
                    else:
                        self.system.agent["running"] = True
                        self.system.try_record()
            return original_rename(path, target)

        with mock.patch.object(Path, "rename", rename):
            code, err = self.run_update()
        self.assertEqual(code, 0, err)
        self.assertIn(("startup_fenced", 10), self.system.events)
        self.assertFalse(self.system.agent["running"])
        self.assertIsNone(self.system.recording)

    def test_offline_swap_refuses_instance_claim_even_when_cli_says_absent(self):
        self.system.agent.update(installed=False, running=False)
        run = self.system.data_dir / "run"
        run.mkdir(mode=0o700)
        pid = run / "broscribed.pid"
        pid.write_text("4242\n")
        with pid.open("r") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("instance claim", err)
        self.assert_untouched()
        self.assertEqual(pid.read_text(), "4242\n")

    def test_unregistered_agent_failed_second_rename_restores_the_original(self):
        # No agent to hold an admission: the installed app moves aside, then
        # moving the staged copy into its place fails once.
        self.system.agent.update(installed=False, running=False)
        stamp = "20261117T120000Z"
        staging, previous = f".BroScribe.app.update-{stamp}", f".BroScribe.app.previous-{stamp}"
        renames: list[tuple[str, str]] = []
        original_rename = Path.rename

        def rename(path: Path, target) -> Path:
            renames.append((path.name, Path(target).name))
            if (path.name, Path(target)) == (staging, self.installed) and renames.count(renames[-1]) == 1:
                raise OSError("synthetic rename failure")
            return original_rename(path, target)

        with mock.patch.object(Path, "rename", rename):
            code, err = self.run_update()
        self.assertEqual(code, 1)
        self.assertIn("rolled back", err)
        self.assertIn("synthetic rename failure", err)
        self.assertEqual(renames, [("BroScribe.app", previous), (staging, "BroScribe.app"),
                                   (previous, "BroScribe.app")])
        self.assertEqual(bundle_build(self.installed), 10)
        self.assertEqual(identity(self.installed)["build"], 10)
        self.assertEqual((self.installed / update.CLI).read_text(), "synthetic cli\n")
        self.assertEqual(sorted(p.name for p in self.applications.iterdir()), ["BroScribe.app"])
        self.assertEqual((self.system.agent["installed"], self.system.agent["running"]), (False, False))
        self.assertIsNone(self.system.hold)
        self.assertEqual(self.system.events, [])
        self.assertEqual({c for c, _ in self.cli_calls()}, {"daemon status", "doctor"})
        self.assertEqual(self.library.read_text(), "synthetic journal\n")
        self.assertEqual([bundle_build(p) for p in self.backups.iterdir()], [10])


if __name__ == "__main__":
    unittest.main()

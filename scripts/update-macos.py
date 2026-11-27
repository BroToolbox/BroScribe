#!/usr/bin/env python3
"""Manually replace an installed BroScribe.app with a newer signed build.

  scripts/update-macos.py --new path/to/BroScribe.app
      [--installed /Applications/BroScribe.app] [--backup-dir DIR]
      [--allow-same-build] [--dry-run]

The library, journals, queue and settings are never touched: only the app
bundle is replaced. Order of operations:

 1. Check the new bundle: same bundle identifiers, build information that
    agrees with Info.plist, valid signature, a real (not ad-hoc) signer whose
    designated requirements equal the installed app's and helper's (macOS
    keeps permission grants only for an unchanged requirement), and a build
    number above the installed one.
 2. Refuse while the installed agent records, holds a stopped recording that
    is not saved yet, or runs a worker job, while it runs another build than
    the installed one, while it is registered but does not answer, and while
    the BroScribe app is open.
 3. Run `broscribe doctor` as a baseline. --dry-run stops here.
 4. Back up the installed app (default: ~/Library/Application Support/
    BroScribe/app-backups/) and check the copy's requirement.
 5. Take the agent's install admission (`broscribe update admission
    acquire`, from the installed app): the agent grants it only while
    nothing records, refuses every capture start from then on and keeps it,
    durably, across its own restarts until it is released. An installation
    whose CLI or agent cannot hold it is refused: nothing is changed.
 6. Hold the profile startup and app-instance locks, then unregister the login job
    (`daemon uninstall --admission-fence`) and wait for the endpoint, its
    instance claim and its workers to clear.
 7. Swap in the new app with same-volume renames, release both locks,
    register and start the new agent, and wait until a new process proves it
    runs exactly the new build and still holds the admission.
 8. Run `broscribe doctor` again: any required check that passed before and
    fails now, or is no longer reported, is a regression.
 9. Release the admission with the new app's CLI.
A failure after step 5 rolls back while the admission is held. The script
takes the startup lock again if needed and waits for any rejected build's
menu-bar app to exit before restoring the previous app. The restored agent
must prove it runs the previous build before the admission is released.
Clients connected to the old agent (CLI, MCP) reconnect.

Without a running or registered agent the script checks the endpoint and
instance claim under the startup lock. It keeps the lock through the swap
and any rollback and leaves the agent unregistered.
"""

from __future__ import annotations

import argparse
import json
import os
import plistlib
import re
import shutil
import stat
import subprocess
import sys
import time
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from itertools import count
from pathlib import Path

if os.name == "posix":
    import fcntl

APP_ID = "org.broscribe.BroScribe"
HELPER_ID = "org.broscribe.Agent"
PRODUCT = "BroScribe"
HELPER = Path("Contents/Library/LoginItems/BroScribe Agent.app")
BUILD_INFO = Path("Contents/Resources/build-info.json")
CLI = Path("Contents/MacOS/broscribe")
GUI = Path("Contents/MacOS/broscribe-app")
WORKER = Path("Contents/MacOS/broscribe-worker")
DAEMON = HELPER / "Contents/MacOS/broscribed"
ADMISSION = ("update", "admission")
# Agent answers that refuse the admission: something records or holds the
# capture slot, another admission is held, or a signed update is pending.
ADMISSION_REFUSALS = {"recording_active", "busy", "update_installing", "update_pending"}
DOCTOR_STATES = {"ok", "problem", "unknown"}


class UpdateError(Exception):
    pass


class CliError(UpdateError):
    """A `broscribe --json` command failed; `code` is its JSON error code,
    `None` when it answered no JSON error (e.g. an unknown command)."""

    def __init__(self, what: str, code: str | None, message: str):
        super().__init__(f"{what} failed" + (f" ({code})" if code else "") + f": {message}")
        self.code = code


class System:
    """The commands and clock the update uses; tests substitute a fake."""

    def run(self, command: list[str]) -> tuple[int, str, str]:
        result = subprocess.run(command, capture_output=True, text=True)
        return result.returncode, result.stdout, result.stderr

    def sleep(self, seconds: float) -> None:
        time.sleep(seconds)

    def monotonic(self) -> float:
        return time.monotonic()

    def now(self) -> datetime:
        return datetime.now(timezone.utc)

    def log(self, message: str) -> None:
        print(f"update-macos: {message}", file=sys.stderr)


def ensure_directory(path: Path, *, private: bool = False) -> None:
    path.mkdir(parents=True, mode=0o700, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode):
        raise UpdateError(f"the profile path is not a plain directory: {path}")
    if private and (info.st_uid != os.getuid() or info.st_mode & 0o077):
        raise UpdateError(f"the profile directory is not private: {path}")


class AppReservation:
    """The desktop app's never-unlinked, nonblocking instance lock."""

    def __init__(self, data_dir: Path):
        self.path = data_dir / "run" / "app.lock"
        self.fd: int | None = None

    def acquire(self) -> None:
        if self.fd is not None:
            return
        ensure_directory(self.path.parent, private=True)
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
        try:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise UpdateError("the app instance lock is not a regular file")
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise UpdateError("BroScribe is open; quit it first") from None
        except BaseException:
            os.close(fd)
            raise
        self.fd = fd

    def release(self) -> None:
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


class StartupReservation:
    """The same never-unlinked profile lock that broscribed takes before bind."""

    def __init__(self, system: System, data_dir: Path, timeout: float):
        self.system = system
        self.path = data_dir / "daemon-start.lock"
        self.timeout = timeout
        self.fd: int | None = None

    def acquire(self) -> None:
        if self.fd is not None:
            return
        ensure_directory(self.path.parent)
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
        try:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise UpdateError("the agent startup lock is not a regular file")
            deadline = self.system.monotonic() + self.timeout
            while True:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    remaining = deadline - self.system.monotonic()
                    if remaining <= 0:
                        raise UpdateError("timed out waiting for the profile's agent startup lock") from None
                    self.system.sleep(min(0.25, remaining))
        except BaseException:
            os.close(fd)
            raise
        self.fd = fd

    def release(self) -> None:
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


def instance_claim_held(data_dir: Path) -> bool:
    """Probe the Unix agent claim without creating or writing its PID file."""
    run = data_dir / "run"
    try:
        info = run.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise UpdateError("the profile's agent run directory is not private")
        fd = os.open(run / "broscribed.pid", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return False
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise UpdateError("the profile's agent PID file is not regular")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False
    finally:
        os.close(fd)


def _int(value) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


@dataclass
class Bundle:
    path: Path
    version: str
    build: int
    target: str

    @classmethod
    def read(cls, path: Path, label: str) -> "Bundle":
        if not path.is_dir():
            raise UpdateError(f"{label} app not found: {path}")
        try:
            with (path / "Contents/Info.plist").open("rb") as handle:
                info = plistlib.load(handle)
            with (path / HELPER / "Contents/Info.plist").open("rb") as handle:
                helper = plistlib.load(handle)
        except (OSError, plistlib.InvalidFileException) as error:
            raise UpdateError(f"{label} app has no readable Info.plist: {error}") from None
        if info.get("CFBundleIdentifier") != APP_ID or helper.get("CFBundleIdentifier") != HELPER_ID:
            raise UpdateError(f"{label} app is not BroScribe ({APP_ID} with helper {HELPER_ID})")
        build = str(info.get("CFBundleVersion", ""))
        if not build.isdigit():
            raise UpdateError(f"{label} app has no numeric build number (CFBundleVersion {build!r})")
        version = str(info.get("CFBundleShortVersionString", ""))
        # The agent reports the build and target it read from this file; the
        # restarted agent is proven against it.
        try:
            with (path / BUILD_INFO).open("rb") as handle:
                identity = json.load(handle)
        except (OSError, ValueError) as error:
            raise UpdateError(f"{label} app has no readable {BUILD_INFO.name}: {error}") from None
        if not isinstance(identity, dict) or identity.get("schema") != 1 or identity.get("product") != PRODUCT:
            raise UpdateError(f"{label} app's {BUILD_INFO.name} is not BroScribe build information (schema 1)")
        target = identity.get("target")
        if identity.get("version") != version or identity.get("build") != int(build) or not _int(identity.get("build")):
            raise UpdateError(f"{label} app's {BUILD_INFO.name} names {identity.get('version')} "
                              f"({identity.get('build')}), not its Info.plist {version} ({build})")
        if not isinstance(target, str) or not target:
            raise UpdateError(f"{label} app's {BUILD_INFO.name} names no target")
        return cls(path, version, int(build), target)

    def describe(self) -> str:
        return f"{self.version} build {self.build} ({self.target})"

    def payload(self) -> dict:
        """The installation identity the CLI reports (`PayloadIdentity`)."""
        return {"product": PRODUCT, "app_id": APP_ID, "version": self.version, "build": self.build,
                "target": self.target}


@dataclass(frozen=True)
class Admission:
    """The durable install admission the installed agent granted."""

    fence: int
    pid: int


@dataclass(frozen=True)
class Health:
    """A `broscribe doctor --json` report: required checks by name."""

    ok: bool
    data_dir: Path
    required: Counter
    failing: Counter

    @classmethod
    def parse(cls, report) -> "Health":
        def malformed(why: str) -> UpdateError:
            return UpdateError(f"`broscribe doctor` did not report the expected schema: {why}")

        if not isinstance(report, dict) or not isinstance(report.get("ok"), bool):
            raise malformed("no boolean `ok`")
        data_dir = report.get("data_dir")
        if not isinstance(data_dir, str) or not Path(data_dir).is_absolute():
            raise malformed("no absolute profile data directory")
        checks = report.get("checks")
        if not isinstance(checks, list) or not checks:
            raise malformed("no `checks`")
        required, failing = Counter(), Counter()
        for check in checks:
            if not isinstance(check, dict):
                raise malformed("a check is not an object")
            name, ok, state, needed = check.get("check"), check.get("ok"), check.get("state"), check.get("required")
            if not isinstance(name, str) or not name or not isinstance(ok, bool) or not isinstance(needed, bool) \
                    or state not in DOCTOR_STATES or ok != (state == "ok"):
                raise malformed(f"check {name!r} lacks a consistent check/ok/state/required")
            if needed:
                required[name] += 1
                if not ok:
                    failing[name] += 1
        if report["ok"] != (not failing):
            raise malformed("`ok` disagrees with its required checks")
        return cls(report["ok"], Path(data_dir), required, failing)

    def regressions(self, after: "Health") -> list[str]:
        """Required checks that fail after but not before, or that are no
        longer reported."""
        problems = [f"{name} now fails" for name in sorted(after.failing - self.failing)]
        problems += [f"{name} is no longer checked" for name in sorted(self.required - after.required)]
        return problems


class Updater:
    def __init__(self, system: System, new: Path, installed: Path, backup_dir: Path,
                 allow_same_build: bool = False, dry_run: bool = False, timeout: float = 60.0):
        self.system = system
        self.new_path = new
        self.installed_path = installed.resolve()
        self.backup_dir = backup_dir
        self.allow_same_build = allow_same_build
        self.dry_run = dry_run
        self.timeout = timeout

    # ------------------------------------------------------------ commands

    def must(self, command: list[str], what: str) -> str:
        code, out, err = self.system.run(command)
        if code != 0:
            raise UpdateError(f"{what} failed: {(err or out).strip()}")
        return out

    def requirement(self, path: Path) -> str:
        code, out, err = self.system.run(["codesign", "--display", "--requirements", "-", str(path)])
        text = out + err
        lines = [line[len("designated => "):] for line in text.splitlines() if line.startswith("designated => ")]
        if code != 0 or len(lines) != 1:
            raise UpdateError(f"cannot read the designated requirement of {path}")
        return lines[0]

    def cli(self, app: Path, *args: str) -> tuple[int, str, str]:
        return self.system.run([str(app / CLI), "--json", *args])

    def cli_json(self, app: Path, what: str, *args: str) -> dict:
        """Runs a `broscribe --json` command and returns its JSON object."""
        code, out, err = self.cli(app, *args)
        try:
            value = json.loads(out)
        except json.JSONDecodeError:
            value = None
        if code != 0:
            error = value.get("error") if isinstance(value, dict) else None
            if isinstance(error, dict) and isinstance(error.get("code"), str):
                raise CliError(what, error["code"], str(error.get("message", "")))
            raise CliError(what, None, (err or out).strip() or f"exit {code}")
        if not isinstance(value, dict):
            raise UpdateError(f"{what} did not answer with a JSON object")
        return value

    def daemon_state(self, app: Path) -> dict:
        code, out, err = self.cli(app, "daemon", "status")
        if code != 0:
            raise UpdateError(f"cannot read the agent status: {(err or out).strip()}")
        try:
            status = json.loads(out)
        except json.JSONDecodeError:
            raise UpdateError("the agent status was not JSON") from None
        if not isinstance(status, dict):
            raise UpdateError("the agent status was not a JSON object")
        return status

    @staticmethod
    def process_pattern(path: Path) -> str:
        # A POSIX ERE, anchored to this bundle's executable and its argument boundary.
        escaped = re.sub(r"([.\[\]\\*^$+?{}|()])", r"\\\1", str(path))
        return f"^{escaped}($| )"

    def running(self, path: Path) -> bool:
        code, _, err = self.system.run(["pgrep", "-u", str(os.geteuid()), "-f", self.process_pattern(path)])
        if code not in (0, 1):
            raise UpdateError(f"cannot check whether {path.name} is running: {err.strip()}")
        return code == 0

    def wait_until(self, predicate, what: str) -> None:
        deadline = self.system.monotonic() + self.timeout
        while not predicate():
            if self.system.monotonic() >= deadline:
                raise UpdateError(f"timed out waiting for {what}")
            self.system.sleep(0.5)

    # -------------------------------------------------------------- checks

    def check_sealed(self, path: Path, expected_requirement: str | None = None) -> None:
        self.must(
            ["codesign", "--verify", "--deep", "--strict", str(path)],
            f"verifying the sealed BroScribe bundle at {path}. Move any personal files out and restore the signed bundle before retrying",
        )

        if expected_requirement is not None and self.requirement(path) != expected_requirement:
            raise UpdateError(f"the sealed BroScribe bundle at {path} has a different designated requirement")

    def check_bundles(self) -> tuple[Bundle, Bundle, str]:
        new = Bundle.read(self.new_path, "new")
        installed = Bundle.read(self.installed_path, "installed")
        if new.path.resolve() == installed.path.resolve():
            raise UpdateError("the new app is the installed app")
        if new.build < installed.build or (new.build == installed.build and not self.allow_same_build):
            raise UpdateError(f"new build {new.build} does not exceed installed build {installed.build}")
        self.check_sealed(installed.path)
        self.check_sealed(new.path)
        installed_requirement = self.requirement(installed.path)
        for relative in (Path("."), HELPER):
            old_req = installed_requirement if relative == Path(".") else self.requirement(installed.path / relative)
            new_req = self.requirement(new.path / relative)
            if "cdhash" in new_req or "cdhash" in old_req:
                raise UpdateError("ad-hoc signatures change with every build and would drop permission grants; "
                                  "use a stable signing identity")
            if old_req != new_req:
                raise UpdateError(f"designated requirement of {relative} changed; macOS would treat it as another "
                                  f"app.\n  installed: {old_req}\n  new:       {new_req}")
        return new, installed, installed_requirement

    @staticmethod
    def identity_problem(pid, version, build, target, bundle: Bundle) -> str | None:
        """Why an agent reporting `pid`/`version`/`build`/`target` does not
        prove it runs exactly `bundle`. An unknown build never matches."""
        if build is None or target is None:
            return f"the agent (process {pid}) cannot tell which build it runs"
        if version != bundle.version or not _int(build) or build != bundle.build or target != bundle.target:
            return f"the agent (process {pid}) runs {version} build {build} ({target}), not {bundle.describe()}"
        return None

    @classmethod
    def state_problem(cls, state: dict, bundle: Bundle) -> str | None:
        """[`identity_problem`] of a `daemon status` state (`build` nests
        the build number and target)."""
        build = state.get("build")
        build = build if isinstance(build, dict) else {}
        return cls.identity_problem(state.get("pid"), state.get("version"), build.get("build"),
                                    build.get("target"), bundle)

    @classmethod
    def agent_problem(cls, reply: dict, bundle: Bundle, not_pids: set[int]) -> str | None:
        """Why the `agent` an admission answer names is not a process outside
        `not_pids` running exactly `bundle`."""
        agent = reply.get("agent")
        if not isinstance(agent, dict) or not _int(agent.get("pid")) or agent["pid"] < 1:
            return "the admission answer names no agent process"
        if agent["pid"] in not_pids:
            return f"the agent has not restarted (still process {agent['pid']})"
        return cls.identity_problem(agent["pid"], agent.get("version"), agent.get("build"), agent.get("target"),
                                    bundle)

    def check_agent(self, installed: Bundle) -> bool:
        """Refuses while capture or processing is active, while the agent runs
        another build, or while it is registered but does not answer; whether
        an agent runs (and must hold the install admission). A snapshot: only
        the admission itself excludes captures that start after it."""
        status = self.daemon_state(installed.path)
        if not status.get("running"):
            if status.get("installed"):
                raise UpdateError("the agent is registered but does not answer, so it cannot hold the install "
                                  "admission and launchd may start it during the replacement; start it with "
                                  "`broscribe daemon start` and try again")
            return False
        state = status.get("state")
        if not isinstance(state, dict):
            raise UpdateError("the running agent reported no state")
        if state.get("recording"):
            raise UpdateError("a recording is in progress; stop it and wait for it to be saved")
        if state.get("stopped"):
            raise UpdateError("a stopped recording is not saved yet; save or discard it first")
        if (state.get("jobs") or {}).get("running"):
            raise UpdateError("a processing job is running; wait for it to finish")
        if state.get("install") is not None:
            raise UpdateError("another installation holds the agent's install admission; let it finish first")
        problem = self.state_problem(state, installed)
        if problem:
            raise UpdateError(f"{problem}, so a rollback could not be proven; restart the agent "
                              "(`broscribe daemon stop`, `broscribe daemon start`) and try again")
        return True

    def check_gui_closed(self, app: Path) -> None:
        if self.running(app / GUI):
            raise UpdateError("BroScribe is open; quit it first")

    def doctor(self, app: Path) -> Health:
        # doctor exits 0 when it reports problems: its JSON is the verdict.
        code, out, err = self.cli(app, "doctor")
        if code != 0:
            raise UpdateError(f"`broscribe doctor` failed: {(err or out).strip() or f'exit {code}'}")
        try:
            report = json.loads(out)
        except json.JSONDecodeError:
            raise UpdateError("`broscribe doctor` did not report JSON") from None
        return Health.parse(report)

    # ----------------------------------------------------------- admission

    def acquire(self, installed: Bundle) -> Admission:
        try:
            reply = self.cli_json(installed.path, "acquiring the install admission", *ADMISSION, "acquire")
        except CliError as error:
            if error.code is None:
                raise UpdateError(
                    f"the installed BroScribe cannot hold an install admission ({error}); without it a recording "
                    "could start while the agent is replaced, so nothing was changed. Once nothing records, "
                    "unregister the agent yourself (`broscribe daemon uninstall`) and run this again; then "
                    "register the new one with `broscribe daemon install` and `broscribe daemon start`") from None
            if error.code in ADMISSION_REFUSALS:
                raise UpdateError(f"the agent refused the install admission, so nothing was changed: {error}") \
                    from None
            raise
        fence = reply.get("fence")
        if reply.get("state") != "held" or reply.get("durable") is not True or not _int(fence) or fence < 1:
            raise UpdateError(f"the install admission answer is not a held durable fence: {json.dumps(reply)}; "
                              "check `broscribe daemon status` before recording")
        problem = self.agent_problem(reply, installed, set())
        if problem is None and reply.get("installed") != installed.payload():
            problem = (f"the CLI reports the installation {json.dumps(reply.get('installed'))}, not "
                       f"{installed.describe()} at {installed.path}")
        if problem:
            # The agent admits no capture while it is held; the agent has to
            # run the installed build before the fence can be released.
            raise UpdateError(f"{problem}; nothing was changed, but {self.still_held(fence)}")
        return Admission(fence, reply["agent"]["pid"])

    def proof_problem(self, bundle: Bundle, fence: int, not_pids: set[int], observed: set[int]) -> str | None:
        """Why the agent does not (yet) prove it is a new process running
        exactly `bundle` that still holds durable admission `fence`."""
        try:
            status = self.daemon_state(bundle.path)
        except UpdateError as error:
            return str(error)
        state = status.get("state")
        if not status.get("running") or not isinstance(state, dict):
            return "the agent does not answer"
        pid = state.get("pid")
        if not _int(pid) or pid < 1:
            return "the agent's status names no process"
        observed.add(pid)
        if pid in not_pids:
            return f"the agent has not restarted (still process {pid})"
        problem = self.state_problem(state, bundle)
        if problem:
            return problem
        hold = state.get("install")
        if not isinstance(hold, dict) or hold.get("fence") != fence or hold.get("durable") is not True \
                or not _int(hold.get("fence")):
            return f"the agent (process {pid}) no longer holds install admission {fence}"
        return None

    def prove(self, bundle: Bundle, fence: int, not_pids: set[int], observed: set[int]) -> None:
        last: list[str | None] = [None]

        def proven() -> bool:
            last[0] = self.proof_problem(bundle, fence, not_pids, observed)
            return last[0] is None

        try:
            self.wait_until(proven, f"the agent to prove it runs {bundle.describe()}")
        except UpdateError as error:
            raise UpdateError(f"{error}: {last[0]}") from None

    def release_after_proof(self, bundle: Bundle, admission: Admission, not_pids: set[int],
                            observed: set[int] | None = None) -> None:
        """Releases `admission` with `bundle`'s CLI once its agent proved it
        runs `bundle` and holds the admission; a failure keeps it held."""
        self.prove(bundle, admission.fence, not_pids, set() if observed is None else observed)
        reply = self.cli_json(bundle.path, f"releasing install admission {admission.fence}", *ADMISSION,
                              "release", "--fence", str(admission.fence))
        problem = self.agent_problem(reply, bundle, not_pids)
        if reply.get("state") != "released" or reply.get("fence") != admission.fence or problem:
            raise UpdateError(f"releasing install admission {admission.fence} answered {json.dumps(reply)}"
                              + (f" ({problem})" if problem else ""))

    @staticmethod
    def still_held(fence: int) -> str:
        return (f"install admission {fence} may still be held, and no recording starts while it is; check "
                f"`broscribe daemon status` and end it with `broscribe update admission release --fence {fence}`")

    # ----------------------------------------------------------- lifecycle

    def require_guarded_uninstall(self, app: Path, *, require_idle: bool = False) -> None:
        help_text = self.must([str(app / CLI), "daemon", "uninstall", "--help"],
                              "checking the CLI's guarded uninstall support")
        if "--admission-fence" not in help_text.split() or (
                require_idle and "--admission-fence-if-idle" not in help_text.split()):
            raise UpdateError(f"{app} cannot unregister the login job under install admission")

    def unregister_agent(self, app: Path, fence: int) -> None:
        # `daemon stop` refuses while an install admission is held; with the
        # admission nothing records, so unregistering ends no capture.
        self.cli_json(app, "unregistering the agent", "daemon", "uninstall", "--admission-fence", str(fence))

    def agent_absent(self, app: Path, data_dir: Path) -> bool:
        status = self.daemon_state(app)
        return (status.get("running") is False and status.get("installed") is False
                and not instance_claim_held(data_dir))

    def wait_for_agent_exit(self, app: Path, data_dir: Path) -> None:
        self.wait_until(lambda: not self.running(app / DAEMON) and self.agent_absent(app, data_dir),
                        "the unregistered agent to exit")
        self.wait_until(lambda: not self.running(app / WORKER), "workers to exit")

    def quit_rejected_gui(self, app: Path) -> None:
        gui = app / GUI
        if self.running(gui):
            code, _, err = self.system.run(["pkill", "-TERM", "-u", str(os.geteuid()),
                                            "-f", self.process_pattern(gui)])
            if code not in (0, 1):
                raise UpdateError(f"could not terminate the rejected build's app: {err.strip()}")
            self.wait_until(lambda: not self.running(gui), "the rejected build's app to quit")

    def start_agent(self, app: Path) -> None:
        self.must([str(app / CLI), "--json", "daemon", "install"], "registering the agent")
        self.must([str(app / CLI), "--json", "daemon", "start"], "starting the agent")

    def copy(self, source: Path, destination: Path) -> None:
        self.must(["ditto", str(source), str(destination)], f"copying {source.name}")

    # ----------------------------------------------------------------- run

    def run(self) -> int:
        new, installed, installed_requirement = self.check_bundles()
        agent = self.check_agent(installed)
        self.check_gui_closed(installed.path)
        baseline = self.doctor(installed.path)
        if agent:
            self.require_guarded_uninstall(installed.path)
            self.require_guarded_uninstall(new.path, require_idle=True)
        plan = (f"replace {installed.path} {installed.describe()} with {new.describe()}; "
                f"backup in {self.backup_dir}")
        if self.dry_run:
            self.system.log(f"dry run: checks passed; would {plan}"
                            + (" under the agent's install admission" if agent else ""))
            return 0
        self.system.log(plan)

        stamp = self.system.now().strftime("%Y%m%dT%H%M%SZ")
        backup_dir = self.backup_dir
        try:
            backup_dir.mkdir(mode=0o700, parents=True)
        except FileExistsError:
            for sequence in count(1):
                suffix = "" if sequence == 1 else f"-{sequence}"
                backup_dir = self.backup_dir / f"BroScribe-backup-{stamp}{suffix}"
                try:
                    backup_dir.mkdir(mode=0o700)
                    break
                except FileExistsError:
                    continue
        os.chmod(backup_dir, 0o700)
        backup = backup_dir / f"BroScribe-{installed.version}-{installed.build}-{stamp}.app"
        self.copy(installed.path, backup)
        self.check_sealed(backup, installed_requirement)

        parent = installed.path.parent
        staging = parent / f".BroScribe.app.update-{stamp}"
        previous = parent / f".BroScribe.app.previous-{stamp}"
        self.copy(new.path, staging)
        try:
            self.check_sealed(staging, installed_requirement)
        except UpdateError:
            shutil.rmtree(staging, ignore_errors=True)
            raise

        admission: Admission | None = None
        unregister_attempted = stopped = moved = swapped = False
        observed: set[int] = set()
        startup = StartupReservation(self.system, baseline.data_dir, self.timeout)
        app = AppReservation(baseline.data_dir)
        try:
            if agent:
                # From here no capture starts until the admission is released.
                admission = self.acquire(installed)
            startup.acquire()
            app.acquire()
            self.check_gui_closed(installed.path)
            if admission:
                # A failed CLI response can arrive after launchd unregistered.
                unregister_attempted = True
                self.unregister_agent(installed.path, admission.fence)
                self.wait_for_agent_exit(installed.path, baseline.data_dir)
                stopped = True
            elif not self.agent_absent(installed.path, baseline.data_dir):
                raise UpdateError("the profile's agent is now registered or holds its instance claim; run the update again")
            self.check_sealed(installed.path, installed_requirement)
            installed.path.rename(previous)
            # From here the original app is no longer at its path: every
            # failure must put it back, even before the new one is in place.
            moved = True
            staging.rename(installed.path)
            swapped = True
            new.path = installed.path
            app.release()
            startup.release()
            if admission:
                self.start_agent(installed.path)
                self.prove(new, admission.fence, {admission.pid}, observed)
            after = self.doctor(installed.path)
            if after.data_dir != baseline.data_dir:
                raise UpdateError("the updated CLI resolved a different profile data directory")
            regressions = baseline.regressions(after)
            if regressions:
                raise UpdateError("`broscribe doctor` regressed after the update: " + "; ".join(regressions))
        except (UpdateError, OSError) as error:
            if not moved and not unregister_attempted and admission is None:
                shutil.rmtree(staging, ignore_errors=True)
                raise
            self.system.log(f"{error}; rolling back")
            try:
                self.rollback(installed, previous, staging, backup, swapped, admission, unregister_attempted,
                              stopped, observed, baseline.data_dir, startup, app, installed_requirement)
            except (UpdateError, OSError) as failure:
                held = f"; {self.still_held(admission.fence)}" if admission else ""
                raise UpdateError(f"update failed ({error}) and the rollback failed ({failure}); the previous app "
                                  f"is at {previous if previous.exists() else backup}{held}") from None
            raise UpdateError(f"update failed and was rolled back: {error}") from None
        finally:
            app.release()
            startup.release()
        if admission:
            try:
                self.release_after_proof(new, admission, {admission.pid})
            except (UpdateError, OSError) as error:
                raise UpdateError(f"updated to {new.describe()} but {error}; {self.still_held(admission.fence)}; "
                                  f"the previous app is at {previous}") from None
        self.check_sealed(previous, installed_requirement)
        shutil.rmtree(previous, ignore_errors=True)
        state = "doctor passes" if after.ok else "doctor reports the same problems as before"
        self.system.log(f"updated to {new.describe()}; {state}; backup kept at {backup}")
        return 0

    def rollback(self, installed: Bundle, previous: Path, staging: Path, backup: Path, swapped: bool,
                 admission: Admission | None, unregister_attempted: bool, stopped: bool, observed: set[int],
                 data_dir: Path, startup: StartupReservation, app: AppReservation, installed_requirement: str) -> None:
        restart = False
        if swapped or unregister_attempted or previous.exists():
            startup.acquire()
        if previous.exists():
            self.check_sealed(previous, installed_requirement)
        if swapped:
            self.check_sealed(installed.path, installed_requirement)
            if admission:
                state = self.daemon_state(installed.path)
                if state.get("running") is False and state.get("installed") is True:
                    # The CLI takes its own startup reservation, checks the
                    # persisted fence, and proves the registration is ours and idle.
                    startup.release()
                    self.cli_json(installed.path, "unregistering the idle agent",
                                  "daemon", "uninstall", "--admission-fence-if-idle", str(admission.fence))
                    startup.acquire()
                elif not self.agent_absent(installed.path, data_dir):
                    self.unregister_agent(installed.path, admission.fence)
                self.wait_for_agent_exit(installed.path, data_dir)
            elif not self.agent_absent(installed.path, data_dir):
                raise UpdateError("the profile's agent is registered or holds its instance claim during rollback")
            self.quit_rejected_gui(installed.path)
            app.acquire()
            self.check_gui_closed(installed.path)
            restart = admission is not None
            failed = installed.path.parent / previous.name.replace("previous", "failed")
            self.check_sealed(installed.path, installed_requirement)
            installed.path.rename(failed)
            shutil.rmtree(failed, ignore_errors=True)
        shutil.rmtree(staging, ignore_errors=True)
        if previous.exists():
            self.check_sealed(previous, installed_requirement)
            previous.rename(installed.path)
        elif not installed.path.exists():
            self.check_sealed(backup, installed_requirement)
            self.copy(backup, installed.path)
        if admission is not None and unregister_attempted and not swapped:
            state = self.daemon_state(installed.path)
            if state.get("installed") is False:
                self.wait_for_agent_exit(installed.path, data_dir)
                stopped = True
                restart = True
            elif state.get("running") is False:
                restart = True
        # Neither relaunch nor any proof may run while either lock is held.
        app.release()
        startup.release()
        if admission is None:
            return
        if restart:
            self.start_agent(installed.path)
        # The restored agent must prove it runs the previous build and still
        # holds the admission (as a new process once the old one exited)
        # before anything may record again.
        not_pids = set(observed) - {admission.pid}
        if stopped:
            not_pids.add(admission.pid)
        self.release_after_proof(installed, admission, not_pids)


def main(argv: list[str] | None = None, system: System | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--new", type=Path, required=True)
    parser.add_argument("--installed", type=Path, default=Path("/Applications/BroScribe.app"))
    parser.add_argument("--backup-dir", type=Path,
                        default=Path.home() / "Library/Application Support/BroScribe/app-backups")
    parser.add_argument("--allow-same-build", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    if system is None:
        if sys.platform != "darwin":
            print("update-macos: macOS only", file=sys.stderr)
            return 1
        system = System()
    try:
        return Updater(system, args.new, args.installed, args.backup_dir,
                       args.allow_same_build, args.dry_run).run()
    except (UpdateError, OSError) as error:
        print(f"update-macos: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

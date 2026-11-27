"""Offline archive, privacy, relocation and provenance regressions."""

from copy import deepcopy
from pathlib import Path, PureWindowsPath
import json
import os
import shlex
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import patch
import zipfile

from support import REPO, SCRIPTS, load_script

from desktop_artifacts import (
    BINARIES, DESKTOP, ICONS, LICENCES, TARGETS, build_root_spellings, build_roots, builder_paths,
    forbidden_paths,
    json_bytes, json_object, release_files,
)

package = load_script("package-desktop.py")
verify = load_script("verify-desktop-artifact.py")
preview = load_script("package-macos-preview.py")
LINUX = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu")
WINDOWS = "x86_64-pc-windows-msvc"


def elf(machine, runpath=b"$ORIGIN", dependency=b"libc.so.6"):
    strings_at = 512
    strings = b"\0" + runpath + b"\0" + dependency + b"\0"
    header = bytearray(strings_at)
    header[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<HH", header, 16, 3, machine)
    struct.pack_into("<Q", header, 32, 64)
    struct.pack_into("<HH", header, 54, 56, 2)
    struct.pack_into("<IIQQQQQQ", header, 64, 1, 5, 0, 0, 0, strings_at + len(strings), 0, 4096)
    struct.pack_into("<IIQQQQQQ", header, 120, 2, 6, 192, 192, 0, 80, 0, 8)
    for offset, (tag, value) in enumerate(((5, strings_at), (10, len(strings)), (29, 1), (1, len(runpath) + 2), (0, 0))):
        struct.pack_into("<qQ", header, 192 + offset * 16, tag, value)
    return bytes(header) + strings


def pe(machine=0x8664):
    header = bytearray(128)
    header[:2] = b"MZ"
    struct.pack_into("<I", header, 60, 64)
    struct.pack_into("<4sHHIIIHHH", header, 64, b"PE\0\0", machine, 1, 0, 0, 0, 2, 2, 0x20B)
    return bytes(header)


class DesktopArtifacts(unittest.TestCase):
    @unittest.skipUnless(os.name == "posix", "requires a POSIX shell")
    def test_linux_uninstaller_uses_only_the_packaged_regular_cli(self):
        with tempfile.TemporaryDirectory() as home:
            prefix = Path(home) / ".local/lib/broscribe"
            stray = prefix / "bin/broscribe"
            stray.parent.mkdir(parents=True)
            stray.write_text("#!/bin/sh\nprintf 'stray CLI'\n")
            stray.chmod(0o755)
            cli = prefix / "broscribe"

            def run():
                return subprocess.run(
                    ["/bin/sh", REPO / "packaging/linux/uninstall.sh", "--yes"],
                    env={**os.environ, "HOME": home},
                    capture_output=True, text=True, check=False,
                )

            for kind in ("missing", "directory", "link", "non-executable"):
                with self.subTest(kind=kind):
                    if kind == "directory":
                        cli.mkdir()
                    elif kind == "link":
                        cli.symlink_to(stray)
                    elif kind == "non-executable":
                        cli.write_text("#!/bin/sh\nexit 0\n")
                        cli.chmod(0o600)
                    result = run()
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(result.stdout, "")
                    self.assertIn(f"Expected a regular executable at {cli}.", result.stderr)
                    if kind == "directory":
                        cli.rmdir()
                    else:
                        cli.unlink(missing_ok=True)

            cli.write_text('#!/bin/sh\nprintf "%s" "$*"\n')
            cli.chmod(0o755)
            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "uninstall --yes")

    def test_package_layout_is_in_the_compiled_release_history(self):
        path = REPO / "crates/broscribe-platform/src/release_layouts.json"
        layouts = json.loads(path.read_bytes())
        for platform in ("linux", "windows"):
            history = [set(layout) for layout in layouts[platform]]
            self.assertIn(release_files(platform == "linux"), history)
        self.assertIn(release_files(True) - {"uninstall.sh"}, [set(layout) for layout in layouts["linux"]])

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.stage = self.root / "build"
        self.stage.mkdir()
        self.work = self.root / "work"
        self.work.mkdir()
        self.dist = self.root / "dist"
        self.dist.mkdir()

    def fixture(self, target):
        linux = target in LINUX
        for name in BINARIES:
            executable = elf(TARGETS[target][2]) if linux else pe()
            (self.stage / (name if linux else name + ".exe")).write_bytes(executable)
        (self.stage / "THIRD_PARTY_NOTICES.txt").write_text(
            f"BroScribe third-party notices\nTarget: {target}\n\nLicence and notice texts\nSynthetic notice.\n")
        (self.stage / "third-party-inventory.json").write_bytes(json_bytes({
            "schema": 1, "target": target, "complete": True, "unresolved": [],
            **{key: [{"status": "ok"}] for key in ("packages", "native", "embedded")},
        }))
        build = {
            "schema": 1, "product": "BroScribe", "version": "1.2.3",
            "build": 42, "target": target, "profile": "release",
            "git": {"commit": "a" * 40, "dirty": False},
            "toolchain": {"rustc": "rustc 1.100.0", "cargo": "cargo 1.100.0"},
            "sherpa_onnx": {"archive": "synthetic.tar.bz2", "sha256": "c" * 64},
            "cargo_lock_sha256": "b" * 64,
        }
        return build, package.payload(self.stage, self.stage, build, linux)

    def emit(self, build, files, forbid=()):
        return package.write_release(self.dist, self.work, build, files, list(forbid))

    def test_combined_manifest_checks_all_artifacts_and_omits_ad_hoc_macos(self):
        from test_release_metadata import NOW, SIGNING, make_bundle, zip_app

        metadata = load_script("release-metadata.py")
        packages = self.root / "packages"
        packages.mkdir()
        desktop = None
        for target in (*LINUX, WINDOWS):
            build, files = self.fixture(target)
            label = TARGETS[target][1]
            folder = packages / (label if target != WINDOWS else label + "-unsigned")
            folder.mkdir()
            archive = package.write_release(folder, self.work, build, files, [])
            desktop = json_object(archive.with_name(
                archive.name.removesuffix(".zip").removesuffix(".tar.gz") + ".provenance.json"
            ).read_bytes())
        folder = packages / "macos-arm64-notarized"
        folder.mkdir()
        app = make_bundle(self.root, build=42, version="1.2.3")
        info = app / "Contents/Resources/build-info.json"
        build = json_object(info.read_bytes())
        info.write_bytes(json_bytes(build | {name: desktop[name] for name in ("git", "cargo_lock_sha256")}))
        zip_path = zip_app(app, folder / "BroScribe-1.2.3-42-macos-arm64.zip")
        record = metadata.provenance(zip_path, app, SIGNING | {"team_identifier": metadata.MACOS_TEAM},
                                     "synthetic-id", "Accepted", folder)
        prov_path = zip_path.with_suffix(".provenance.json")
        prov_path.write_bytes(json_bytes(record))
        mac_sidecar = zip_path.with_name(zip_path.name + ".sha256")
        mac_sidecar.write_text(f'{record["artifact"]["sha256"]}  {zip_path.name}\n')
        disk_image = zip_path.with_suffix(".dmg")
        disk_image.write_bytes(b"synthetic disk image")
        disk_image_sidecar = disk_image.with_name(disk_image.name + ".sha256")
        disk_image_sidecar.write_text(f"{metadata.sha256_file(disk_image)}  {disk_image.name}\n")
        envelope = metadata.combined_manifest(packages, "v1.2.3", NOW)
        self.assertEqual([entry["archive"] for entry in envelope["artifacts"]], ["zip", "tar.gz", "tar.gz", "zip"])
        # The disk image is for first installs only: the updater installs the ZIP.
        self.assertFalse(any(".dmg" in entry["url"] for entry in envelope["artifacts"]))
        self.assertEqual(["BroScribe-1.2.3-42-macos-arm64.zip"],
                         [entry["url"].rsplit("/", 1)[1] for entry in envelope["artifacts"]
                          if entry["target"] == "aarch64-apple-darwin"])
        self.assertEqual([entry["app_id"] for entry in envelope["artifacts"]],
                         ["org.broscribe.BroScribe", "BroScribe", "BroScribe", "BroScribe"])
        self.assertTrue(all(entry["url"].startswith(metadata.RELEASE_ASSETS + "/v1.2.3/")
                            for entry in envelope["artifacts"]))
        self.assertEqual(envelope["published_at"], "2026-11-17T10:00:00Z")
        self.assertEqual(envelope["expires_at"], "2027-02-15T10:00:00Z")
        refresh = load_script("refresh-update.py")
        from datetime import timedelta
        downloads = self.root / "downloaded"
        downloads.mkdir()
        assets = [path for path in packages.glob("*/*")]
        for path in assets:
            shutil.copyfile(path, downloads / path.name)
        previous = downloads / refresh.ENVELOPE
        previous.write_bytes(b"synthetic authenticated baseline")
        release = {
            "tag_name": "v1.2.3", "draft": False, "prerelease": False,
            "published_at": "2026-11-17T10:00:00Z",
            "assets": [{"name": path.name} for path in downloads.iterdir()],
        }
        for invalid in (
            release | {"draft": True}, release | {"prerelease": True},
            release | {"assets": release["assets"][:-2]},
            release | {"assets": [*release["assets"], {"name": "unexpected.zip"}]},
            release | {"assets": [asset for asset in release["assets"] if asset["name"] != disk_image.name]},
            release | {"assets": [asset for asset in release["assets"]
                                  if asset["name"] != disk_image_sidecar.name]},
        ):
            with self.assertRaises(ValueError):
                refresh.release_assets(invalid)
        bad_downloads = self.root / "bad-downloads"
        shutil.copytree(downloads, bad_downloads)
        (bad_downloads / mac_sidecar.name).write_text(f'{"0" * 64}  {zip_path.name}\n')
        with self.assertRaisesRegex(refresh.metadata.MetadataError, r"\.sha256 does not match"):
            refresh.refresh_manifest(release, bad_downloads, self.root / "bad-refresh", NOW)
        fresh = refresh.refresh_manifest(release, downloads, self.root / "refresh", NOW + timedelta(days=100))
        self.assertNotEqual(fresh["published_at"], envelope["published_at"])
        self.assertNotEqual(fresh["expires_at"], envelope["expires_at"])
        refresh.check_previous(envelope, fresh)
        changed = deepcopy(fresh)
        changed["artifacts"][1]["sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "different release artifacts"):
            refresh.check_previous(envelope, changed)
        with self.assertRaisesRegex(metadata.MetadataError, "different team"):
            prov_path.write_bytes(json_bytes(record | {"signing": SIGNING}))
            metadata.combined_manifest(packages, "v1.2.3", NOW)
        adhoc = folder.with_name("macos-arm64-adhoc")
        prov_path.unlink()
        adhoc_zip = zip_path.with_stem(zip_path.stem + "-adhoc")
        zip_path.rename(adhoc_zip)
        mac_sidecar.unlink()
        disk_image.unlink()
        disk_image_sidecar.unlink()
        adhoc_sidecar = adhoc_zip.with_name(adhoc_zip.name + ".sha256")
        adhoc_sidecar.write_text(f'{record["artifact"]["sha256"]}  {adhoc_zip.name}\n')
        zip_path.with_name(zip_path.stem + "-adhoc.provenance.json").write_bytes(json_bytes(
            record | {"signing": {"kind": "ad-hoc"}, "notarization": {"stapled": False}}
        ))
        folder.rename(adhoc)
        adhoc_downloads = self.root / "adhoc-downloads"
        adhoc_downloads.mkdir()
        for path in packages.glob("*/*"):
            shutil.copyfile(path, adhoc_downloads / path.name)
        (adhoc_downloads / adhoc_sidecar.name).write_text(f'{"0" * 64}  {adhoc_zip.name}\n')
        adhoc_release = release | {"assets": [{"name": path.name} for path in adhoc_downloads.iterdir()]}
        with self.assertRaisesRegex(ValueError, "never has a disk image"):
            refresh.release_assets(adhoc_release | {"assets": [
                *adhoc_release["assets"], {"name": disk_image.name}, {"name": disk_image_sidecar.name}]})
        with self.assertRaisesRegex(refresh.metadata.MetadataError, r"\.sha256 does not match"):
            refresh.refresh_manifest(adhoc_release, adhoc_downloads, self.root / "bad-adhoc-refresh", NOW)
        manifest = metadata.combined_manifest(packages, "v1.2.3", NOW)
        self.assertEqual(len(manifest["artifacts"]), 3)
        self.assertFalse(any(entry["target"] == "aarch64-apple-darwin" for entry in manifest["artifacts"]))
        with archive.open("ab") as out:
            out.write(b"bad")
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            metadata.combined_manifest(packages, "v1.2.3", NOW)

    def test_all_targets_deterministic_and_verifiable(self):
        for target in (*LINUX, WINDOWS):
            with self.subTest(target=target):
                build, files = self.fixture(target)
                artifact = self.emit(build, files)
                initial = artifact.read_bytes()
                for source, _ in files.values():
                    if isinstance(source, Path) and source.parent == self.stage:
                        os.utime(source, (123, 123))
                self.assertEqual(initial, self.emit(build, dict(reversed(files.items()))).read_bytes())
                result = subprocess.run([sys.executable, str(SCRIPTS / "verify-desktop-artifact.py"), str(artifact)],
                                        capture_output=True, text=True)
                self.assertEqual(0, result.returncode, result.stderr)
                if target in LINUX:
                    with tarfile.open(artifact) as archive:
                        self.assertEqual(0o755, archive.getmember("BroScribe/install.sh").mode)
                        self.assertEqual(0o755, archive.getmember("BroScribe/uninstall.sh").mode)
                        self.assertEqual(0, archive.getmember("BroScribe/broscribe").uid)
                        self.assertEqual(0, archive.getmember("BroScribe/broscribe").mtime)
                        self.assertIsNotNone(archive.getmember("BroScribe/" + DESKTOP))
                        self.assertEqual({22, 32, 64, 128}, set(ICONS.values()))
                        self.assertEqual({"BroScribe/" + name for name in ICONS},
                                         {name for name in archive.getnames() if "/hicolor/" in name})
                else:
                    with zipfile.ZipFile(artifact) as archive:
                        self.assertEqual((1980, 1, 1, 0, 0, 0), archive.getinfo("BroScribe/broscribe.exe").date_time)
                        self.assertTrue(all(not name.endswith(".lnk") for name in archive.namelist()))

    def test_wrong_architecture_rpath_and_unshipped_linux_libraries_fail(self):
        build, files = self.fixture(LINUX[1])
        for data, message in (
            (elf(62), "wrong ELF format or machine"),
            (elf(183, b"/usr/local/lib"), "RPATH/RUNPATH"),
            (elf(183, dependency=b"libonnxruntime.so"), "unshipped sherpa"),
        ):
            with self.subTest(message=message), self.assertRaisesRegex(ValueError, message):
                self.emit(build, {**files, "broscribe": (data, 0o755)})
        build, files = self.fixture(WINDOWS)
        with self.assertRaisesRegex(ValueError, "Windows x64 PE32"):
            self.emit(build, {**files, "broscribed.exe": (pe(0xaa64), 0o755)})

    def test_upstream_paths_in_native_members_are_target_specific(self):
        cases = (
            (LINUX[0], b"/home/runner/work/sherpa-onnx/sherpa-onnx/csrc/log.cc",
             b"/home/runner/work/broscribe/broscribe/crates/own.cc"),
            (LINUX[1], b"/k2-fsa/sherpa-onnx/csrc/log.cc", b"/opt/runner/scratch/own.cc"),
            (WINDOWS, r"D:\a\sherpa-onnx\sherpa-onnx\csrc\log.cc".encode("utf-16le"),
             rb"D:\a\broscribe\broscribe\crates\own.cc"),
        )
        for target, upstream, builder in cases:
            with self.subTest(target=target):
                build, files = self.fixture(target)
                windows = target == WINDOWS
                name, binary = ("broscribe.exe", pe()) if windows else ("broscribe", elf(TARGETS[target][2]))
                self.emit(build, {**files, name: (binary + upstream, 0o755)})
                with self.assertRaisesRegex(ValueError, "forbidden builder or home") as raised:
                    self.emit(build, {**files, name: (binary + b"\0".join((upstream, builder)), 0o755)},
                              forbid=["/opt/runner/scratch"])
                # The message names the match so a CI log shows what leaked.
                self.assertIn(builder.decode()[:12].lower(), str(raised.exception).lower())

        build, files = self.fixture(LINUX[1])
        other_target = cases[0][1]
        with self.assertRaisesRegex(ValueError, "forbidden builder or home"):
            self.emit(build, {**files, "broscribe": (elf(TARGETS[LINUX[1]][2]) + other_target, 0o755)})
        build, files = self.fixture(LINUX[0])
        with self.assertRaisesRegex(ValueError, "forbidden builder or home"):
            self.emit(build, {**files, "NOTICE": (cases[0][1], 0o644)})

    def test_dbus_object_paths_are_not_home_paths(self):
        # accesskit_unix's packed read-only strings: the root object, then
        # another D-Bus path, read as `.../root/org/...`.
        packed = b"\0/org/a11y/atspi/accessible/root/org/a11y/atspi/cache\0"
        self.assertIsNone(forbidden_paths([])(packed))
        explicit = forbidden_paths(["/org/a11y/atspi"])
        self.assertEqual(explicit(packed), b"/org/a11y/atspi")
        leaks = b"\0/root/.cargo/x.rs\0/home/dev/y.rs\0/root/.cargo/x.rs\0"
        self.assertEqual(forbidden_paths([])(leaks), b"/root/.cargo/, /home/dev/")

    def test_overlap_skips_only_home_and_keeps_actual_build_roots(self):
        roots = {
            "__build_home__": Path("/Users/runner"),
            "__build_cargo__": Path("/Users/runner/.cargo"),
            "__build_repo__": Path("/Users/runner/work/broscribe/broscribe"),
            "__build_target__": Path("/Users/runner/work/broscribe/broscribe/target"),
            "__build_temp__": Path("/Users/runner/work/_temp"),
        }
        self.assertCountEqual(builder_paths("osx-arm64", roots), [str(root) for root in list(roots.values())[1:]])
        self.assertCountEqual(builder_paths("linux-aarch64", roots), [str(root) for root in roots.values()])
        near = Path("/Users/run")
        self.assertEqual(builder_paths("osx-arm64", {"__build_home__": near}), [str(near)])

    def test_desktop_binary_scan_distinguishes_runtime_literals_from_builder_paths(self):
        scan = verify.forbidden_paths(())
        for literal in (
            "/users/me/calendarList",
            "{base}/users/me/calendarList?{}",
            "https://graph.microsoft.com/v1.0/users/{}/calendars",
        ):
            for encoding in ("utf-8", "utf-16le"):
                with self.subTest(literal=literal, encoding=encoding):
                    self.assertFalse(scan(literal.encode(encoding)))
        for path in (
            "/Users/runner/work/broscribe/src/main.rs",
            "/private/tmp/tmpabc/src/main.rs",
            "/private/var/tmp/tmpabc/src/main.rs",
            "/private/tmp/private/var/tmp/file",
            "/private/var/tmp/private/tmp/file",
            "/private/tmp/private/var/tmp-build/obj",
            "/private/tmp/private/var/tmp\0",
            "BundleVersion/private/tmp/private/var/tmpContents/Resourcesthe signing certificate",
            r"C:\users\Builder\private.pdb",
            "c:/users/Builder/private.pdb",
            r"d:\A\BROscribe\build.pdb",
            r"D:\\a\\build.pdb",
        ):
            for encoding in ("utf-8", "utf-16le"):
                with self.subTest(path=path, encoding=encoding):
                    self.assertTrue(scan(path.encode(encoding)))
        for path in ("/private/tmp/build.log", r"D:\a\build.pdb", "/Users/runner"):
            for suffix in ("", "\0", "\x01", "\x08", "\x0e", "\x1f", "\x7f", "'", '"', " ", "\t", "\n"):
                for encoding in ("utf-8", "utf-16le"):
                    with self.subTest(path=path, suffix=suffix, encoding=encoding):
                        self.assertTrue(scan((path + suffix).encode(encoding)))
        self.assertTrue(verify.forbidden_paths(("/private/tmp",))(b"/private/tmp/private/var/tmp\0"))

    def test_symlinked_builder_roots_are_remapped_and_explicitly_refused(self):
        real = Path(self.tmp.name).resolve() / "physical"
        real.mkdir()
        link = real.with_name("logical")
        try:
            link.symlink_to(real, target_is_directory=True)
        except OSError as error:
            if getattr(error, "winerror", None) == 1314:
                self.skipTest("Windows is not configured to create symlinks")
            raise
        given = dict(zip(
            ("__build_home__", "__build_cargo__", "__build_repo__", "__build_target__", "__build_temp__"),
            (link / name for name in ("home", "cargo", "repo", "target", "runner")),
        ))
        for path in given.values():
            path.mkdir()
        with patch.object(Path, "home", return_value=given["__build_home__"]), patch.dict(os.environ, {
            "PWD": str(link),
            "CARGO_HOME": str(given["__build_cargo__"]),
            "CARGO_TARGET_DIR": "target",
            "RUNNER_TEMP": str(given["__build_temp__"]),
        }), patch.object(Path, "cwd", return_value=real):
            roots = build_roots(repo_dir=given["__build_repo__"])
        self.assertEqual(roots, given)
        explicit = builder_paths("linux-aarch64", roots)
        for root in roots.values():
            self.assertIn(str(root), explicit)
            self.assertIn(str(root.resolve()), explicit)
        # The Linux builder runs only on Linux. On a Windows test host the
        # fixture still exercises expansion and verifier roots above.
        if os.name != "nt":
            with patch.object(package, "run", side_effect=self.read_rustflags) as run:
                package.build_native(LINUX[0], roots["__build_target__"].resolve(), roots, "/bin/bash")
            config = self.rustflags_config
            native = run.call_args.kwargs["env"][f"CFLAGS_{LINUX[0].replace('-', '_')}"]
            for label, root in roots.items():
                for path in (root, root.resolve()):
                    self.assertIn(f"--remap-path-prefix={path}=/{label}", config)
                    self.assertIn(f"-ffile-prefix-map={path}=/{label}", native)

    def read_rustflags(self, *command, **kwargs):
        config = Path(command[command.index("--config") + 1])
        self.assertTrue(config.is_absolute())
        self.rustflags_config = config.read_text(encoding="utf-8")

    def test_explicit_runner_root_overrides_a_native_path_exception(self):
        build, files = self.fixture(LINUX[0])
        root = "/home/runner/work/sherpa-onnx/sherpa-onnx/target"
        data = elf(TARGETS[LINUX[0]][2]) + f"{root}/own.cc".encode()
        with patch.dict(os.environ, {"RUNNER_TEMP": root}):
            with self.assertRaisesRegex(ValueError, "forbidden builder or home"):
                self.emit(build, {**files, "broscribe": (data, 0o755)})

    def test_msvc_build_sets_both_trim_spellings_and_preserves_user_flags(self):
        roots = {
            "__build_home__": PureWindowsPath(r"C:\Users\Builder"),
            "__build_repo__": PureWindowsPath(r"D:\a\broscribe\broscribe"),
            "__build_target__": PureWindowsPath(r"D:\a\_temp\bs"),
        }
        bash = r"C:\Program Files\Git\bin\bash.exe"
        self.msvc = self.root / "MSVC/bin/Hostx64/x64"
        self.msvc.mkdir(parents=True)
        for tool in ("link.exe", "cl.exe"):
            (self.msvc / tool).touch()
        cflags = "CFLAGS_x86_64_pc_windows_msvc"
        cxxflags = "CXXFLAGS_x86_64_pc_windows_msvc"
        expected = [f"/d1trimfile:{form}" for _, root in build_root_spellings(roots)
                    for form in (str(root), root.as_posix())]
        for shell_flags in ("1", "0"):
            with self.subTest(shell_flags=shell_flags):
                env = {cflags: "/DSYNTHETIC_C", cxxflags: "/DSYNTHETIC_CXX", "CC_SHELL_ESCAPED_FLAGS": shell_flags}
                with patch.dict(os.environ, env, clear=True), \
                        patch.object(package.shutil, "which", return_value=str(self.msvc / "link.exe")), \
                        patch.object(package, "run", side_effect=self.read_rustflags) as run:
                    package.build_native(WINDOWS, roots["__build_target__"], roots, bash)
                self.assertEqual(run.call_args.args[:2], (bash, "scripts/build-target.sh"))
                # Git Bash collapses escaped backslashes in a quoted argument,
                # so no argument may carry the TOML value inline.
                self.assertFalse(any("\\" in arg for arg in run.call_args.args[2:]))
                rustflags = tomllib.loads(self.rustflags_config)["target"][WINDOWS]["rustflags"]
                for _, root in build_root_spellings(roots):
                    self.assertIn(f"--remap-path-prefix={root}=/{_}", rustflags)
                self.assertEqual(rustflags[-1], "-Clink-arg=/PDBALTPATH:%_PDB%")
                linker = tomllib.loads(self.rustflags_config)["target"][WINDOWS]["linker"]
                self.assertEqual(linker, str(self.msvc / "link.exe"))
                forwarded = run.call_args.kwargs["env"]
                split = shlex.split if shell_flags == "1" else str.split
                self.assertEqual(split(forwarded[cflags]), ["/DSYNTHETIC_C", *expected])
                self.assertEqual(split(forwarded[cxxflags]), ["/DSYNTHETIC_CXX", *expected])
        # Git's coreutils link has no cl.exe beside it.
        coreutils = self.root / "Git/usr/bin/link.exe"
        with patch.object(package.shutil, "which", return_value=str(coreutils)), patch.object(package, "run") as run:
            with self.assertRaisesRegex(ValueError, "MSVC tools"):
                package.build_native(WINDOWS, roots["__build_target__"], roots, bash)
        run.assert_not_called()

    def test_git_bash_is_preferred_to_wsl_and_used_for_the_pin_check(self):
        wsl, git = self.root / "System32/bash.exe", self.root / "Git/cmd/git.exe"
        git_bash = git.parent.parent / "bin/bash.exe"
        git_bash.parent.mkdir(parents=True)
        git_bash.touch()
        with patch.object(package.shutil, "which", side_effect={"bash": str(wsl), "git": str(git)}.get) as which:
            resolved = package.find_bash(windows=True)
        self.assertEqual(resolved, str(git_bash.resolve()))
        self.assertEqual([call.args for call in which.call_args_list].count(("bash",)), 1)
        with patch.object(package, "output", return_value="archive.tar.bz2\nchecksum") as run:
            self.assertEqual(package.pinned_library(WINDOWS, resolved)[1:], ("archive.tar.bz2", "checksum"))
        self.assertEqual(run.call_args.args[:2], (resolved, "-ec"))
        with patch.object(package.shutil, "which", side_effect={"bash": str(wsl), "git": None}.get):
            with self.assertRaisesRegex(ValueError, "Git Bash is required"):
                package.find_bash(windows=True)
        with patch.object(package.shutil, "which", return_value=None):
            with self.assertRaisesRegex(ValueError, "bash is required"):
                package.find_bash(windows=False)

    def test_msvc_packaging_checks_final_imports(self):
        with patch.object(package, "output", return_value="    KERNEL32.dll\n    VCRUNTIME140.dll\n"):
            with self.assertRaisesRegex(ValueError, "unshipped runtime: VCRUNTIME140.dll"):
                package.check_windows_imports(self.stage)
        with patch.object(package, "output", return_value="    onnxruntime.dll\n"):
            with self.assertRaisesRegex(ValueError, "unshipped runtime: onnxruntime.dll"):
                package.check_windows_imports(self.stage)
        with patch.object(package, "output", return_value="    KERNEL32.dll\n") as command, patch("builtins.print"):
            package.check_windows_imports(self.stage)
        self.assertEqual([str(self.stage / (name + ".exe")) for name in BINARIES],
                         [call.args[2] for call in command.call_args_list])

    def test_privacy_and_structure_are_rejected_before_dist(self):
        build, files = self.fixture(LINUX[0])
        for name, data, mode, forbid, message in (
            ("install.sh", b"#!/bin/sh\n", 0o644, (), "executable bits"),
            ("uninstall.sh", b"#!/bin/sh\n", 0o644, (), "executable bits"),
            ("README.private.txt", b"dummy file", 0o644, (), "private archive entry"),
            ("../LICENSE", b"dummy file", 0o644, (), "unexpected archive file"),
            ("LICENSE", b"Apache License /home/buildperson/work", 0o644, (), "builder or home"),
            ("NOTICE", b"Copyright Example\n/opt/build/synthetic/path", 0o644, ("/opt/build/synthetic",), "builder or home"),
        ):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, message):
                self.emit(build, {**files, name: (data, mode)}, forbid)
        with self.assertRaisesRegex(ValueError, "missing archive files"):
            self.emit(build, {name: item for name, item in files.items() if name != "LICENSE"})
        build, files = self.fixture(WINDOWS)
        leaked = r"C:\Users\Builder\private.pdb".encode("utf-16le")
        with self.assertRaisesRegex(ValueError, "builder or home"):
            self.emit(build, {**files, "broscribe-app.exe": (pe() + leaked, 0o755)})
        self.assertEqual([], list(self.dist.iterdir()))

    def test_refresh_accepts_the_legacy_linux_layout_but_packaging_still_requires_uninstall(self):
        build, files = self.fixture(LINUX[0])
        files.pop("uninstall.sh")
        artifact = self.dist / "BroScribe-1.2.3-42-linux-x86_64.tar.gz"
        entries, installed = package.write_archive(artifact, files, linux=True)
        provenance = package.artifact_record(artifact, build, files["third-party-inventory.json"][0],
                                             entries, installed)
        artifact.with_name(artifact.name.removesuffix(".tar.gz") + ".provenance.json").write_bytes(
            json_bytes(provenance))
        artifact.with_name(artifact.name + ".sha256").write_text(
            f"{provenance['artifact']['sha256']}  {artifact.name}\n")
        verify.verify(artifact, allow_legacy_layout=True)
        with self.assertRaisesRegex(ValueError, "missing archive files: uninstall.sh"):
            verify.verify(artifact)

    def test_refresh_checks_executable_bits_on_historical_resources(self):
        _, files = self.fixture(LINUX[0])
        historical = dict(files)
        historical.pop("uninstall.sh")
        resource = "historical-resource.txt"
        historical[resource] = (b"legacy resource", 0o644)
        layouts = self.root / "crates/broscribe-platform/src"
        layouts.mkdir(parents=True)
        (layouts / "release_layouts.json").write_text(json.dumps({"linux": [sorted(historical)]}))
        artifact = self.dist / "BroScribe-1.2.3-42-linux-x86_64.tar.gz"
        with patch.object(verify, "__file__", str(self.root / "scripts/verify-desktop-artifact.py")):
            package.write_archive(artifact, historical, linux=True)
            verify.inspect(artifact, "linux-x86_64", LINUX[0], TARGETS[LINUX[0]][2], [],
                           allow_legacy_layout=True)
            historical[resource] = (b"legacy resource", 0o755)
            package.write_archive(artifact, historical, linux=True)
            with self.assertRaisesRegex(ValueError, resource + " must be a non-executable resource"):
                verify.inspect(artifact, "linux-x86_64", LINUX[0], TARGETS[LINUX[0]][2], [],
                               allow_legacy_layout=True)

    def test_refresh_requires_one_exact_layout_from_the_history(self):
        _, files = self.fixture(WINDOWS)
        history = json.loads((REPO / "crates/broscribe-platform/src/release_layouts.json").read_text())
        first = set(history["windows"][0])
        history["windows"] = [sorted(first | {"historical.txt"}), sorted(first | {"new.txt"})]
        layouts = self.root / "crates/broscribe-platform/src"
        layouts.mkdir(parents=True)
        (layouts / "release_layouts.json").write_text(json.dumps(history))
        artifact = self.dist / "BroScribe-1.2.3-42-windows-x64.zip"
        with patch.object(verify, "__file__", str(self.root / "scripts/verify-desktop-artifact.py")):
            historical = files | {"historical.txt": (b"historic resource", 0o644)}
            package.write_archive(artifact, historical, linux=False)
            verify.inspect(artifact, "windows-x64", WINDOWS, 0x8664, [], allow_legacy_layout=True)
            mixed = historical | {"new.txt": (b"future resource", 0o644)}
            package.write_archive(artifact, mixed, linux=False)
            with self.assertRaisesRegex(ValueError, "does not match a known release layout"):
                verify.inspect(artifact, "windows-x64", WINDOWS, 0x8664, [], allow_legacy_layout=True)

    def test_complete_inventory_must_agree_with_notices(self):
        build, files = self.fixture(WINDOWS)
        inventory, mode = files["third-party-inventory.json"]
        for changes, message in (({"complete": False}, "inventory is incomplete"),
                                 ({"target": LINUX[0]}, "inventory is incomplete"),
                                 ({"native": [{"status": "missing-text"}]}, "incomplete native")):
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, message):
                altered = {**json_object(inventory), **changes}
                self.emit(build, {**files, "third-party-inventory.json": (json_bytes(altered), mode)})
        notices, mode = files["THIRD_PARTY_NOTICES.txt"]
        with self.assertRaisesRegex(ValueError, "incomplete third-party notices"):
            self.emit(build, {**files, "THIRD_PARTY_NOTICES.txt": (notices + b"\nINCOMPLETE: example\n", mode)})

    def test_tampering_with_checksum_provenance_and_build_number(self):
        build, files = self.fixture(LINUX[0])
        artifact = self.emit(build, files)
        checksum = artifact.with_name(artifact.name + ".sha256")
        original = checksum.read_bytes()
        checksum.write_text("0" * 64 + f"  {artifact.name}\n")
        with self.assertRaisesRegex(ValueError, "SHA-256 sidecar"):
            verify.verify(artifact)
        checksum.write_bytes(original)
        provenance = artifact.with_name(artifact.name.removesuffix(".tar.gz") + ".provenance.json")
        record = json_object(provenance.read_bytes())
        provenance.write_bytes(json_bytes({**record, "bundle_identifier": "example.test"}))
        with self.assertRaisesRegex(ValueError, "provenance disagrees"):
            verify.verify(artifact)
        provenance.write_bytes(json_bytes(record))
        later = {**build, "git": {"commit": "d" * 40, "dirty": False}}
        with self.assertRaisesRegex(ValueError, "build number must increase"):
            self.emit(later, files)
        invalid = {**build, "build": 0}
        with self.assertRaisesRegex(ValueError, "invalid release archive name"):
            self.emit(invalid, files)

    def test_macos_preview_provenance_never_claims_notarization(self):
        build, _ = self.fixture(LINUX[0])
        build["target"] = "aarch64-apple-darwin"
        resources = self.root / "BroScribe.app/Contents/Resources"
        resources.mkdir(parents=True)
        (resources / "build-info.json").write_bytes(json_bytes(build))
        (resources / "third-party-inventory.json").write_bytes(b"synthetic inventory")

        def native_command(command, **kwargs):
            if command[0] == "codesign":
                return subprocess.CompletedProcess(command, 0, "", "Signature=adhoc\n")
            if command[0] == "ditto":
                with zipfile.ZipFile(command[-1], "w") as archive:
                    archive.writestr("BroScribe.app/example", "synthetic")
                return subprocess.CompletedProcess(command, 0)
            self.assertIn("--codesign", command)
            self.assertIn("--require-complete-licences", command)
            return subprocess.CompletedProcess(command, 0)

        with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(self.root)}):
            with patch.object(preview.subprocess, "run", side_effect=native_command):
                preview.main()
            with patch.object(preview.subprocess, "run", return_value=subprocess.CompletedProcess(
                    ["codesign"], 0, "", "Authority=Developer ID Application: Synthetic\n")):
                with self.assertRaisesRegex(ValueError, "clean ad-hoc build"):
                    preview.main()
        stem = "BroScribe-1.2.3-42-macos-arm64-adhoc"
        record = json_object((self.dist / (stem + ".provenance.json")).read_bytes())
        self.assertEqual({"kind": "ad-hoc"}, record["signing"])
        self.assertEqual({"status": "Not submitted", "stapled": False}, record["notarization"])
        self.assertTrue((self.dist / (stem + ".zip.sha256")).is_file())

    @unittest.skipUnless(sys.platform == "linux" or shutil.which("gln"), "requires GNU ln")
    def test_linux_installer_uses_sibling_layout_and_xdg_links(self):
        build, files = self.fixture(LINUX[0])
        artifact = self.emit(build, files)
        extract = self.root / "stage"
        extract.mkdir()
        with tarfile.open(artifact) as archive:
            archive.extractall(extract, filter="data")
        home = self.root / 'home with $cash and %f'
        data = home / "XDG data"
        tools = self.root / "tools"
        tools.mkdir()
        if sys.platform != "linux":
            (tools / "ln").symlink_to(shutil.which("gln"))
        # This fixture can run in a root-owned CI container without installing
        # into the runner's home. A synthetic id and HOME affect the child only.
        user = tools / "id"
        user.write_text("#!/bin/sh\necho 1234\n")
        user.chmod(0o755)
        env = {**os.environ, "HOME": str(home), "XDG_DATA_HOME": str(data),
               "PATH": str(tools) + os.pathsep + os.environ["PATH"]}
        command = ["sh", str(extract / "BroScribe/install.sh")]
        result = subprocess.run(command, env=env, capture_output=True, text=True)
        self.assertEqual(0, result.returncode, result.stderr)
        prefix = home / ".local/lib/broscribe"
        self.assertTrue(all((prefix / name).is_file() for name in BINARIES))
        menu = data / DESKTOP.removeprefix("share/")
        self.assertFalse(menu.is_symlink())
        launcher = menu.read_text()
        self.assertIn(r'home with \\$cash and %%f', launcher)
        cli = home / ".local/bin/broscribe"
        self.assertEqual(prefix / "broscribe", cli.readlink())
        source = extract / "BroScribe"
        for path in source.rglob("*"):
            if path.is_file():
                self.assertEqual(path.read_bytes(), (prefix / path.relative_to(source)).read_bytes())
        prefix.rename(prefix.with_name("previous"))
        shutil.copytree(source, prefix)
        self.assertEqual(launcher, menu.read_text())
        self.assertTrue(cli.is_file())
        for name in ICONS:
            self.assertEqual(prefix / name, (data / name.removeprefix("share/")).readlink())
        self.assertEqual({"22x22", "32x32", "64x64", "128x128"},
                         {path.name for path in (data / "icons/hicolor").iterdir()})
        result = subprocess.run(command, env=env, capture_output=True, text=True)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("Already exists", result.stderr)
        self.assertTrue((prefix / "build-info.json").is_file())

        obsolete = subprocess.run([*command, "--upgrade"], env=env, capture_output=True, text=True)
        self.assertEqual(2, obsolete.returncode)
        self.assertIn("Usage: install.sh", obsolete.stderr)

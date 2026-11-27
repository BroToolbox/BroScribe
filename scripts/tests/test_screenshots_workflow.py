"""Validate the screenshots workflow and its manifest helper offline."""

import hashlib
import importlib.util
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import textwrap
import unittest
import zlib
from pathlib import Path

from support import REPO

HARNESS = REPO / "scripts/linux-desktop"
spec = importlib.util.spec_from_file_location("linux_desktop_shots", HARNESS / "shots.py")
shots = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = shots
spec.loader.exec_module(shots)


def chunk(kind: bytes, body: bytes) -> bytes:
    return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    return a if pa <= pb and pa <= pc else (b if pb <= pc else c)


def encode(width, height, pixels, channels=3, filters=(0,), extra=(), idat=None):
    """A PNG of `pixels` (rows of bytes), cycling through the given filters.

    `extra` chunks go between IHDR and IDAT; `idat` replaces the compressed
    image data."""
    stride = width * channels
    previous = bytes(stride)
    raw = bytearray()
    if filters == (0,):
        raw = b"".join(b"\0" + pixels[y * stride:(y + 1) * stride] for y in range(height))
        height = 0
    for y in range(height):
        row = pixels[y * stride:(y + 1) * stride]
        kind = filters[y % len(filters)]
        out = bytearray()
        for i, value in enumerate(row):
            left = row[i - channels] if i >= channels else 0
            corner = previous[i - channels] if i >= channels else 0
            predictor = (0, left, previous[i], (left + previous[i]) >> 1, paeth(left, previous[i], corner))[kind]
            out.append((value - predictor) & 0xFF)
        raw += bytes([kind]) + out
        previous = row
    header = struct.pack(">IIBBBBB", width, len(pixels) // stride, 8, 6 if channels == 4 else 2, 0, 0, 0)
    return (shots.PNG_SIGNATURE + chunk(b"IHDR", header) + b"".join(extra)
            + chunk(b"IDAT", zlib.compress(bytes(raw)) if idat is None else idat) + chunk(b"IEND", b""))


def solid(value: int, width=shots.WIDTH, height=shots.HEIGHT) -> bytes:
    return encode(width, height, bytes([value, value // 2, 255 - value]) * (width * height))


COMMIT = "a" * 40


def write_artifact(folder: Path, commit=COMMIT, shade=10):
    folder.mkdir(parents=True, exist_ok=True)
    for index, name in enumerate(shots.IMAGES):
        (folder / name).write_bytes(solid(shade + index))
    shots.write_manifest(folder, commit)


class Decoder(unittest.TestCase):
    def test_every_filter_decodes_to_the_original_pixels(self):
        width, height = 7, 10
        for channels in (3, 4):
            pixels = bytes((x * 37 + y * 11 + c * 5) & 0xFF
                           for y in range(height) for x in range(width) for c in range(channels))
            data = encode(width, height, pixels, channels, filters=(0, 1, 2, 3, 4))
            _, _, rgba = shots.rgba(data, None)
            if channels == 3:
                expected = b"".join(pixels[i:i + 3] + b"\xff" for i in range(0, len(pixels), 3))
            else:
                expected = pixels
            self.assertEqual(expected, rgba)

    def test_rgb_and_rgba_of_the_same_picture_hash_alike(self):
        rgb = bytes([1, 2, 3]) * 12
        rgba = bytes([1, 2, 3, 255]) * 12
        self.assertEqual(shots.describe(encode(4, 3, rgb), None)["pixels_sha256"],
                         shots.describe(encode(4, 3, rgba, 4), None)["pixels_sha256"])

    def test_compression_does_not_change_the_pixel_hash(self):
        pixels = bytes(range(256)) * 3 * 4
        plain, filtered = encode(32, 32, pixels), encode(32, 32, pixels, filters=(4, 1))
        self.assertNotEqual(plain, filtered)
        self.assertEqual(shots.describe(plain, None)["pixels_sha256"],
                         shots.describe(filtered, None)["pixels_sha256"])

    def test_rejects_corrupt_or_unexpected_files(self):
        good = encode(2, 2, bytes(12))
        broken = bytearray(good)
        broken[-20] ^= 1
        def with_header(depth, colour, interlace=0):
            body = struct.pack(">IIBBBBB", 2, 2, depth, colour, 0, 0, interlace)
            return shots.PNG_SIGNATURE + chunk(b"IHDR", body) + good[33:]

        cases = {
            "not a PNG": b"GIF89a" + good,
            "bad CRC": bytes(broken),
            "truncated": good[:-6],
            "trailing data": good + b"x",
            "16-bit": with_header(16, 2),
            "palette": with_header(8, 3),
            "interlaced": with_header(8, 2, 1),
            "short image data": encode(2, 2, bytes(12))[:33] + chunk(b"IDAT", zlib.compress(b"\0" * 5))
            + chunk(b"IEND", b""),
            "non-empty IEND": good[:-12] + chunk(b"IEND", b"x"),
        }
        for label, data in cases.items():
            with self.subTest(label), self.assertRaises(shots.Invalid):
                shots.describe(data, None)

    def test_only_the_chunks_the_capture_writes_are_accepted(self):
        for kind in (b"tEXt", b"tIME", b"iTXt", b"zTXt", b"eXIf", b"tRNS", b"gAMA", b"sRGB", b"PLTE",
                     b"pHYs", b"cHRM", b"bKGD", b"zzZz"):
            with self.subTest(kind), self.assertRaisesRegex(shots.Invalid, "unexpected"):
                shots.describe(encode(2, 2, bytes(12), extra=(chunk(kind, b"\0\0\0\0\0\0"),)), None)
        good = encode(2, 2, bytes(12))
        with self.assertRaisesRegex(shots.Invalid, "IHDR"):
            shots.describe(good[:33] + good[8:33] + good[33:], None)  # a second IHDR

    def test_transparency_is_refused(self):
        # RGB with a tRNS colour key would hash as opaque; it must not pass.
        rgb_key = encode(2, 2, bytes(12), extra=(chunk(b"tRNS", b"\0\0\0\0\0\0"),))
        with self.assertRaisesRegex(shots.Invalid, "tRNS"):
            shots.describe(rgb_key, None)

    def test_size_is_checked_before_inflating(self):
        # A wrong-size header with image data that would not even inflate:
        # the size error comes first, so a decompression bomb is never opened.
        for width, height in ((shots.WIDTH + 1, shots.HEIGHT), (8192, 8192), (1, 1)):
            data = encode(width, height, bytes(width * 3), idat=b"not zlib at all")
            with self.subTest((width, height)), self.assertRaisesRegex(shots.Invalid, "not 1280x800"):
                shots.describe(data)

    def test_zlib_stream_must_be_valid_and_complete(self):
        raw = b"\0" + bytes(6) + b"\0" + bytes(6)
        stream = zlib.compress(raw)
        cases = {
            "garbage": (b"\x78\x9c" + b"\xff" * 20, "does not inflate"),
            "missing checksum": (stream[:-4], "does not end"),
            "trailing bytes": (stream + b"\0\0", "does not end"),
            "second stream": (stream + zlib.compress(b"x"), "does not end"),
            "too much data": (zlib.compress(raw + b"\0"), "wrong size"),
        }
        for label, (idat, message) in cases.items():
            with self.subTest(label), self.assertRaisesRegex(shots.Invalid, message):
                shots.describe(encode(2, 2, bytes(12), idat=idat), None)
        self.assertEqual(2, shots.describe(encode(2, 2, bytes(12), idat=stream), None)["width"])


class Manifest(unittest.TestCase):
    def test_manifest_lists_the_four_images_without_host_details(self):
        with tempfile.TemporaryDirectory() as folder:
            write_artifact(Path(folder))
            manifest = json.loads((Path(folder) / "manifest.json").read_text())
            self.assertEqual({"source_commit", "images"}, set(manifest))
            self.assertEqual(COMMIT, manifest["source_commit"])
            self.assertEqual(set(shots.IMAGES), set(manifest["images"]))
            for name, entry in manifest["images"].items():
                data = (Path(folder) / name).read_bytes()
                self.assertEqual(hashlib.sha256(data).hexdigest(), entry["sha256"])
                self.assertEqual((1280, 800), (entry["width"], entry["height"]))
            self.assertNotIn(folder, json.dumps(manifest))
            self.assertEqual(manifest, shots.validate(Path(folder), COMMIT))

    def test_manifest_refuses_wrong_sizes_and_commits(self):
        with tempfile.TemporaryDirectory() as folder:
            write_artifact(Path(folder))
            with self.assertRaises(shots.Invalid):
                shots.build_manifest(Path(folder), "HEAD")
            (Path(folder) / "recording.png").write_bytes(solid(1, 1280, 801))
            with self.assertRaises(shots.Invalid):
                shots.build_manifest(Path(folder), COMMIT)


class Validation(unittest.TestCase):
    def check_refused(self, change, message):
        with tempfile.TemporaryDirectory() as root:
            folder = Path(root) / "artifact"
            write_artifact(folder)
            change(folder)
            with self.assertRaisesRegex(shots.Invalid, message):
                shots.validate(folder, COMMIT)

    def test_exact_names_regular_files_and_size_cap(self):
        self.check_refused(lambda f: (f / "extra.png").write_bytes(solid(1)), "unexpected artifact files")
        self.check_refused(lambda f: (f / "meeting.png").unlink(), "unexpected artifact files")

        def symlink(folder):
            (folder / "meeting.png").unlink()
            (folder / "meeting.png").symlink_to(folder / "library.png")
        self.check_refused(symlink, "not a regular file")

        def huge(folder):
            with (folder / "library.png").open("ab") as file:
                file.truncate(shots.MAX_IMAGE_BYTES + 1)
        self.check_refused(huge, "larger than")

    def test_manifest_must_match_commit_and_recomputed_hashes(self):
        def manifest(change):
            def apply(folder):
                data = json.loads((folder / "manifest.json").read_text())
                change(data)
                (folder / "manifest.json").write_text(json.dumps(data))
            return apply

        self.check_refused(manifest(lambda m: m.update(source_commit="b" * 40)), "source_commit")
        self.check_refused(manifest(lambda m: m.update(extra=1)), "exactly source_commit")
        self.check_refused(manifest(lambda m: m["images"]["library.png"].update(pixels_sha256="0" * 64)),
                           "does not match")
        self.check_refused(manifest(lambda m: m["images"].pop("meeting.png")), "exactly the four")
        self.check_refused(lambda f: (f / "meeting-dark.png").write_bytes(solid(99)), "does not match")
        self.check_refused(lambda f: (f / "manifest.json").write_text("[1"), "not JSON")

    def test_a_wrong_size_image_with_a_matching_manifest_is_refused(self):
        def resize(folder):
            (folder / "recording.png").write_bytes(solid(5, 1280, 720))
            data = json.loads((folder / "manifest.json").read_text())
            data["images"]["recording.png"] = shots.describe((folder / "recording.png").read_bytes(), None)
            (folder / "manifest.json").write_text(json.dumps(data))
        self.check_refused(resize, "not 1280x800")


class Decision(unittest.TestCase):
    def manifest(self, shade):
        with tempfile.TemporaryDirectory() as folder:
            write_artifact(Path(folder), shade=shade)
            return json.loads((Path(folder) / "manifest.json").read_text())

    def test_only_pixel_changes_publish(self):
        new = self.manifest(10)
        self.assertEqual("publish", shots.decide(new, None))
        self.assertEqual("publish", shots.decide(new, {"images": "garbage"}))
        self.assertEqual("publish", shots.decide(new, self.manifest(20)))
        same_pixels = json.loads(json.dumps(new))
        same_pixels["source_commit"] = "c" * 40
        for entry in same_pixels["images"].values():
            entry["sha256"] = "f" * 64
        self.assertEqual("unchanged", shots.decide(new, same_pixels))

    def test_published_manifest_is_read_defensively(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "manifest.json"
            self.assertIsNone(shots.read_published(path))
            path.write_text("not json")
            self.assertIsNone(shots.read_published(path))
            path.write_text("[]")
            self.assertIsNone(shots.read_published(path))
            path.write_text(json.dumps({"source_commit": "HEAD; rm -rf /"}))
            self.assertEqual("", shots.source_commit(shots.read_published(path)))
            path.write_text(json.dumps({"source_commit": COMMIT}))
            self.assertEqual(COMMIT, shots.source_commit(shots.read_published(path)))


class Workflow(unittest.TestCase):
    def setUp(self):
        self.workflow = (REPO / ".github/workflows/screenshots.yml").read_text(encoding="utf-8")
        self.header, jobs = self.workflow.split("\njobs:\n", 1)
        parts = re.split(r"^  ([a-z_]+):\n", jobs, flags=re.MULTILINE)
        self.jobs = dict(zip(parts[1::2], parts[2::2], strict=True))

    def test_triggers_never_use_pull_request_target(self):
        self.assertNotIn("pull_request_target", self.workflow)
        self.assertIn("push:\n    branches: [main]", self.header)
        self.assertIn("pull_request:\n    branches: [main]\n    paths:", self.header)
        self.assertIn("workflow_dispatch:", self.header)
        paths = re.findall(r"^      - '([^']+)'$", self.header, flags=re.MULTILINE)
        self.assertEqual([
            "crates/**", "third_party/**", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/**",
            "scripts/linux-desktop/**", "scripts/docker-linux-test.sh",
            ".github/workflows/screenshots.yml",
        ], paths)

    def test_least_privilege(self):
        self.assertRegex(self.header, r"(?m)^permissions: \{\}$")
        self.assertEqual(["capture", "publish"], list(self.jobs))
        self.assertIn("    permissions:\n      contents: read\n", self.jobs["capture"])
        self.assertNotIn("write", self.jobs["capture"])
        self.assertIn("    permissions:\n      contents: write\n", self.jobs["publish"])
        self.assertEqual(1, len(re.findall(r"(?m)^ +contents: write$", self.workflow)))
        self.assertNotIn("secrets.", self.workflow)

    def test_publish_guard_is_exact(self):
        self.assertIn(
            "    if: github.repository == 'BroToolbox/BroScribe' && github.ref == 'refs/heads/main' && "
            "(github.event_name == 'push' || github.event_name == 'workflow_dispatch')\n",
            self.jobs["publish"],
        )
        self.assertIn("    needs: capture\n", self.jobs["publish"])
        self.assertIn("    timeout-minutes: 10\n", self.jobs["publish"])
        self.assertIn("    runs-on: ubuntu-24.04-arm\n", self.jobs["capture"])
        self.assertRegex(self.jobs["capture"], r"timeout-minutes: \d+")
        self.assertIn('CARGO_BUILD_JOBS: "2"', self.jobs["capture"])

    def test_actions_are_the_release_pins_and_nothing_else(self):
        release = (REPO / ".github/workflows/release.yml").read_text(encoding="utf-8")
        pins = dict(re.findall(r"uses: (actions/[a-z-]+)@([0-9a-f]{40})\n", release))
        actions = re.findall(r"(?m)^\s+uses:\s*(\S+)", self.workflow)
        upload = f"actions/upload-artifact@{pins['actions/upload-artifact']}"
        self.assertEqual([upload, upload, f"actions/download-artifact@{pins['actions/download-artifact']}"],
                         actions)
        for forbidden in ("actions/checkout", "actions/cache", "cache:"):
            self.assertNotIn(forbidden, self.workflow)

    def test_checkout_matches_ci_and_token_stays_out_of_argv(self):
        ci = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        marker = "      - name: Check out the exact commit\n"
        ci_checkout = ci.split(marker, 2)[1].split("      - name:", 1)[0]
        for job in ("capture", "publish"):
            self.assertEqual(ci_checkout, self.jobs[job].split(marker, 1)[1].split("      - name:", 1)[0])
        publish = self.jobs["publish"].split("      - name: Replace the screenshots branch", 1)[1]
        self.assertIn('echo "::add-mask::${auth}"', publish)
        self.assertIn('export GIT_CONFIG_VALUE_0="AUTHORIZATION: basic ${auth}"', publish)
        self.assertNotIn("git config", publish.replace("git config core.autocrlf", ""))
        self.assertNotRegex(publish, r"https://[^\s\"]*@")
        self.assertNotIn("${{ github.token }}@", self.workflow)
        self.assertIn('--force-with-lease="refs/heads/screenshots:${tip}"', publish)

    def test_concurrency(self):
        # Only main shares the serialized group; a manual run on another
        # branch must not queue behind (or hold up) main's publication.
        self.assertIn(
            "  group: ${{ github.ref == 'refs/heads/main' && 'screenshots-main' || "
            "format('screenshots-ref-{0}', github.ref) }}\n"
            "  cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}\n",
            self.header,
        )

    def test_artifact_holds_only_the_expected_files(self):
        upload = self.jobs["capture"].split("      - name: Upload the screenshots\n", 1)[1]
        paths = re.findall(r"\$\{\{ runner\.temp \}\}/screenshots/(\S+)", upload)
        self.assertEqual(sorted(shots.EXPECTED_FILES), sorted(paths))
        self.assertIn("if-no-files-found: error", upload)
        self.assertIn("name: screenshots\n", upload)
        download = self.jobs["publish"].split("      - name: Download this run's screenshots\n", 1)[1]
        self.assertIn("path: ${{ runner.temp }}/screenshots-artifact\n", download)
        self.assertEqual(1, self.workflow.count("actions/download-artifact@"))
        self.assertIn("          name: screenshots\n", download.split("      - name:", 1)[0])
        validate = self.jobs["publish"].split("      - name: Validate the artifact as data\n", 1)[1]
        self.assertIn('shots.py validate "$RUNNER_TEMP/screenshots-artifact" --commit "$GITHUB_SHA"',
                      validate.split("      - name:", 1)[0])

    def test_diagnostics_are_uploaded_only_on_failure_and_never_published(self):
        step = self.jobs["capture"].split("      - name: Upload the failure diagnostics\n", 1)[1]
        self.assertEqual(
            "        if: failure()\n"
            "        uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02\n"
            "        with:\n"
            "          name: screenshots-diagnostics\n"
            "          path: ${{ runner.temp }}/screenshots-diagnostics\n"
            "          if-no-files-found: ignore\n"
            "          retention-days: 3\n",
            step.split("\n      - name:", 1)[0].rstrip("\n") + "\n",
        )
        capture = self.jobs["capture"].split("      - name: Capture the screenshots", 1)[1]
        self.assertIn('"$RUNNER_TEMP/screenshots" "$RUNNER_TEMP/screenshots-diagnostics"',
                      capture.split("      - name:", 1)[0])
        self.assertNotIn("screenshots-diagnostics", self.jobs["publish"])


@unittest.skipUnless(shutil.which("git"), "git is required")
class PublishStep(unittest.TestCase):
    """Run the publish step's script against a local bare repository."""

    def setUp(self):
        workflow = (REPO / ".github/workflows/screenshots.yml").read_text(encoding="utf-8")
        step = workflow.split("      - name: Replace the screenshots branch when the pixels changed\n", 1)[1]
        self.script = textwrap.dedent(step.split("        run: |\n", 1)[1])
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.server = self.root / "server"
        self.remote = self.server / "owner/repo.git"
        self.git("init", "--quiet", "--bare", "--initial-branch=main", str(self.remote))
        self.git("-C", str(self.remote), "config", "uploadpack.allowAnySHA1InWant", "true")
        self.source = self.root / "source"
        self.git("init", "--quiet", "--initial-branch=main", str(self.source))
        (self.source / "scripts/linux-desktop").mkdir(parents=True)
        shutil.copy(HARNESS / "shots.py", self.source / "scripts/linux-desktop/shots.py")

    def tearDown(self):
        self.tmp.cleanup()

    def git(self, *args, cwd=None):
        env = {**os.environ, "GIT_AUTHOR_NAME": "Synthetic", "GIT_AUTHOR_EMAIL": "synthetic@example.invalid",
               "GIT_COMMITTER_NAME": "Synthetic", "GIT_COMMITTER_EMAIL": "synthetic@example.invalid",
               "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        return subprocess.run(["git", *args], cwd=cwd, env=env, check=True,
                              capture_output=True, text=True).stdout.strip()

    def commit_main(self, label):
        (self.source / "change.txt").write_text(label)
        self.git("add", "--all", cwd=self.source)
        self.git("commit", "--quiet", "-m", label, cwd=self.source)
        self.git("push", "--quiet", str(self.remote), "HEAD:refs/heads/main", cwd=self.source)
        return self.git("rev-parse", "HEAD", cwd=self.source)

    def publish(self, commit, shade, succeed=True, shallow=True, path=None):
        self.runs = getattr(self, "runs", 0) + 1
        run = self.root / f"run-{self.runs}"
        checkout = run / "checkout"
        checkout.mkdir(parents=True)
        self.git("init", "--quiet", cwd=checkout)
        self.git("remote", "add", "origin", f"file://{self.server}/owner/repo.git", cwd=checkout)
        depth = ["--depth=1"] if shallow else []
        self.git("fetch", "--quiet", "--no-tags", *depth, "origin", commit, cwd=checkout)
        self.git("checkout", "--quiet", "--detach", "FETCH_HEAD", cwd=checkout)
        write_artifact(run / "screenshots-artifact", commit, shade)
        env = {**os.environ, "GITHUB_SHA": commit, "GITHUB_SERVER_URL": f"file://{self.server}",
               "GITHUB_REPOSITORY": "owner/repo", "RUNNER_TEMP": str(run), "PUSH_TOKEN": "synthetic-token",
               "GIT_AUTHOR_NAME": "Synthetic", "GIT_AUTHOR_EMAIL": "synthetic@example.invalid",
               "GIT_COMMITTER_NAME": "Synthetic", "GIT_COMMITTER_EMAIL": "synthetic@example.invalid",
               "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        if path:
            env["PATH"] = f"{path}{os.pathsep}{env['PATH']}"
        result = subprocess.run(["bash", "-c", self.script], cwd=checkout, env=env,
                                capture_output=True, text=True)
        if succeed:
            self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        else:
            self.assertNotEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertNotIn("synthetic-token", result.stdout + result.stderr)
        return result.stdout + result.stderr

    def branch(self):
        try:
            tip = self.git("-C", str(self.remote), "rev-parse", "refs/heads/screenshots")
        except subprocess.CalledProcessError:
            return None
        files = self.git("-C", str(self.remote), "ls-tree", "--name-only", tip).split()
        parents = self.git("-C", str(self.remote), "log", "--format=%P", "-1", tip)
        manifest = json.loads(self.git("-C", str(self.remote), "show", f"{tip}:manifest.json"))
        return tip, files, parents, manifest["source_commit"]

    def test_publishes_changes_once_as_a_single_orphan_commit(self):
        first = self.commit_main("first")
        self.assertIn("Published", self.publish(first, 10))
        tip, files, parents, source = self.branch()
        self.assertEqual(sorted(shots.EXPECTED_FILES | {"README.md"}), sorted(files))
        self.assertEqual("", parents)
        self.assertEqual(first, source)

        self.assertIn("already published", self.publish(first, 10))
        second = self.commit_main("second")
        self.assertIn("published pixels", self.publish(second, 10))
        self.assertEqual(tip, self.branch()[0])

        third = self.commit_main("third")
        self.assertIn("Published", self.publish(third, 40))
        new_tip, _, parents, source = self.branch()
        self.assertNotEqual(tip, new_tip)
        self.assertEqual(("", third), (parents, source))

    def test_skips_stale_runs(self):
        first = self.commit_main("first")
        second = self.commit_main("second")
        self.assertIn("main is now at", self.publish(first, 10))
        self.assertIsNone(self.branch())
        self.assertIn("Published", self.publish(second, 20))
        # A rerun of an older commit after main moved back to it (a revert
        # by force push is not allowed on main, so only reruns reach this).
        self.git("push", "--quiet", "--force", str(self.remote), f"{first}:refs/heads/main", cwd=self.source)
        self.assertIn("newer", self.publish(first, 10))
        self.assertEqual(second, self.branch()[3])

    def publish_tip(self, commit, shade, manifest_commit):
        """Publish, then rewrite the branch's manifest to name another source."""
        self.publish(commit, shade)
        work = self.root / "rewrite"
        self.git("clone", "--quiet", "--branch", "screenshots", str(self.remote), str(work))
        manifest = json.loads((work / "manifest.json").read_text())
        manifest["source_commit"] = manifest_commit
        (work / "manifest.json").write_text(json.dumps(manifest))
        self.git("commit", "--quiet", "--all", "--amend", "-m", "rewritten", cwd=work)
        self.git("push", "--quiet", "--force", "origin", "HEAD:refs/heads/screenshots", cwd=work)
        shutil.rmtree(work)

    def test_first_creation_uses_an_empty_lease(self):
        self.assertIsNone(self.branch())
        first = self.commit_main("first")
        self.assertIn("Published", self.publish(first, 10))
        self.assertEqual(first, self.branch()[3])

    def test_lease_refuses_a_branch_that_changed_after_it_was_read(self):
        # A wrapper creates or moves the branch just before the push, as a
        # concurrent publication would; the lease must refuse both cases.
        wrapper = self.root / "bin"
        wrapper.mkdir()
        real_git = shutil.which("git")
        (wrapper / "git").write_text(
            "#!/usr/bin/env bash\n"
            'if [[ " $* " == *" push "* ]]; then\n'
            f'  GIT_DIR="{self.remote}" "{real_git}" update-ref refs/heads/screenshots "$RACE_COMMIT"\n'
            "fi\n"
            f'exec "{real_git}" "$@"\n'
        )
        (wrapper / "git").chmod(0o755)
        first = self.commit_main("first")
        os.environ["RACE_COMMIT"] = first
        self.addCleanup(os.environ.pop, "RACE_COMMIT", None)
        output = self.publish(first, 10, succeed=False, path=wrapper)
        self.assertRegex(output, "stale info|rejected")
        self.assertEqual(first, self.git("-C", str(self.remote), "rev-parse", "refs/heads/screenshots"))

        self.git("-C", str(self.remote), "update-ref", "-d", "refs/heads/screenshots")
        self.publish(first, 10)
        tip = self.branch()[0]
        second = self.commit_main("second")
        os.environ["RACE_COMMIT"] = second
        output = self.publish(second, 20, succeed=False, path=wrapper)
        self.assertRegex(output, "stale info|rejected")
        self.assertNotEqual(tip, self.git("-C", str(self.remote), "rev-parse", "refs/heads/screenshots"))

    def test_older_published_source_is_compared_through_history(self):
        # The published commit is an ancestor of the new one: the shallow
        # checkout is deepened, and changed pixels are published.
        first = self.commit_main("first")
        self.publish(first, 10)
        second = self.commit_main("second")
        self.assertIn("Published", self.publish(second, 30))
        self.assertEqual(second, self.branch()[3])
        # The same from a complete (not shallow) checkout: no --unshallow error.
        third = self.commit_main("third")
        self.assertIn("Published", self.publish(third, 50, shallow=False))
        self.assertEqual(third, self.branch()[3])

    def test_unrelated_published_source_is_replaced(self):
        # Not an ancestor and not a descendant (status 1): publish.
        first = self.commit_main("first")
        side = self.root / "side"
        self.git("init", "--quiet", "--initial-branch=side", str(side))
        (side / "other.txt").write_text("other")
        self.git("add", "--all", cwd=side)
        self.git("commit", "--quiet", "-m", "other", cwd=side)
        self.git("push", "--quiet", str(self.remote), "HEAD:refs/heads/side", cwd=side)
        other = self.git("rev-parse", "HEAD", cwd=side)
        self.publish_tip(first, 10, other)
        self.assertIn("Published", self.publish(first, 20))
        self.assertEqual(first, self.branch()[3])

    def test_an_unfetchable_published_source_fails_the_job(self):
        first = self.commit_main("first")
        self.publish_tip(first, 10, "b" * 40)
        tip = self.branch()[0]
        self.publish(first, 20, succeed=False)
        self.assertEqual(tip, self.branch()[0])


if __name__ == "__main__":
    unittest.main()

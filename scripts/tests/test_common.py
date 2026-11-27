"""Tests for scripts/common.sh: git_tree_dirty, the predicate bundle-macos.sh
records as the build's `git.dirty` (build-info.json → release provenance).

Each test makes its own git repository with synthetic files in a temporary
folder. System and global git configuration, templates, hooks and signing are
disabled, no remote exists, and the project's own repository is never touched.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from support import SCRIPTS

COMMON = SCRIPTS / "common.sh"
# Per-invocation settings that override anything a repository could carry.
NO_HOOKS_OR_SIGNING = ("-c", f"core.hooksPath={os.devnull}", "-c", "commit.gpgsign=false",
                       "-c", "tag.gpgsign=false", "-c", "core.fsmonitor=false")


@unittest.skipIf(shutil.which("bash") is None or shutil.which("git") is None, "bash and git are required")
class GitTreeDirtyTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        # No GIT_DIR/GIT_WORK_TREE or other git variable reaches the fixture,
        # and discovery never climbs above the temporary folder.
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.env.update(
            HOME=str(self.root), XDG_CONFIG_HOME=str(self.root / "config"),
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_SYSTEM=os.devnull, GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CEILING_DIRECTORIES=str(self.root), GIT_TERMINAL_PROMPT="0",
            GIT_AUTHOR_NAME="Synthetic", GIT_AUTHOR_EMAIL="synthetic@example.invalid",
            GIT_COMMITTER_NAME="Synthetic", GIT_COMMITTER_EMAIL="synthetic@example.invalid",
            GIT_AUTHOR_DATE="2026-11-18T12:00:00Z", GIT_COMMITTER_DATE="2026-11-18T12:00:00Z")
        self.repo = self.root / "repo"
        self.git("init", "--quiet", "--template=", "--initial-branch=main", str(self.repo), cwd=self.root)
        self.write("src/lib.rs", "// synthetic\n")
        self.write(".gitignore", "target/\n")
        self.git("add", "src/lib.rs", ".gitignore")
        self.git("commit", "--quiet", "--no-verify", "-m", "synthetic")

    def git(self, *args: str, cwd: Path | None = None) -> None:
        subprocess.run(["git", *NO_HOOKS_OR_SIGNING, *args], cwd=cwd or self.repo, env=self.env,
                       check=True, capture_output=True, text=True)

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def dirty(self, folder: Path) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", "-c", 'source "$0"; git_tree_dirty "$1"', str(COMMON), str(folder)],
                              env=self.env, capture_output=True, text=True)

    def assert_dirty(self, expected: str) -> None:
        result = self.dirty(self.repo)
        self.assertEqual((result.returncode, result.stdout), (0, f"{expected}\n"), result.stderr)

    def test_a_committed_tree_with_ignored_build_output_is_clean(self):
        self.assert_dirty("false")
        self.write("target/aarch64-apple-darwin/release/broscribe", "synthetic build output\n")
        self.assert_dirty("false")

    def test_an_untracked_source_file_is_dirty(self):
        self.write("src/new.rs", "// synthetic, not committed\n")
        self.assert_dirty("true")

    def test_an_untracked_crate_folder_is_dirty(self):
        self.write("crates/synthetic/src/lib.rs", "// synthetic, not committed\n")
        self.assert_dirty("true")

    def test_a_tracked_change_is_dirty_staged_or_not(self):
        self.write("src/lib.rs", "// synthetic, changed\n")
        self.assert_dirty("true")
        self.git("add", "src/lib.rs")
        self.assert_dirty("true")

    def test_a_folder_git_cannot_read_fails_closed(self):
        plain = self.root / "plain"
        plain.mkdir()
        result = self.dirty(plain)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("cannot read the git status", result.stderr)


if __name__ == "__main__":
    unittest.main()

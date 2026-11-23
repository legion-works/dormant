import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
CHANGED_FILES = ROOT / "scripts/gates/changed-files.sh"
ZERO = "0" * 40


def git(cwd: pathlib.Path, *args: str) -> str:
    env = dict(os.environ)
    env.update(
        GIT_AUTHOR_NAME="t",
        GIT_AUTHOR_EMAIL="t@example.invalid",
        GIT_COMMITTER_NAME="t",
        GIT_COMMITTER_EMAIL="t@example.invalid",
        GIT_CONFIG_GLOBAL="/dev/null",
        GIT_CONFIG_SYSTEM="/dev/null",
    )
    return subprocess.run(
        ["git", *args], cwd=cwd, env=env, check=True, capture_output=True, text=True
    ).stdout.strip()


class ChangedFilesTests(unittest.TestCase):
    """The pre-push filter must not skip a gate for a branch the remote has not seen."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        temp = pathlib.Path(self._tmp.name)
        self.remote = temp / "remote.git"
        self.work = temp / "work"
        subprocess.run(["git", "init", "-q", "--bare", str(self.remote)], check=True)
        self.work.mkdir()
        git(self.work, "init", "-q", "-b", "dev")
        (self.work / "README.md").write_text("base\n")
        git(self.work, "add", "-A")
        git(self.work, "commit", "-q", "-m", "base")
        self.base = git(self.work, "rev-parse", "HEAD")
        git(self.work, "remote", "add", "origin", str(self.remote))
        git(self.work, "push", "-q", "origin", "dev")

    def tearDown(self):
        self._tmp.cleanup()

    def branch_from_sha(self, name: str, path: str):
        # A branch started from a commit id has no upstream.
        git(self.work, "checkout", "-q", "-b", name, self.base)
        (self.work / path).parent.mkdir(parents=True, exist_ok=True)
        (self.work / path).write_text("change\n")
        git(self.work, "add", "-A")
        git(self.work, "commit", "-q", "-m", "change")

    def run_filter(self, push_line: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(CHANGED_FILES), "--pattern", "*.rs", "--", "echo", "GATE-RAN"],
            cwd=self.work,
            input=push_line,
            capture_output=True,
            text=True,
            check=False,
        )

    def new_branch_push_line(self, name: str) -> str:
        head = git(self.work, "rev-parse", "HEAD")
        return f"refs/heads/{name} {head} refs/heads/{name} {ZERO}\n"

    def test_new_branch_without_upstream_runs_gate_for_matching_change(self):
        self.branch_from_sha("feature", "src/lib.rs")
        result = self.run_filter(self.new_branch_push_line("feature"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE-RAN", result.stdout)

    def test_new_branch_without_upstream_skips_gate_for_unmatched_change(self):
        self.branch_from_sha("docs-only", "docs/page.md")
        result = self.run_filter(self.new_branch_push_line("docs-only"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("GATE-RAN", result.stdout)

    def test_new_branch_without_integration_ref_runs_gate(self):
        self.branch_from_sha("feature", "src/lib.rs")
        git(self.work, "update-ref", "-d", "refs/remotes/origin/dev")
        result = self.run_filter(self.new_branch_push_line("feature"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE-RAN", result.stdout)

    def test_existing_remote_branch_checks_only_unpushed_commits(self):
        self.branch_from_sha("feature", "src/lib.rs")
        pushed = git(self.work, "rev-parse", "HEAD")
        git(self.work, "push", "-q", "origin", "feature")
        (self.work / "docs").mkdir()
        (self.work / "docs/page.md").write_text("docs\n")
        git(self.work, "add", "-A")
        git(self.work, "commit", "-q", "-m", "docs")
        head = git(self.work, "rev-parse", "HEAD")
        result = self.run_filter(f"refs/heads/feature {head} refs/heads/feature {pushed}\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("GATE-RAN", result.stdout)


if __name__ == "__main__":
    unittest.main()

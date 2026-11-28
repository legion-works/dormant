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
            [
                "bash", str(CHANGED_FILES), "--pattern", "*.rs", "--", "bash", "-c",
                'echo "GATE-RAN ${DORMANT_PUSHED_SHA-<unset>}"',
            ],
            cwd=self.work,
            env=dict(os.environ, DORMANT_PUSHED_SHA="stale"),
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

    def test_remote_commit_missing_locally_runs_gate_for_matching_change(self):
        # A force push after a rebase can name a remote commit this clone does
        # not have; the merge-base with it is unknown.
        self.branch_from_sha("feature", "src/lib.rs")
        head = git(self.work, "rev-parse", "HEAD")
        missing = "1234567890abcdef1234567890abcdef12345678"
        result = self.run_filter(f"refs/heads/feature {head} refs/heads/feature {missing}\n")
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

    def test_second_pushed_ref_with_matching_change_runs_gate(self):
        # `git push origin a b` sends one stdin line for each ref.
        self.branch_from_sha("docs-only", "docs/page.md")
        docs_line = self.new_branch_push_line("docs-only")
        self.branch_from_sha("feature", "src/lib.rs")
        feature_line = self.new_branch_push_line("feature")
        # HEAD must not carry the matching change, or the test passes on HEAD alone.
        git(self.work, "checkout", "-q", "docs-only")
        result = self.run_filter(docs_line + feature_line)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE-RAN", result.stdout)
        with self.subTest("warns about the working tree"):
            self.assertIn("multiple distinct local SHAs; running gate on the working tree", result.stderr)
        with self.subTest("clears the inherited SHA"):
            self.assertEqual(result.stdout.strip(), "GATE-RAN <unset>")

    def test_pushed_commit_is_checked_not_the_checked_out_branch(self):
        # `git push origin feature` from another branch pushes feature's commit.
        self.branch_from_sha("feature", "src/lib.rs")
        feature = git(self.work, "rev-parse", "HEAD")
        self.branch_from_sha("docs-only", "docs/page.md")
        result = self.run_filter(f"refs/heads/feature {feature} refs/heads/feature {ZERO}\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), f"GATE-RAN {feature}")

    def test_duplicate_pushed_sha_and_deletion_keep_sha_when_diff_is_unknown(self):
        self.branch_from_sha("feature", "src/lib.rs")
        feature = git(self.work, "rev-parse", "HEAD")
        missing = "1234567890abcdef1234567890abcdef12345678"
        result = self.run_filter(
            f"refs/heads/feature {feature} refs/heads/feature {missing}\n"
            f"(delete) {ZERO} refs/heads/old {self.base}\n"
            f"refs/heads/feature {feature} refs/heads/copy {ZERO}\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Old remote commit is not available", result.stderr)
        self.assertEqual(result.stdout.strip(), f"GATE-RAN {feature}")

    def test_unmatched_pushed_commit_skips_gate_while_head_matches(self):
        # The control for the test above: HEAD has a matching change, the pushed commit does not.
        self.branch_from_sha("docs-only", "docs/page.md")
        docs = git(self.work, "rev-parse", "HEAD")
        self.branch_from_sha("feature", "src/lib.rs")
        result = self.run_filter(f"refs/heads/docs-only {docs} refs/heads/docs-only {ZERO}\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("GATE-RAN", result.stdout)

    def test_branch_deletion_only_skips_gate(self):
        self.branch_from_sha("feature", "src/lib.rs")
        head = git(self.work, "rev-parse", "HEAD")
        result = self.run_filter(f"(delete) {ZERO} refs/heads/old {head}\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("GATE-RAN", result.stdout)

    def commit_file(self, path: str, text: str = "change\n") -> str:
        (self.work / path).parent.mkdir(parents=True, exist_ok=True)
        (self.work / path).write_text(text)
        git(self.work, "add", "-A")
        git(self.work, "commit", "-q", "-m", f"add {path}")
        return git(self.work, "rev-parse", "HEAD")

    def test_force_rewind_that_removes_a_matching_file_runs_gate(self):
        # The remote tip has the Rust change; the push rewinds to its parent.
        self.branch_from_sha("feature", "docs/page.md")
        parent = git(self.work, "rev-parse", "HEAD")
        tip = self.commit_file("src/lib.rs")
        git(self.work, "push", "-q", "origin", "feature")
        git(self.work, "reset", "-q", "--hard", parent)
        result = self.run_filter(f"refs/heads/feature {parent} refs/heads/feature {tip}\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE-RAN", result.stdout)

    def test_existing_ref_with_old_commit_missing_locally_runs_gate(self):
        # Another clone moved the remote ref; this clone cannot read the old
        # commit, so it cannot know what the push removes.
        self.branch_from_sha("docs-only", "docs/page.md")
        head = git(self.work, "rev-parse", "HEAD")
        missing = "1234567890abcdef1234567890abcdef12345678"
        result = self.run_filter(f"refs/heads/dev {head} refs/heads/dev {missing}\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE-RAN", result.stdout)

    def test_rename_away_from_a_matching_path_runs_gate(self):
        self.branch_from_sha("feature", "src/lib.rs")
        pushed = git(self.work, "rev-parse", "HEAD")
        git(self.work, "push", "-q", "origin", "feature")
        (self.work / "docs").mkdir()
        git(self.work, "mv", "src/lib.rs", "docs/lib.txt")
        git(self.work, "commit", "-q", "-m", "rename")
        head = git(self.work, "rev-parse", "HEAD")
        result = self.run_filter(f"refs/heads/feature {head} refs/heads/feature {pushed}\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE-RAN", result.stdout)

    def test_non_ascii_matching_path_runs_gate(self):
        # Without -z, git quotes this path and the quote ends the name.
        self.branch_from_sha("feature", "src/\u00fcb.rs")
        result = self.run_filter(self.new_branch_push_line("feature"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GATE-RAN", result.stdout)


if __name__ == "__main__":
    unittest.main()

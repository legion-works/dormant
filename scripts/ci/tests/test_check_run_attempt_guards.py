import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import check_run_attempt_guards


EXPECTED_CHECKOUT_JOBS = {
    "fmt",
    "webui",
    "clippy",
    "test",
    "render",
    "windows-portability",
    "macos-test",
    "changed-test-stress-ubuntu",
    "changed-test-stress-macos",
    "macos-msrv",
    "deny",
    "msrv",
    "mqtt-integration",
    "docs",
    "mdbook",
    "taplo",
    "gitleaks",
    "typos",
    "policy",
}

CHECKOUT_LESS_EXEMPTIONS = {"pr-title"}
ROOT = pathlib.Path(__file__).resolve().parents[3]
REJECT_SCRIPT = ROOT / "scripts/ci/reject_required_rerun.sh"
CHECKER = ROOT / "scripts/ci/check_run_attempt_guards.py"


class CheckRunAttemptGuardsTests(unittest.TestCase):
    def assert_checker_rejects(self, steps: str, job_if: str = ""):
        with tempfile.TemporaryDirectory() as directory:
            workflow = pathlib.Path(directory) / "ci.yml"
            workflow.write_text(
                "jobs:\n"
                "  test:\n"
                f"{job_if}"
                "    steps:\n"
                "      - uses: actions/checkout@v4\n"
                f"{steps}",
                encoding="utf-8",
            )

            result = subprocess.run(
                ["python3", str(CHECKER), str(workflow)],
                capture_output=True,
                text=True,
                check=False,
            )

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_reports_checkout_without_later_guard(self):
        with tempfile.TemporaryDirectory() as directory:
            workflow = pathlib.Path(directory) / "ci.yml"
            workflow.write_text(
                "jobs:\n"
                "  test:\n"
                "    steps:\n"
                "      - uses: actions/checkout@v4\n"
                "      - run: cargo test\n",
                encoding="utf-8",
            )

            errors = check_run_attempt_guards.validate_guards(workflow)

        self.assertEqual(errors, ["test: checkout step 1 has no later rerun guard"])

    def test_requires_guard_after_checkout(self):
        with tempfile.TemporaryDirectory() as directory:
            workflow = pathlib.Path(directory) / "ci.yml"
            workflow.write_text(
                "jobs:\n"
                "  test:\n"
                "    steps:\n"
                "      - run: bash scripts/ci/reject_required_rerun.sh\n"
                "      - uses: actions/checkout@v4\n"
                "      - run: cargo test\n",
                encoding="utf-8",
            )

            errors = check_run_attempt_guards.validate_guards(workflow)

        self.assertEqual(errors, ["test: checkout step 2 has no later rerun guard"])

    def test_accepts_guard_after_checkout(self):
        with tempfile.TemporaryDirectory() as directory:
            workflow = pathlib.Path(directory) / "ci.yml"
            workflow.write_text(
                "jobs:\n"
                "  test:\n"
                "    steps:\n"
                "      - uses: actions/checkout@v4\n"
                "      - run: bash scripts/ci/reject_required_rerun.sh\n"
                "      - run: cargo test\n",
                encoding="utf-8",
            )

            errors = check_run_attempt_guards.validate_guards(workflow)

        self.assertEqual(errors, [])

    def test_rejects_conditional_guard(self):
        self.assert_checker_rejects(
            "      - if: github.run_attempt == 1\n"
            "        run: bash scripts/ci/reject_required_rerun.sh\n"
        )

    def test_rejects_continue_on_error_guard(self):
        self.assert_checker_rejects(
            "      - continue-on-error: true\n"
            "        run: bash scripts/ci/reject_required_rerun.sh\n"
        )

    def test_rejects_echo_only_guard(self):
        self.assert_checker_rejects(
            "      - run: echo scripts/ci/reject_required_rerun.sh\n"
        )

    def test_rejects_job_level_run_attempt_condition(self):
        self.assert_checker_rejects(
            "      - run: bash scripts/ci/reject_required_rerun.sh\n",
            "    if: github.run_attempt == 1\n",
        )

    def test_reject_script_allows_attempt_one(self):
        result = subprocess.run(
            ["bash", str(REJECT_SCRIPT)],
            env={**os.environ, "GITHUB_RUN_ATTEMPT": "1"},
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")

    def test_reject_script_allows_unset_attempt(self):
        environment = os.environ.copy()
        environment.pop("GITHUB_RUN_ATTEMPT", None)
        result = subprocess.run(
            ["bash", str(REJECT_SCRIPT)],
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")

    def test_reject_script_rejects_later_attempt(self):
        result = subprocess.run(
            ["bash", str(REJECT_SCRIPT)],
            env={**os.environ, "GITHUB_RUN_ATTEMPT": "2"},
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stdout,
            "Same-SHA reruns are diagnostic only; push a new commit and update "
            ".github/flake-ledger.toml.\n",
        )
        self.assertEqual(result.stderr, "")

    def test_ci_coverage_lists_every_required_job_and_only_pr_title_exemption(self):
        workflow = ROOT / ".github/workflows/ci.yml"
        ci = check_run_attempt_guards.load_yaml(workflow)
        jobs = ci["jobs"]
        checkout_jobs = {
            name
            for name, job in jobs.items()
            if check_run_attempt_guards.checkout_steps(job)
        }

        self.assertEqual(checkout_jobs, EXPECTED_CHECKOUT_JOBS)
        self.assertEqual(set(jobs) - checkout_jobs, CHECKOUT_LESS_EXEMPTIONS)
        self.assertEqual(check_run_attempt_guards.validate_guards(workflow), [])


if __name__ == "__main__":
    unittest.main()

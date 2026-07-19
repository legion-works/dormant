import pathlib
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


class CheckRunAttemptGuardsTests(unittest.TestCase):
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

    def test_ci_coverage_lists_every_required_job_and_only_pr_title_exemption(self):
        workflow = pathlib.Path(__file__).resolve().parents[3] / ".github/workflows/ci.yml"
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

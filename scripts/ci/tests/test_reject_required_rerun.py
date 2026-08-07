import json
import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
RERUN_GUARD = ROOT / "scripts/ci/reject_required_rerun.sh"
LABEL = "ci-infra-rerun"
REJECTION = (
    "Same-SHA reruns are diagnostic only; push a new commit and update "
    ".github/flake-ledger.toml.\n"
)


class RejectRequiredRerunTests(unittest.TestCase):
    def run_guard(
        self,
        attempt: str,
        event: object | None = None,
        path: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ | {"GITHUB_RUN_ATTEMPT": attempt}
        with tempfile.TemporaryDirectory() as directory:
            if event is not None:
                event_path = pathlib.Path(directory) / "event.json"
                event_path.write_text(json.dumps(event), encoding="utf-8")
                environment["GITHUB_EVENT_PATH"] = str(event_path)
            if path is not None:
                environment["PATH"] = path
            return subprocess.run(
                ["/bin/bash", str(RERUN_GUARD)],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )

    def assert_rejected(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(result.stdout, REJECTION)
        self.assertEqual(result.stderr, "")

    def test_attempt_one_passes_without_override(self):
        result = self.run_guard("1")

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")

    def test_later_attempt_without_label_is_rejected(self):
        result = self.run_guard("2", {"pull_request": {"labels": [{"name": "other"}]}})

        self.assert_rejected(result)

    def test_later_attempt_without_payload_is_rejected(self):
        result = self.run_guard("2")

        self.assert_rejected(result)

    def test_later_attempt_with_infrastructure_label_passes(self):
        result = self.run_guard("2", {"pull_request": {"labels": [{"name": LABEL}]}})

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(LABEL, result.stdout)
        self.assertIn("infrastructure failures only", result.stdout)
        self.assertEqual(result.stderr, "")

    def test_later_attempt_with_malformed_payload_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            event_path = pathlib.Path(directory) / "event.json"
            event_path.write_text("{", encoding="utf-8")
            result = subprocess.run(
                ["/bin/bash", str(RERUN_GUARD)],
                env=os.environ | {
                    "GITHUB_RUN_ATTEMPT": "2",
                    "GITHUB_EVENT_PATH": str(event_path),
                },
                capture_output=True,
                text=True,
                check=False,
            )

        self.assert_rejected(result)

    def test_later_attempt_without_pull_request_is_rejected(self):
        result = self.run_guard("2", {"ref": "refs/heads/dev"})

        self.assert_rejected(result)

    def test_later_attempt_with_label_and_no_jq_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_guard(
                "2",
                {"pull_request": {"labels": [{"name": LABEL}]}},
                directory,
            )

        self.assert_rejected(result)

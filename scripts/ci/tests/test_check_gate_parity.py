import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import check_gate_parity


class CheckGateParityTests(unittest.TestCase):
    def test_reports_missing_required_script_for_each_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ci = root / "ci.yml"
            lefthook = root / "lefthook.yml"
            manifest = root / "parity.txt"
            ci.write_text("jobs:\n  fmt:\n    steps:\n      - run: bash scripts/gates/fmt.sh\n")
            lefthook.write_text(
                "pre-commit:\n  jobs:\n    - run: bash scripts/gates/fmt.sh\npre-push:\n  jobs: []\n"
            )
            manifest.write_text("ci\tscripts/gates/fmt.sh\npre-push\tscripts/gates/clippy.sh\n")

            errors = check_gate_parity.validate_parity(ci, lefthook, manifest)

        self.assertEqual(errors, ["pre-push: missing required script scripts/gates/clippy.sh"])

    def test_reports_raw_gate_command_in_yaml(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ci = root / "ci.yml"
            lefthook = root / "lefthook.yml"
            manifest = root / "parity.txt"
            ci.write_text("jobs:\n  fmt:\n    steps:\n      - run: cargo fmt --all -- --check\n")
            lefthook.write_text("pre-commit:\n  jobs: []\npre-push:\n  jobs: []\n")
            manifest.write_text("ci\tscripts/gates/fmt.sh\n")

            errors = check_gate_parity.validate_parity(ci, lefthook, manifest)

        self.assertEqual(
            errors,
            [
                "ci: missing required script scripts/gates/fmt.sh",
                "ci: raw gate command remains in YAML: cargo fmt --all -- --check",
            ],
        )

    def test_accepts_script_only_gate_invocations(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ci = root / "ci.yml"
            lefthook = root / "lefthook.yml"
            manifest = root / "parity.txt"
            ci.write_text("jobs:\n  fmt:\n    steps:\n      - run: bash scripts/gates/fmt.sh\n")
            lefthook.write_text(
                "pre-commit:\n  jobs:\n    - run: bash scripts/gates/taplo.sh\npre-push:\n  jobs:\n    - run: bash scripts/gates/clippy.sh\n"
            )
            manifest.write_text(
                "ci\tscripts/gates/fmt.sh\npre-commit\tscripts/gates/taplo.sh\npre-push\tscripts/gates/clippy.sh\n"
            )

            errors = check_gate_parity.validate_parity(ci, lefthook, manifest)

        self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main()

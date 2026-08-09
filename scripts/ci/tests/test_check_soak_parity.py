import pathlib
import tempfile
import unittest

import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import check_soak_parity


LEDGER = '''
schema_version = 1

[[incident]]
id = "FLAKE-0001"
test = "module::tests::known_flake"
first_seen = "2026-08-01"
signature = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
first_run_url = ""
last_run_url = ""
mechanism = "scheduler-window"
reproduction = "cargo nextest run"
root_cause = ""
fix_pr = ""
proving_test = ""
soak_evidence = []
status = "open"
'''


class CheckSoakParityTests(unittest.TestCase):
    def test_rejects_open_test_absent_from_soak_filters(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ledger = root / "flake-ledger.toml"
            workflow = root / "ci-soak.yml"
            ledger.write_text(LEDGER)
            workflow.write_text("run: cargo nextest run -E 'test(other_test)'\n")

            errors = check_soak_parity.validate_parity(ledger, workflow)

        self.assertTrue(any("known_flake" in error for error in errors))

    def test_accepts_open_test_present_in_soak_filter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ledger = root / "flake-ledger.toml"
            workflow = root / "ci-soak.yml"
            ledger.write_text(LEDGER)
            workflow.write_text("run: cargo nextest run -E 'test(known_flake)'\n")

            self.assertEqual(check_soak_parity.validate_parity(ledger, workflow), [])

    def test_ignores_obsolete_incident(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ledger = root / "flake-ledger.toml"
            workflow = root / "ci-soak.yml"
            ledger.write_text(LEDGER.replace('status = "open"', 'status = "obsolete"'))
            workflow.write_text("run: cargo nextest run -E 'test(other_test)'\n")

            self.assertEqual(check_soak_parity.validate_parity(ledger, workflow), [])

    def test_test_list_rejects_missing_live_test(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ledger = root / "flake-ledger.toml"
            workflow = root / "ci-soak.yml"
            test_list = root / "test-list.txt"
            ledger.write_text(LEDGER)
            workflow.write_text("run: cargo nextest run -E 'test(known_flake)'\n")
            test_list.write_text("other::tests::unrelated\n")

            errors = check_soak_parity.validate_parity(ledger, workflow, test_list)

        self.assertTrue(any("test no longer exists" in error for error in errors))

    def test_test_list_accepts_obsolete_missing_test(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            ledger = root / "flake-ledger.toml"
            workflow = root / "ci-soak.yml"
            test_list = root / "test-list.txt"
            ledger.write_text(LEDGER.replace('status = "open"', 'status = "obsolete"'))
            workflow.write_text("run: cargo nextest run -E 'test(other_test)'\n")
            test_list.write_text("other::tests::unrelated\n")

            self.assertEqual(check_soak_parity.validate_parity(ledger, workflow, test_list), [])


if __name__ == "__main__":
    unittest.main()

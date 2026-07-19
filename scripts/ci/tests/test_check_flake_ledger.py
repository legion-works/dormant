import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import check_flake_ledger


OPEN_INCIDENT = '''
[[incident]]
id = "FLAKE-0001"
test = "module::tests::known_flake"
signature = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
first_run_url = ""
last_run_url = ""
mechanism = "scheduler-window"
reproduction = "cargo nextest run -E 'test(known_flake)'"
root_cause = ""
fix_pr = ""
proving_test = ""
soak_evidence = []
status = "open"
'''


FIXED_EVIDENCE = '''
first_run_url = "https://github.com/legion-works/dormant/actions/runs/123"
last_run_url = "https://github.com/legion-works/dormant/actions/runs/456"
root_cause = "the scheduler interleaved both writes"
fix_pr = "https://github.com/legion-works/dormant/pull/789"
proving_test = "module::tests::known_flake"
soak_evidence = ["https://github.com/legion-works/dormant/actions/runs/999"]
status = "fixed"
'''


class CheckFlakeLedgerTests(unittest.TestCase):
    def validate(self, contents: str) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "flake-ledger.toml"
            path.write_text(contents)
            return check_flake_ledger.validate_ledger(path)

    def test_open_incident_permits_empty_evidence(self):
        errors = self.validate("schema_version = 1\n" + OPEN_INCIDENT)

        self.assertEqual(errors, [])

    def test_fixed_incident_rejects_missing_evidence(self):
        errors = self.validate(
            "schema_version = 1\n" + OPEN_INCIDENT.replace('status = "open"', 'status = "fixed"')
        )

        self.assertTrue(any("FLAKE-0001" in error and "root_cause" in error for error in errors))
        self.assertTrue(any("FLAKE-0001" in error and "soak_evidence" in error for error in errors))

    def test_fixed_incident_requires_validated_evidence_urls(self):
        fixed = OPEN_INCIDENT.replace(
            'first_run_url = ""\nlast_run_url = ""\nroot_cause = ""\nfix_pr = ""\nproving_test = ""\nsoak_evidence = []\nstatus = "open"',
            FIXED_EVIDENCE,
        )

        self.assertEqual(self.validate("schema_version = 1\n" + fixed), [])

    def test_rejects_non_github_or_non_https_evidence_url(self):
        invalid = OPEN_INCIDENT.replace(
            'first_run_url = ""',
            'first_run_url = "http://github.com/legion-works/dormant/actions/runs/123"',
        )

        errors = self.validate("schema_version = 1\n" + invalid)

        self.assertTrue(any("FLAKE-0001" in error and "first_run_url" in error for error in errors))

    def test_rejects_missing_required_incident_key(self):
        errors = self.validate("schema_version = 1\n" + OPEN_INCIDENT.replace('mechanism = "scheduler-window"\n', ""))

        self.assertTrue(any("FLAKE-0001" in error and "mechanism" in error for error in errors))

    def test_rejects_duplicate_ids_and_test_names(self):
        duplicate = OPEN_INCIDENT.replace("FLAKE-0001", "FLAKE-0002").replace(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )

        errors = self.validate("schema_version = 1\n" + OPEN_INCIDENT + duplicate)

        self.assertTrue(any("FLAKE-0002" in error and "test" in error for error in errors))

    def test_rejects_bad_schema_and_signature(self):
        errors = self.validate(
            "schema_version = 2\n"
            + OPEN_INCIDENT.replace(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "sha256:not-a-real-signature",
            )
        )

        self.assertTrue(any("schema_version" in error for error in errors))
        self.assertTrue(any("FLAKE-0001" in error and "signature" in error for error in errors))


if __name__ == "__main__":
    unittest.main()

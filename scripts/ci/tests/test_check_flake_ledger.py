import pathlib
import sys
import tempfile
import unittest
from datetime import date

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import check_flake_ledger


OPEN_INCIDENT = '''
[[incident]]
id = "FLAKE-0001"
test = "module::tests::known_flake"
first_seen = "2026-08-01"
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
    def validate(self, contents: str, today: str = "2026-08-09") -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "flake-ledger.toml"
            path.write_text(contents)
            return check_flake_ledger.validate_ledger(path, today=date.fromisoformat(today))

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

    def test_rejects_invalid_first_seen_date(self):
        errors = self.validate("schema_version = 1\n" + OPEN_INCIDENT.replace('first_seen = "2026-08-01"', 'first_seen = "2026-13-45"'))

        self.assertTrue(any("FLAKE-0001" in error and "first_seen" in error for error in errors))

    def test_rejects_missing_first_seen(self):
        errors = self.validate("schema_version = 1\n" + OPEN_INCIDENT.replace('first_seen = "2026-08-01"\n', ""))

        self.assertTrue(any("FLAKE-0001" in error and "first_seen" in error for error in errors))

    def test_rejects_past_triage_deferral(self):
        deferred = OPEN_INCIDENT.replace('status = "open"', 'triage_deferred_until = "2026-08-08"\ntriage_deferred_reason = "waiting on upstream"\nstatus = "open"')

        errors = self.validate("schema_version = 1\n" + deferred)

        self.assertTrue(any("triage_deferred_until" in error and "past" in error for error in errors))

    def test_rejects_triage_deferral_beyond_cap(self):
        deferred = OPEN_INCIDENT.replace('status = "open"', 'triage_deferred_until = "2026-11-08"\ntriage_deferred_reason = "waiting on upstream"\nstatus = "open"')

        errors = self.validate("schema_version = 1\n" + deferred)

        self.assertTrue(any("90 days" in error for error in errors))

    def test_rejects_partial_triage_deferral(self):
        deferred = OPEN_INCIDENT.replace('status = "open"', 'triage_deferred_until = "2026-08-15"\nstatus = "open"')

        errors = self.validate("schema_version = 1\n" + deferred)

        self.assertTrue(any("both" in error and "triage" in error for error in errors))

    def test_rejects_old_open_incident_without_diagnosis(self):
        old = OPEN_INCIDENT.replace('first_seen = "2026-08-01"', 'first_seen = "2026-07-01"')
        errors = self.validate("schema_version = 1\n" + old)

        self.assertTrue(any("30 days" in error and "root_cause" in error for error in errors))

    def test_accepts_old_open_incident_with_diagnosis(self):
        diagnosed = OPEN_INCIDENT.replace('root_cause = ""', 'root_cause = "scheduler contention"')

        self.assertEqual(self.validate("schema_version = 1\n" + diagnosed), [])

    def test_rejects_soak_pull_request_url(self):
        invalid = OPEN_INCIDENT.replace('soak_evidence = []', 'soak_evidence = ["https://github.com/legion-works/dormant/pull/261"]')

        errors = self.validate("schema_version = 1\n" + invalid)

        self.assertTrue(any("soak_evidence[1]" in error for error in errors))

    def test_obsolete_incident_requires_root_cause(self):
        obsolete = OPEN_INCIDENT.replace('root_cause = ""', 'root_cause = ""').replace('status = "open"', 'status = "obsolete"')

        errors = self.validate("schema_version = 1\n" + obsolete)

        self.assertTrue(any("obsolete" in error and "root_cause" in error for error in errors))


if __name__ == "__main__":
    unittest.main()

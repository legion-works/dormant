import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import classify_nextest_junit


class ClassifyNextestJunitTests(unittest.TestCase):
    def classify(self, xml: str):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "junit.xml"
            path.write_text(xml)
            return classify_nextest_junit.classify_junit(path)

    def test_clean_testcase_emits_no_record(self):
        records = self.classify(
            """<testsuites><testsuite name="suite"><testcase name="always_passes" classname="fixture::lib" /></testsuite></testsuites>"""
        )

        self.assertEqual(records, [])

    def test_permanent_failure_counts_reruns(self):
        records = self.classify(
            """<testsuites><testsuite name="suite"><testcase name="always_fails" classname="fixture::lib">
                <failure message="thread 'always_fails' panicked at src/lib.rs:20:5" type="test failure with exit code 101">detail</failure>
                <rerunFailure message="thread 'always_fails' panicked at src/lib.rs:20:5" type="test failure with exit code 101">detail</rerunFailure>
                <rerunFailure message="thread 'always_fails' panicked at src/lib.rs:20:5" type="test failure with exit code 101">detail</rerunFailure>
            </testcase></testsuite></testsuites>"""
        )

        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["classification"], "TEST-FAILURE")
        self.assertEqual(records[0]["test_name"], "fixture::lib::always_fails")
        self.assertEqual(records[0]["attempt_count"], 3)

    def test_flaky_failure_uses_observed_retry_shape(self):
        records = self.classify(
            """<testsuites><testsuite name="suite"><testcase name="fail_once" classname="fixture::lib">
                <failure message="test passed on attempt 2/3 but is configured to fail when flaky" type="flaky failure" />
                <flakyFailure timestamp="2026-07-18T00:00:00Z" time="0.013s" message="thread 'fail_once' panicked at src/lib.rs:13:9" type="test failure with exit code 101">detail</flakyFailure>
            </testcase></testsuite></testsuites>"""
        )

        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["classification"], "FLAKE-OBSERVED")
        self.assertEqual(records[0]["test_name"], "fixture::lib::fail_once")
        self.assertEqual(records[0]["attempt_count"], 2)

    def test_signatures_normalize_volatile_runner_details(self):
        first = self.classify(
            """<testsuites><testsuite><testcase name="fails" classname="fixture">
                <failure message="runner at /tmp/nextest-a/bin/test took 12.4ms at 2026-07-18T12:00:00Z address 0x7fff1234" type="test failure" />
            </testcase></testsuite></testsuites>"""
        )[0]
        second = self.classify(
            """<testsuites><testsuite><testcase name="fails" classname="fixture">
                <failure message="runner at /tmp/nextest-b/bin/test took 4.7s at 2027-01-01T00:00:00Z address 0xdeadbeef" type="test failure" />
            </testcase></testsuite></testsuites>"""
        )[0]

        self.assertEqual(first["signature"], second["signature"])

    def test_signatures_normalize_rust_thread_ids(self):
        first = classify_nextest_junit.normalize_signature(
            "thread 'tests::always_fails' (12345) panicked at src/lib.rs:20:5"
        )
        second = classify_nextest_junit.normalize_signature(
            "thread 'tests::always_fails' (67890) panicked at src/lib.rs:20:5"
        )

        self.assertEqual(first, second)

    def test_error_only_testcase_is_an_infrastructure_candidate(self):
        records = self.classify(
            """<testsuites><testsuite><testcase name="setup" classname="fixture">
                <error message="worker exited unexpectedly" type="some-other-error" />
            </testcase></testsuite></testsuites>"""
        )

        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["classification"], "INFRA-CANDIDATE")
        self.assertEqual(records[0]["test_name"], "fixture::setup")

    def test_malformed_xml_is_an_infrastructure_candidate(self):
        records = self.classify("<testsuites><testcase")

        self.assertEqual(records[0]["classification"], "INFRA-CANDIDATE")
        self.assertIn("malformed JUnit XML", records[0]["failure_excerpt"])

    def test_missing_junit_is_an_infrastructure_candidate(self):
        records = classify_nextest_junit.classify_junit(pathlib.Path("does-not-exist.xml"))

        self.assertEqual(records[0]["classification"], "INFRA-CANDIDATE")
        self.assertIn("missing JUnit XML", records[0]["failure_excerpt"])


if __name__ == "__main__":
    unittest.main()

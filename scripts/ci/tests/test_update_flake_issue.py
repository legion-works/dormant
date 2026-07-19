import pathlib
import sys
import unittest


sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import update_flake_issue


class FakeIssueClient:
    def __init__(self, issues=()):
        self.issues = list(issues)
        self.created = []
        self.comments = []
        self.reopened = []

    def search_issues(self, marker):
        return [issue for issue in self.issues if marker in issue["body"]]

    def create_issue(self, title, body):
        issue = {"number": 99, "state": "open", "body": body, "title": title}
        self.created.append(issue)
        self.issues.append(issue)
        return issue

    def comment_issue(self, number, body):
        self.comments.append((number, body))

    def reopen_issue(self, number):
        self.reopened.append(number)


class UpdateFlakeIssueTests(unittest.TestCase):
    def event(self, **overrides):
        event = {
            "test_name": "daemon_smoke::reloads_config",
            "classification": "TEST-FAILURE",
            "attempt_count": 1,
            "failure_excerpt": "thread panicked at /tmp/test.rs:20:5",
        }
        event.update(overrides)
        return event

    def context(self):
        return update_flake_issue.RunContext(
            run_url="https://github.com/example/dormant/actions/runs/42",
            os_image="ubuntu-24.04",
            commit_sha="0123456789abcdef",
        )

    def test_dedupe_key_includes_test_name_and_normalized_signature(self):
        first = update_flake_issue.dedupe_key(
            "suite::first", "timed out after 1.2s at 2026-07-18T01:02:03Z"
        )
        same_mechanism = update_flake_issue.dedupe_key(
            "suite::first", "timed out after 8.4s at 2027-01-01T01:02:03Z"
        )
        different_test = update_flake_issue.dedupe_key(
            "suite::second", "timed out after 8.4s at 2027-01-01T01:02:03Z"
        )

        self.assertEqual(first, same_mechanism)
        self.assertNotEqual(first, different_test)

    def test_absent_key_creates_issue(self):
        client = FakeIssueClient()

        action = update_flake_issue.update_issue(client, self.event(), self.context())

        self.assertEqual(action, "created")
        self.assertEqual(len(client.created), 1)
        self.assertIn("dormant-flake-key:", client.created[0]["body"])
        self.assertIn(self.context().run_url, client.created[0]["body"])

    def test_present_key_appends_comment(self):
        key = update_flake_issue.dedupe_key(
            self.event()["test_name"], self.event()["failure_excerpt"]
        )
        client = FakeIssueClient(
            [{"number": 7, "state": "open", "body": f"<!-- dormant-flake-key: {key} -->"}]
        )

        action = update_flake_issue.update_issue(client, self.event(), self.context())

        self.assertEqual(action, "commented")
        self.assertEqual(client.created, [])
        self.assertEqual(len(client.comments), 1)

    def test_closed_issue_reopens_on_recurrence(self):
        key = update_flake_issue.dedupe_key(
            self.event()["test_name"], self.event()["failure_excerpt"]
        )
        client = FakeIssueClient(
            [{"number": 8, "state": "closed", "body": f"<!-- dormant-flake-key: {key} -->"}]
        )

        action = update_flake_issue.update_issue(client, self.event(), self.context())

        self.assertEqual(action, "reopened")
        self.assertEqual(client.reopened, [8])
        self.assertEqual(len(client.comments), 1)

    def test_never_auto_closes_after_a_green_night(self):
        client = FakeIssueClient()

        actions = update_flake_issue.update_issues(client, [], self.context())

        self.assertEqual(actions, [])
        self.assertFalse(hasattr(client, "close_issue"))

    def test_failure_excerpt_is_capped_at_four_kibibytes(self):
        client = FakeIssueClient()

        update_flake_issue.update_issue(
            client, self.event(failure_excerpt="x" * 5000), self.context()
        )

        self.assertIn("x" * 4096, client.created[0]["body"])
        self.assertNotIn("x" * 4097, client.created[0]["body"])

    def test_failure_excerpt_sanitizes_control_characters(self):
        client = FakeIssueClient()

        update_flake_issue.update_issue(
            client, self.event(failure_excerpt="bad\x00output\x1b[31m"), self.context()
        )

        body = client.created[0]["body"]
        self.assertNotIn("\x00", body)
        self.assertNotIn("\x1b", body)
        self.assertIn("bad?output?[31m", body)


if __name__ == "__main__":
    unittest.main()

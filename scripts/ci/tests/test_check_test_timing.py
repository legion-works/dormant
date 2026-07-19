import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import check_test_timing


class CheckTestTimingTests(unittest.TestCase):
    def test_added_line_parsing_tracks_new_file_line_numbers(self):
        diff = """diff --git a/crates/example/src/lib.rs b/crates/example/src/lib.rs
--- a/crates/example/src/lib.rs
+++ b/crates/example/src/lib.rs
@@ -7,0 +8,2 @@
+    tokio::time::sleep(Duration::from_secs(1)).await;
+    ok();
"""

        added = check_test_timing.parse_added_lines(diff)

        self.assertEqual(
            added,
            [
                check_test_timing.AddedLine(
                    "crates/example/src/lib.rs",
                    8,
                    "    tokio::time::sleep(Duration::from_secs(1)).await;",
                ),
                check_test_timing.AddedLine("crates/example/src/lib.rs", 9, "    ok();"),
            ],
        )

    def test_anchor_is_stable_when_a_call_moves(self):
        original = "tokio::time::sleep(Duration::from_millis(250)).await;"
        moved = "    tokio::time::sleep( Duration::from_millis(250) ).await;"

        self.assertEqual(
            check_test_timing.normalize_call_anchor(original),
            check_test_timing.normalize_call_anchor(moved),
        )

    def test_cfg_test_range_is_checked_but_production_range_is_not(self):
        source = """fn production() {
    tokio::time::sleep(Duration::from_secs(1)).await;
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn waits_for_device() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
"""
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = root / "crates/example/src/lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source)

            findings = check_test_timing.find_sleep_calls(
                root,
                [
                    check_test_timing.AddedLine(path.relative_to(root).as_posix(), 2, source.splitlines()[1]),
                    check_test_timing.AddedLine(path.relative_to(root).as_posix(), 9, source.splitlines()[8]),
                ],
            )

        self.assertEqual([(finding.line, finding.function) for finding in findings], [(9, "waits_for_device")])

    def test_duplicate_function_anchors_are_rejected(self):
        source = """#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn waits_twice() {
        tokio::time::sleep(Duration::from_secs(1)).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
"""
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = root / "crates/example/src/lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source)
            findings = check_test_timing.find_sleep_calls(
                root,
                [
                    check_test_timing.AddedLine(path.relative_to(root).as_posix(), 5, source.splitlines()[4]),
                    check_test_timing.AddedLine(path.relative_to(root).as_posix(), 6, source.splitlines()[5]),
                ],
            )

        failures = check_test_timing.evaluate_findings(findings, [])

        self.assertTrue(any("duplicate anchor" in failure for failure in failures), failures)

    def test_exact_allowlist_record_matches_required_fields(self):
        source = """#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn usb_transport_wait() {
        std::thread::sleep(Duration::from_secs(1));
    }
}
"""
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = root / "crates/example/src/lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source)
            finding = check_test_timing.find_sleep_calls(
                root,
                [check_test_timing.AddedLine(path.relative_to(root).as_posix(), 5, source.splitlines()[4])],
            )[0]
            record = check_test_timing.AllowlistRecord(
                finding.path,
                finding.function,
                finding.anchor,
                "ci",
                "outer timeout for a real device",
                "USB serial device",
                "replace when transport exposes a receipt",
            )

        self.assertEqual(check_test_timing.evaluate_findings([finding], [record]), [])

    def test_failure_output_names_current_line_and_exact_record_shape(self):
        source = """#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn mqtt_ack() {
        sleep(Duration::from_secs(1)).await;
    }
}
"""
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = root / "crates/example/src/lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source)
            finding = check_test_timing.find_sleep_calls(
                root,
                [check_test_timing.AddedLine(path.relative_to(root).as_posix(), 5, source.splitlines()[4])],
            )[0]

        failure = check_test_timing.evaluate_findings([finding], [])[0]

        self.assertIn("crates/example/src/lib.rs:5", failure)
        self.assertIn("mqtt_ack", failure)
        self.assertIn("path\tfunction\tanchor\towner\treason\texternal_resource\treplacement_readiness", failure)


if __name__ == "__main__":
    unittest.main()

import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import select_changed_tests


def metadata_fixture() -> str:
    return json.dumps(
        {
            "packages": [
                {
                    "name": "example",
                    "manifest_path": "/repo/crates/example/Cargo.toml",
                    "targets": [
                        {
                            "name": "example",
                            "kind": ["lib"],
                            "src_path": "/repo/crates/example/src/lib.rs",
                        },
                        {
                            "name": "alpha",
                            "kind": ["test"],
                            "src_path": "/repo/crates/example/tests/alpha.rs",
                        },
                        {
                            "name": "beta",
                            "kind": ["test"],
                            "src_path": "/repo/crates/example/tests/beta.rs",
                        },
                        {
                            "name": "renamed",
                            "kind": ["test"],
                            "src_path": "/repo/crates/example/tests/renamed.rs",
                        },
                    ],
                },
                {
                    "name": "tool",
                    "manifest_path": "/repo/crates/tool/Cargo.toml",
                    "targets": [
                        {
                            "name": "tool",
                            "kind": ["bin"],
                            "src_path": "/repo/crates/tool/src/main.rs",
                        }
                    ],
                },
            ]
        }
    )


def diff(
    path: str,
    old_hunk: str,
    new_hunk: str,
    lines: str = "",
    header: str = "fn checks_value()",
) -> str:
    return f"""diff --git a/{path} b/{path}
--- a/{path}
+++ b/{path}
@@ -{old_hunk} +{new_hunk} @@ {header}
{lines}
"""


class SelectChangedTestsTests(unittest.TestCase):
    def setUp(self):
        self.metadata = select_changed_tests.parse_metadata(metadata_fixture(), pathlib.Path("/repo"))

    def assert_targets(self, change: str, expected: list[tuple[str, str, str]]):
        selected = select_changed_tests.select_targets(change, self.metadata, "linux")
        self.assertEqual(
            [(target.package, target.kind, target.name) for target in selected], expected
        )

    def test_top_level_integration_test_maps_to_its_target(self):
        change = diff(
            "crates/example/tests/alpha.rs",
            "8,0",
            "8,1",
            "+assert!(ready());",
        )

        self.assert_targets(change, [("example", "test", "alpha")])

    def test_cfg_test_range_maps_to_library_target(self):
        change = diff(
            "crates/example/src/lib.rs",
            "12,1",
            "12,1",
            "-    assert_eq!(old(), 1);\n+    assert_eq!(new(), 1);",
            "mod tests",
        )

        self.assert_targets(change, [("example", "lib", "example")])

    def test_existing_test_body_hunk_maps_to_library_target(self):
        change = diff(
            "crates/example/src/lib.rs",
            "18,1",
            "18,1",
            "-    assert_eq!(old(), 1);\n+    assert_eq!(new(), 1);",
            "fn test_existing_behavior()",
        )

        self.assert_targets(change, [("example", "lib", "example")])

    def test_cfg_test_range_maps_to_binary_target_when_no_library_exists(self):
        change = diff(
            "crates/tool/src/main.rs",
            "4,0",
            "4,1",
            "+#[test]",
            "fn cli_works()",
        )

        self.assert_targets(change, [("tool", "bin", "tool")])

    def test_nested_support_module_maps_every_integration_target(self):
        change = diff(
            "crates/example/tests/support/helpers.rs",
            "1,0",
            "1,1",
            "+pub fn helper() {}",
        )

        self.assert_targets(
            change,
            [
                ("example", "test", "alpha"),
                ("example", "test", "beta"),
                ("example", "test", "renamed"),
            ],
        )

    def test_deleted_integration_test_line_maps_its_containing_target(self):
        change = """diff --git a/crates/example/tests/alpha.rs b/crates/example/tests/alpha.rs
--- a/crates/example/tests/alpha.rs
+++ b/crates/example/tests/alpha.rs
@@ -1,1 +0,0 @@ fn removed_case()
-#[test]
"""

        self.assert_targets(change, [("example", "test", "alpha")])

    def test_renamed_integration_test_uses_old_path(self):
        change = """diff --git a/crates/example/tests/alpha.rs b/crates/example/tests/renamed.rs
similarity index 90%
rename from crates/example/tests/alpha.rs
rename to crates/example/tests/renamed.rs
--- a/crates/example/tests/alpha.rs
+++ b/crates/example/tests/renamed.rs
@@ -3,1 +3,1 @@ fn checks_value()
-assert!(old());
+assert!(new());
"""

        self.assert_targets(change, [("example", "test", "renamed")])

    def test_macos_vendor_test_change_selects_standalone_manifest(self):
        change = diff(
            "vendor/ddc-macos/src/arm.rs",
            "490,1",
            "490,1",
            "-#[test]\n+#[test]",
            "fn raw_packet()",
        )

        selected = select_changed_tests.select_targets(change, self.metadata, "macos")

        self.assertEqual([(target.package, target.kind, target.name) for target in selected], [("ddc-macos", "lib", "ddc-macos")])
        self.assertEqual(selected[0].manifest_path, "vendor/ddc-macos/Cargo.toml")

    def test_linux_skips_macos_only_vendor_target(self):
        change = diff(
            "vendor/ddc-macos/src/arm.rs",
            "490,1",
            "490,1",
            "+#[test]",
            "fn raw_packet()",
        )

        self.assertEqual(select_changed_tests.select_targets(change, self.metadata, "linux"), [])

    def test_policy_fixture_is_explicitly_skipped(self):
        change = diff(
            ".github/fixtures/nextest-policy/flaky/src/lib.rs",
            "1,0",
            "1,1",
            "+#[test]",
        )

        self.assertEqual(select_changed_tests.select_targets(change, self.metadata, "linux"), [])

    def test_no_change_selects_nothing(self):
        self.assertEqual(select_changed_tests.select_targets("", self.metadata, "linux"), [])

    def test_unmappable_test_path_fails_loudly(self):
        change = diff("examples/tests/forgotten.rs", "1,0", "1,1", "+#[test]")

        with self.assertRaisesRegex(select_changed_tests.SelectionError, "examples/tests/forgotten.rs.*unmappable"):
            select_changed_tests.select_targets(change, self.metadata, "linux")


if __name__ == "__main__":
    unittest.main()

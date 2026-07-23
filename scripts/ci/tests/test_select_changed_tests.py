import json
import pathlib
import subprocess
import sys
import tempfile
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
                {
                    "name": "hybrid",
                    "manifest_path": "/repo/crates/hybrid/Cargo.toml",
                    "targets": [
                        {
                            "name": "hybrid",
                            "kind": ["lib"],
                            "src_path": "/repo/crates/hybrid/src/lib.rs",
                        },
                        {
                            "name": "hybrid",
                            "kind": ["bin"],
                            "src_path": "/repo/crates/hybrid/src/main.rs",
                        },
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

    def test_ordinary_named_unit_test_body_uses_enclosing_source(self):
        change = diff(
            "crates/example/src/lib.rs",
            "18,1",
            "18,1",
            "-    assert_eq!(old(), 1);\n+    assert_eq!(new(), 1);",
            "fn resolve_socket_path_from_config()",
        )
        source = """#[cfg(test)]
mod tests {
    #[test]
    fn resolve_socket_path_from_config() {
        assert_eq!(new(), 1);
    }
}
"""

        selected = select_changed_tests.select_targets(
            change,
            self.metadata,
            "linux",
            {"crates/example/src/lib.rs": (source, source)},
        )

        self.assertEqual([(target.package, target.kind, target.name) for target in selected], [("example", "lib", "example")])

    def test_bin_owned_nested_module_selects_binary_target(self):
        change = diff(
            "crates/hybrid/src/cmd_doctor.rs",
            "18,1",
            "18,1",
            "-    assert_eq!(old(), 1);\n+    assert_eq!(new(), 1);",
            "fn checks_status()",
        )
        sources = {
            "crates/hybrid/src/main.rs": ("mod cmd_doctor;", "mod cmd_doctor;"),
            "crates/hybrid/src/lib.rs": ("pub fn library() {}", "pub fn library() {}"),
            "crates/hybrid/src/cmd_doctor.rs": ("#[cfg(test)] mod tests {}", "#[cfg(test)] mod tests {}"),
        }

        selected = select_changed_tests.select_targets(change, self.metadata, "linux", sources)

        self.assertEqual([(target.package, target.kind, target.name) for target in selected], [("hybrid", "bin", "hybrid")])

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

    def test_linux_only_integration_target_is_skipped_on_macos(self):
        change = diff(
            "crates/example/tests/alpha.rs",
            "8,1",
            "8,1",
            "-assert!(old());\n+assert!(new());",
            "fn checks_value()",
        )
        source = "#![cfg(target_os = \"linux\")]\n#[test]\nfn checks_value() {}\n"

        selected = select_changed_tests.select_targets(
            change,
            self.metadata,
            "macos",
            {"crates/example/tests/alpha.rs": (source, source)},
        )

        self.assertEqual(selected, [])

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

    def test_lib_owned_nested_module_selects_library_target(self):
        # A changed module reached only from the library root (`pub mod menu`)
        # maps to --lib, not --bin, when both roots are available to the
        # ownership walk. This is the dormant-tray shape: menu.rs is `pub mod
        # menu` in lib.rs and unreached by main.rs (which `use`s the crate).
        change = diff(
            "crates/hybrid/src/menu.rs",
            "18,1",
            "18,1",
            "-    assert_eq!(old(), 1);\n+    assert_eq!(new(), 1);",
            "fn checks_menu()",
        )
        sources = {
            "crates/hybrid/src/main.rs": ("fn main() {}", "fn main() {}"),
            "crates/hybrid/src/lib.rs": ("pub mod menu;", "pub mod menu;"),
            "crates/hybrid/src/menu.rs": ("#[cfg(test)] mod tests {}", "#[cfg(test)] mod tests {}"),
        }

        selected = select_changed_tests.select_targets(change, self.metadata, "linux", sources)

        self.assertEqual([(target.package, target.kind, target.name) for target in selected], [("hybrid", "lib", "hybrid")])

    def test_nextest_commands_for_manifest_path_target_uses_repo_config(self):
        target = select_changed_tests.Target("ddc-macos", "lib", "ddc-macos", "vendor/ddc-macos/Cargo.toml")

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / ".config").mkdir()
            (root / ".config" / "nextest.toml").write_text("[profile.ci]\n")
            list_cmd, run_cmd, env = select_changed_tests.nextest_commands(target, 3, root)

        self.assertIn("--config-file", list_cmd)
        self.assertEqual(env.get("CARGO_TARGET_DIR"), str(root / "target"))
        self.assertIn("--config-file", run_cmd)
        self.assertIn("--stress-count", run_cmd)

    def test_nextest_commands_for_workspace_target_has_no_extra_config(self):
        target = select_changed_tests.Target("dormant-core", "lib", "dormant-core")

        list_cmd, run_cmd, env = select_changed_tests.nextest_commands(target, 5, pathlib.Path("/repo"))

        self.assertNotIn("--config-file", list_cmd)
        self.assertEqual(env, {})

    def test_load_source_files_loads_crate_roots_for_ownership(self):
        # Regression: a changed non-root file whose crate roots (lib.rs/main.rs)
        # are NOT in the diff must still resolve to its owning target. Before the
        # fix, load_source_files only loaded changed files, so the ownership walk
        # had no roots and fell back to every candidate (selecting a zero-test
        # --bin alongside the --lib that owns the tests).
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.email", "t@t"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.name", "t"], cwd=root, check=True)
            src = root / "crates/hybrid/src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("pub mod menu;\n")
            (src / "main.rs").write_text("fn main() {}\n")
            (src / "menu.rs").write_text("pub fn m() {}\n")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "base"], cwd=root, check=True)
            base = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, text=True, capture_output=True, check=True).stdout.strip()
            (src / "menu.rs").write_text("pub fn m() {}\n#[cfg(test)] mod tests { #[test] fn t() {} }\n")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "head"], cwd=root, check=True)
            head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, text=True, capture_output=True, check=True).stdout.strip()
            diff = subprocess.run(["git", "diff", "--unified=0", f"{base}..{head}"], cwd=root, text=True, capture_output=True, check=True).stdout
            metadata_json = json.dumps({"packages": [{
                "name": "hybrid",
                "manifest_path": str(root / "crates/hybrid/Cargo.toml"),
                "targets": [
                    {"name": "hybrid", "kind": ["lib"], "src_path": str(src / "lib.rs")},
                    {"name": "hybrid", "kind": ["bin"], "src_path": str(src / "main.rs")},
                ],
            }]})
            metadata = select_changed_tests.parse_metadata(metadata_json, root)

            sources = select_changed_tests.load_source_files(root, base, head, diff, metadata)

            self.assertIn("crates/hybrid/src/lib.rs", sources)
            self.assertIn("crates/hybrid/src/main.rs", sources)
            self.assertIn("crates/hybrid/src/menu.rs", sources)
            selected = select_changed_tests.select_targets(diff, metadata, "linux", sources)
            self.assertEqual([(t.package, t.kind, t.name) for t in selected], [("hybrid", "lib", "hybrid")])

    def test_junit_destination_includes_target_kind(self):
        root = pathlib.Path("/repo")

        library = select_changed_tests.junit_destination(root, select_changed_tests.Target("dormantctl", "lib", "dormantctl"))
        binary = select_changed_tests.junit_destination(root, select_changed_tests.Target("dormantctl", "bin", "dormantctl"))

        self.assertEqual(library, root / "target/nextest/changed/dormantctl-lib-dormantctl.xml")
        self.assertEqual(binary, root / "target/nextest/changed/dormantctl-bin-dormantctl.xml")


if __name__ == "__main__":
    unittest.main()
